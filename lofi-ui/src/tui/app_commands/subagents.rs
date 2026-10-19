use super::*;

use lofi_core::session::replay;

impl App {
    /// Record subagent spend that settled but no turn claimed into the open
    /// session's transcript. Runs before the session is replaced (/new,
    /// /resume) or dropped (quit).
    pub(in crate::tui) fn record_subagent_spend(&mut self) {
        let (Some(subagents), Some(sink)) = (&self.subagents, &self.session.sink) else {
            return;
        };
        let Some((cost, usage)) = subagents.take_unreported() else {
            return;
        };
        if let Err(e) = sink.record_subagent_usage(usage, cost) {
            tracing::warn!(error = %e, "subagent spend record failed");
            return;
        }
        // Match the SubagentUsage event fold: session totals only, child
        // tokens never occupy the parent context window.
        self.cost += cost;
        self.total_in += usage.input_tokens;
        self.total_out += usage.output_tokens;
    }

    /// Reset the registry for a session switch. Spend that settles between
    /// `record_subagent_spend` and this clear is old-session residue: log
    /// and drop it rather than book it into the new session.
    pub(in crate::tui) fn clear_subagents(&mut self) {
        // The registry reset drops the inbox a stored wake points at. Clear
        // the text here too, so no turn starts on the old session's sender.
        self.mailbox_wake = None;
        let Some(subagents) = &self.subagents else {
            return;
        };
        let (cost, usage) = subagents.clear();
        if cost > 0.0 || usage.input_tokens > 0 || usage.output_tokens > 0 {
            tracing::debug!(
                cost,
                "dropping subagent spend settled during session switch"
            );
        }
    }

    /// `/subagent`: open the subagents modal. Lists subagents spawned by the
    /// live session: persisted transcripts (store) merged with in-flight runs
    /// (registry). `Enter` opens a read-only transcript view, `r` reloads.
    /// Without a session sink only the live registry rows show.
    pub(in crate::tui) fn open_subagents_modal(&mut self) {
        let mut modal = SubagentsModalState {
            selected: 0,
            rows: Vec::new(),
            viewing: None,
        };
        self.reload_subagent_rows(&mut modal);
        self.subagents_modal = Some(modal);
    }

