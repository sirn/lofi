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
            let max_scroll = view.lines.len().saturating_sub(view.view_h.max(1));
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

    /// Load one subagent transcript as flat read-only lines. Settled rows
    /// read through a fresh snapshot so a just-finished file is consistent;
    /// live rows never reach here (`Enter` on a live row shows a footer).
    fn subagent_transcript_view(&self, row: &SubagentRow) -> SubagentTranscriptView {
        let lines = self
            .load_subagent_transcript(row)
            .unwrap_or_else(|e| vec![format!("failed to load transcript: {e}")]);
        SubagentTranscriptView {
            id: row.id.clone(),
            lines,
            scroll: 0,
            view_h: 0,
        }
    }

    fn load_subagent_transcript(&self, row: &SubagentRow) -> lofi_error::Result<Vec<String>> {
        let Some(file_name) = &row.file else {
            return Ok(vec![
                "no transcript (session persistence is disabled)".to_string()
            ]);
        };
        let Some(sink) = self.session.sink.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(parent) = self.session.cursor.as_ref().and_then(|cursor| {
            cursor
                .path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        }) else {
            return Ok(Vec::new());
        };
        let files = sink.subagent_sessions(&parent)?;
        let Some(file) = files.into_iter().find(|file| {
            file.path()
                .file_name()
                .is_some_and(|name| name == file_name.as_str())
        }) else {
            return Ok(vec!["transcript not found".to_string()]);
        };
        let (cursor, snapshot) = file.open_snapshot()?;
        let visible = replay::visible_index_path(&snapshot.index);
        let offsets: Vec<u64> = visible.iter().map(|&i| snapshot.index[i].offset).collect();
        let events = cursor.events_at(&offsets)?;
        Ok(format_subagent_transcript(&events))
    }
}

/// Flat line rendering of a subagent transcript, built from the visible
/// display events (not the model-context projection): failed and cancelled
/// turns and discarded rounds must stay visible, and the system prompt is
/// noise here. Deliberately plainer than the main transcript: this view is
/// a diagnostic, not a chat surface.
fn format_subagent_transcript(events: &[lofi_types::SessionEvent]) -> Vec<String> {
    use lofi_types::SessionEventKind as Kind;
    let mut lines = Vec::new();
    for event in events {
        match &event.kind {
            Kind::Message(message) => format_subagent_message(message, &mut lines),
            Kind::TurnFailed { error, .. } => {
                lines.push(format!("turn failed: {error}"));
                lines.push(String::new());
            }
            Kind::TurnCancelled { .. } => {
                lines.push("turn cancelled".to_string());
                lines.push(String::new());
            }
            Kind::RoundDiscarded { detail } => {
                lines.push(format!("round discarded: {detail}"));
                lines.push(String::new());
            }
            _ => {}
        }
    }
    lines
}

fn format_subagent_message(message: &lofi_types::Message, lines: &mut Vec<String>) {
    // The system prompt is recorded for replay fidelity; it is noise in a
    // diagnostic view.
    if message.role == lofi_types::Role::System {
        return;
    }
    let role = match message.role {
        lofi_types::Role::User => "user",
        lofi_types::Role::Assistant => "assistant",
        lofi_types::Role::System => "system",
        lofi_types::Role::Tool => "tool",
    };
    let mut header = true;
    for block in &message.blocks {
        match block {
            lofi_types::ContentBlock::Text { text } => {
                for (i, line) in text.lines().enumerate() {
                    if header && i == 0 {
                        lines.push(format!("{role}: {line}"));
                    } else {
                        lines.push(format!("  {line}"));
                    }
                    header = false;
                }
            }
            lofi_types::ContentBlock::ToolUse { name, .. } => {
                lines.push(format!("{role}: [tool] {name}"));
                header = false;
            }
            lofi_types::ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                let marker = if *is_error { "error" } else { "result" };
                for (shown, line) in content.lines().enumerate() {
                    if shown == 5 {
                        lines.push("  ...".to_string());
                        break;
                    }
                    if shown == 0 {
                        lines.push(format!("{role}: [{marker}] {line}"));
                    } else {
                        lines.push(format!("  {line}"));
                    }
                }
                header = false;
            }
            _ => {}
        }
    }
    lines.push(String::new());
}