    /// Rebuild the row list: store transcripts of this session's children,
    /// then live registry rows without a transcript yet.
    fn reload_subagent_rows(&mut self, modal: &mut SubagentsModalState) {
        let parent = self.session.cursor.as_ref().and_then(|cursor| {
            cursor
                .path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        });
        let mut rows: Vec<SubagentRow> = match (self.session.sink.as_ref(), parent) {
            (Some(sink), Some(parent)) => sink
                .subagent_sessions(&parent)
                .unwrap_or_default()
                .into_iter()
                .map(|file| SubagentRow {
                    id: file.id(),
                    running: false,
                    preview: file.quick_preview().unwrap_or_default(),
                    file: file
                        .path()
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned()),
                })
                .collect(),
            _ => Vec::new(),
        };
        if let Some(registry) = &self.subagents {
            for (id, session, running) in registry.live() {
                match session {
                    Some(file)
                        if rows
                            .iter()
                            .any(|row| row.file.as_deref() == Some(file.as_str())) =>
                    {
                        if let Some(row) = rows
                            .iter_mut()
                            .find(|row| row.file.as_deref() == Some(file.as_str()))
                        {
                            row.running = running;
                            row.id = id;
                        }
                    }
                    _ => rows.push(SubagentRow {
                        id,
                        running,
                        preview: String::new(),
                        file: session,
                    }),
                }
            }
        }
        modal.rows = rows;
        if !modal.rows.is_empty() {
            modal.selected = modal.selected.min(modal.rows.len() - 1);
        }
    }

    /// Keys for the `/subagent` modal. Two levels: the list, and the
    /// transcript view. Always returns true while the modal is open.
    pub(super) fn handle_subagents_key(&mut self, k: &KeyEvent) -> bool {
        // Take the modal out so the body can call `&mut self` helpers without
        // overlapping the modal borrow; put it back (or not, on close).
        let Some(mut modal) = self.subagents_modal.take() else {
            return true;
        };

        // Transcript view.
        if let Some(view) = &mut modal.viewing {
            let max_scroll = view.total.saturating_sub(view.view_h.max(1));
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => modal.viewing = None,
                KeyCode::Up | KeyCode::Char('k') => view.scroll = (view.scroll + 1).min(max_scroll),
                KeyCode::Down | KeyCode::Char('j') => view.scroll = view.scroll.saturating_sub(1),
                KeyCode::Char('g') => view.scroll = max_scroll,
                KeyCode::Char('G') => view.scroll = 0,
                _ => {}
            }
            self.subagents_modal = Some(modal);
            return true;
        }

        // List.
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                // Closed: do not restore the modal.
                return true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                modal.selected = modal.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !modal.rows.is_empty() {
                    modal.selected = (modal.selected + 1).min(modal.rows.len() - 1);
                }
            }
            KeyCode::Char('r') => self.reload_subagent_rows(&mut modal),
            KeyCode::Enter => {
                if let Some(row) = modal.rows.get(modal.selected) {
                    let view = self.subagent_transcript_view(row);
                    modal.viewing = Some(view);
                }
            }
            _ => {}
        }
        self.subagents_modal = Some(modal);
        true
    }

    /// Load one subagent transcript as main-transcript turns. Settled rows
    /// read through a fresh snapshot so a just-finished file is consistent;
    /// live rows reload through the same path once the view is open.
    fn subagent_transcript_view(&self, row: &SubagentRow) -> SubagentTranscriptView {
        let loaded = match &row.file {
            Some(file) => self.load_subagent_turns(file, None),
            None => Err(if row.running {
                // The cursor file appears shortly after the claim.
                "transcript is not available yet".to_string()
            } else {
                "no transcript (session persistence is disabled)".to_string()
            }),
        };
        let (turns, path, parsed_len, load_error) = match loaded {
            Ok((turns, path, parsed_len)) => (turns, Some(path), Some(parsed_len), None),
            Err(message) => (Vec::new(), None, None, Some(message)),
        };
        let signatures = turns.iter().map(turn_signature).collect();
        SubagentTranscriptView {
            id: row.id.clone(),
            file: row.file.clone(),
            path,
            turns,
            load_error,
            heights: Vec::new(),
            signatures,
            prior_heights: Vec::new(),
            parsed_len,
            width: 0,
            total: 0,
            scroll: 0,
            last_reload: std::time::Instant::now(),
            live: row.running,
            reloaded: false,
            view_h: 0,
        }
    }

    /// Refresh the open transcript view while its run is still live: the
    /// file grows as the run works. Throttled so a frame never costs more
    /// than one bounded reload per second. Bottom-anchored scroll stays
    /// pinned to the newest content; a scrolled-up view keeps its place.
    pub(in crate::tui) fn refresh_subagent_view(&mut self) {
        const RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
        let (id, file, cached, was_live, due) = {
            let Some(modal) = self.subagents_modal.as_mut() else {
                return;
            };
            let Some(view) = modal.viewing.as_mut() else {
                return;
            };
            (
                view.id.clone(),
                view.file.clone(),
                view.path.clone(),
                view.live,
                view.last_reload.elapsed() >= RELOAD_INTERVAL,
            )
        };
        let live = self
            .subagents
            .as_ref()
            .is_some_and(|registry| registry.is_running(&id));
        // A live run reloads on the throttle; a just-settled run reloads
        // once more so the closing events are not missed.
        if live && !due {
            // Record the observed liveness before the throttle returns: a
            // run that resumes and settles within one interval still earns
            // the final reload.
            if let Some(view) = self
                .subagents_modal
                .as_mut()
                .and_then(|modal| modal.viewing.as_mut())
            {
                view.live = true;
            }
            return;
        }
        if !live && !was_live {
            return;
        }
        // An unchanged file has nothing new to show; stat it instead of
        // parsing the transcript again.
        if self.skip_unchanged_reload(cached.as_deref(), live) {
            return;
        }
        // A view opened before the run's cursor file existed has no file
        // name yet; pick it up once the registry knows it.
        let file = match file {
            Some(file) => Some(file),
            None => self.subagents.as_ref().and_then(|registry| {
                registry
                    .live()
                    .into_iter()
                    .find(|(run, _, _)| *run == id)
                    .and_then(|(_, session, _)| session)
            }),
        };
        let loaded = match &file {
            Some(file) => self.load_subagent_turns(file, cached.as_deref()),
            None => Err(if live {
                "transcript is not available yet".to_string()
            } else {
                "no transcript (session persistence is disabled)".to_string()
            }),
        };
        let Some(modal) = self.subagents_modal.as_mut() else {
            return;
        };
        let Some(view) = modal.viewing.as_mut() else {
            return;
        };
        view.last_reload = std::time::Instant::now();
        view.live = live;
        if file.is_some() {
            view.file = file;
        }
        match loaded {
            Ok((turns, path, parsed_len)) => apply_subagent_reload(view, turns, path, parsed_len),
            Err(message) => view.load_error = Some(message),
        }
    }

    /// Whether the transcript file did not change since the last load; in
    /// that case the refresh only updates the liveness bookkeeping.
    fn skip_unchanged_reload(&mut self, cached: Option<&std::path::Path>, live: bool) -> bool {
        let Some(path) = cached else {
            return false;
        };
        let len = std::fs::metadata(path).ok().map(|meta| meta.len());
        let Some(view) = self
            .subagents_modal
            .as_mut()
            .and_then(|modal| modal.viewing.as_mut())
        else {
            return false;
        };
        if len.is_none() || len != view.parsed_len {
            return false;
        }
        view.last_reload = std::time::Instant::now();
        view.live = live;
        true
    }

    /// Build the turns with the same replay the main transcript uses, so
    /// the drill-in renders identically to it. The visible display events
    /// (not the model-context projection) feed the replay: failed and
    /// cancelled turns and discarded rounds must stay visible. The events
    /// are already the selected lineage, so the replay must not walk parent
    /// links again: at a compaction the marker's parent is a hidden
    /// checkpoint copy, and a second walk would drop everything before it.
    fn load_subagent_turns(
        &self,
        file_name: &str,
        cached: Option<&std::path::Path>,
    ) -> std::result::Result<(Vec<Turn>, std::path::PathBuf, u64), String> {
        // The name-to-path resolution lists the sessions directory; the
        // view caches the path so a live reload skips the listing.
        let path = if let Some(path) = cached {
            path.to_path_buf()
        } else {
            let (Some(sink), Some(parent)) = (
                self.session.sink.as_ref(),
                self.session.cursor.as_ref().and_then(|cursor| {
                    cursor
                        .path()
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                }),
            ) else {
                return Err("no transcript (session persistence is disabled)".to_string());
            };
            let files = sink
                .subagent_sessions(&parent)
                .map_err(|e| format!("failed to load transcript: {e}"))?;
            files
                .into_iter()
                .find(|file| {
                    file.path()
                        .file_name()
                        .is_some_and(|name| name == file_name)
                })
                .map(|file| file.path().to_path_buf())
                .ok_or_else(|| "transcript not found".to_string())?
        };
        // The bounded display reader: strings are capped and image data is
        // dropped, so opening a large transcript stays cheap. The oldest
        // events get the smallest budget when it runs out.
        let load = || -> lofi_error::Result<(Vec<lofi_types::SessionEvent>, u64)> {
            let (cursor, snapshot) = store::SessionCursor::open_snapshot(path.clone())?;
            let visible = replay::visible_index_path(&snapshot.index);
            let offsets: Vec<u64> = visible.iter().map(|&i| snapshot.index[i].offset).collect();
            let events = cursor.display_events_at(&offsets)?;
            // The parsed watermark, not a fresh stat: bytes appended after
            // the snapshot opened are not in the events, so the refresh
            // must not treat them as shown.
            Ok((events, snapshot.file_size))
        };
        let (mut events, parsed_len) =
            load().map_err(|e| format!("failed to load transcript: {e}"))?;
        // Mirror the frozen main-transcript reduction: a collapsed exec
        // keeps no result body, only the marker that one exists. Bodies
        // would otherwise be retained and re-wrapped on every frame.
        let mut exec_ids = std::collections::HashSet::new();
        let mut available_exec_results = std::collections::HashSet::new();
        for event in &mut events {
            let lofi_types::SessionEventKind::Message(message) = &mut event.kind else {
                continue;
            };
            if message.role == lofi_types::Role::Assistant {
                exec_ids.extend(message.blocks.iter().filter_map(|block| match block {
                    lofi_types::ContentBlock::ToolUse { id, name, .. } if name == "exec" => {
                        Some(id.clone())
                    }
                    _ => None,
                }));
            }
            for block in &mut message.blocks {
                if let lofi_types::ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error: false,
                    ..
                } = block
                {
                    if exec_ids.contains(tool_use_id) {
                        if !content.is_empty() {
                            available_exec_results.insert(tool_use_id.clone());
                        }
                        content.clear();
                        content.shrink_to_fit();
                    }
                }
            }
        }
        let mut turns = crate::tui::replay::turns_from_selected_session_events(&events);
        for (i, turn) in turns.iter_mut().enumerate() {
            // Detail ids from a range no live turn reaches: a main-transcript
            // expansion key must never match a block in this view.
            assign_detail_ids(turn, u32::MAX as usize - i);
            for block in &mut turn.blocks {
                if let Block::Tool(tool) = block {
                    if available_exec_results.contains(&tool.id) {
                        tool.result_availability = ResultAvailability::Available;
                    }
                }
            }
            // The view never expands tools, so keep only the collapsed
            // preview and drop the retained native result body.
            view::blocks::compact_native_previews(turn);
        }
        Ok((turns, path, parsed_len))
    }
}

/// Replace the view's turns with a fresh load, keeping the measured
/// heights of the unchanged turns. The display reader's shared content
/// budget can re-truncate an older turn as the transcript grows, so a
/// prefix rule is not enough; the renderer measures only the holes.
fn apply_subagent_reload(
    view: &mut SubagentTranscriptView,
    turns: Vec<Turn>,
    path: std::path::PathBuf,
    parsed_len: u64,
) {
    let new_signatures: Vec<u64> = turns.iter().map(turn_signature).collect();
    view.prior_heights =
        if view.heights.len() == view.turns.len() && view.heights.iter().all(Option::is_some) {
            view.heights.iter().flatten().copied().collect()
        } else {
            Vec::new()
        };
    let mut heights: Vec<Option<usize>> = Vec::with_capacity(turns.len());
    for (i, signature) in new_signatures.iter().enumerate() {
        let reuse = view.signatures.get(i) == Some(signature) && i < view.heights.len();
        heights.push(if reuse { view.heights[i] } else { None });
    }
    view.parsed_len = Some(parsed_len);
    view.signatures = new_signatures;
    view.heights = heights;
    view.turns = turns;
    view.path = Some(path);
    view.load_error = None;
    // Tell the renderer to keep a scrolled-up view on the same content.
    view.reloaded = true;
}

/// Cheap change detection for a turn between reloads. The block tree has
/// no `PartialEq`; the serialized form covers every field that feeds the
/// layout, so an unchanged signature keeps the measured height.
fn turn_signature(turn: &Turn) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(turn)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}
