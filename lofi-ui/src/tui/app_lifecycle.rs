#![allow(clippy::wildcard_imports)]

use super::*;

impl App {
    pub(super) fn new(
        model_label: String,
        thinking: ThinkingLevel,
        ctx_limit: u64,
        compaction: lofi_types::CompactionConfig,
    ) -> Self {
        let thinking_label =
            (thinking != ThinkingLevel::Off).then(|| format!(":{}", thinking.as_str()));
        Self {
            turns: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            history: Arc::new(Mutex::new(Vec::new())),
            history_nav: Vec::new(),
            history_idx: None,
            input_stash: String::new(),
            model_label,
            thinking_label,
            thinking,
            status_usage: None,
            ctx_limit: if ctx_limit > 0 {
                ctx_limit
            } else {
                DEFAULT_CTX_LIMIT
            },
            compaction,
            prev_ctx_tokens: None,
            last_compact_msg_count: 0,
            compacted: false,
            context_pressure: false,
            cost: 0.0,
            turn_cost: 0.0,
            turn_has_round_usage: false,
            branch_hint: None,
            total_in: 0,
            total_out: 0,
            total_cache_read: 0,
            total_cache_write: 0,
            prompt_queue: Vec::new(),
            run: None,
            run_start: None,
            retry: None,
            pinned: true,
            top_line: 0,
            last_base: 0,
            verbose: false,
            debug_after_draw: None,
            debug: None,
            should_quit: false,
            session: SessionState {
                store: None,
                path: None,
                cwd: PathBuf::new(),
            },
            picker: None,
            tree_picker: None,
            model_picker: None,
            thinking_picker: None,
            model_choices: Vec::new(),
            pending_model_switch: None,
            info: None,
            slash_complete: None,
            no_models_hint: None,
            theme: Theme::default(),
            kill_ring: String::new(),
            last_kill_was_kill: false,
            ctrl_c_at: None,
            log_rect: Rect::default(),
            input_rect: Rect::default(),
            log_vis: Vec::new(),
            log_off: 0,
            input_scroll: 0,
            sel: None,
            mode: Mode::Input,
            pending_confirms: Vec::new(),
            confirm_selected: 0,
            confirm_scroll: 0,
            confirm_total: 0,
            confirm_view_h: 0,
            yank_notify: None,
            yank_cursor: None,
            notify: None,
            nav_cursor: 0,
            nav_col: 0,
            select_anchor: (0, 0),
            log_total: 0,
            last_turn_height: 0,
            log_view_h: 0,
            frozen_render: FrozenCache::new(),
            frozen_heights: Vec::new(),
            frozen_heights_other_mode: Vec::new(),
            turn_byte_ranges: Vec::new(),
            render_epoch: 0,
            frozen_epoch: 0,
            frozen_width: 0,
        }
    }

    /// Label written into the transcript header (model + resolved level).
    pub(super) fn session_model(&self) -> String {
        format!(
            "{}{}",
            self.model_label,
            self.thinking_label.as_deref().unwrap_or("")
        )
    }

    /// Set the explicit branch point for the next run. A UI gesture (e.g.
    /// resuming from a selected entry in the tree picker) calls this with the
    /// target entry's id; the next run branches off that id as a sibling of
    /// its existing children instead of appending to the active leaf. The
    /// hint is consumed by the run launcher, so a single gesture applies to a
    /// single turn and a subsequent run without a gesture continues
    /// linearly.
    pub(super) fn branch_from(&mut self, id: String) {
        self.branch_hint = Some(id);
    }

    /// Apply one event while rebuilding a file-backed transcript. At each
    /// turn boundary the completed turn is immediately reduced to its prompt;
    /// its blocks can be reconstructed later from `turn_byte_ranges`. This
    /// keeps resume peak memory bounded to one visible turn instead of the
    /// entire transcript.
    pub(super) fn apply_file_backed_replay_event(&mut self, ev: AgentEvent) {
        if matches!(ev, AgentEvent::TurnStart { .. }) {
            if let Some(turn) = self.turns.last_mut() {
                turn.blocks.clear();
            }
        }
        self.apply_event(ev);
    }

    /// Fold an [`AgentEvent`] into the current turn's blocks / status.
    #[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
    pub(super) fn apply_event(&mut self, ev: AgentEvent) {
        // TurnStart is the turn boundary: push a fresh turn. Unlike the other
        // arms, it does not assume a current turn exists — it creates one.
        if let AgentEvent::TurnStart { prompt } = ev {
            self.push_turn(Turn {
                prompt,
                blocks: Vec::new(),
            });
            // Reset the per-turn accumulators: the live stream feeds these
            // via `RoundUsage` events, and `TurnEnd` folds them once.
            self.turn_cost = 0.0;
            self.turn_has_round_usage = false;
            return;
        }
        // A silent continuation: the run was force-stopped at the hard cap,
        // compacted, and is resuming on the compacted history. Do NOT push a
        // new turn (no "You:" line) — blocks append to the current turn.
        // Reset the per-turn accumulators for the continuation's rounds.
        if let AgentEvent::TurnContinue = ev {
            self.turn_cost = 0.0;
            self.turn_has_round_usage = false;
            return;
        }
        // Status-only events the turn builder doesn't own.
        match ev {
            AgentEvent::RetryStart {
                attempt,
                max_attempts,
                delay_ms,
                error,
            } => {
                self.retry = Some(RetryState {
                    attempt,
                    max_attempts,
                    deadline: Instant::now() + Duration::from_millis(delay_ms),
                    error,
                });
                return;
            }
            AgentEvent::RetryEnd { success, .. } => {
                self.retry = None;
                if !success {
                    // The retry budget was exhausted; the triggering error
                    // surfaces via the subsequent `Error` event from the
                    // engine, so no block is pushed here.
                }
                return;
            }
            AgentEvent::TurnCommitted {
                byte_start,
                byte_end,
            } => {
                // The just-finished turn is now durably in the transcript
                // file over this byte range. Record it so the turn becomes
                // file-backed when the next prompt freezes it.
                if let Some(r) = self.turn_byte_ranges.last_mut() {
                    // A hard-cap compaction silently continues the same
                    // visible turn in a second agent run. Preserve the first
                    // run's committed prefix instead of replacing it with the
                    // continuation-only range; otherwise the next prompt
                    // freezes the turn, clears its in-memory blocks, and can
                    // only reload the continuation suffix (which has no user
                    // turn start), making the transcript appear to vanish.
                    *r = Some(match *r {
                        Some((start, end)) => (start.min(byte_start), end.max(byte_end)),
                        None => (byte_start, byte_end),
                    });
                }
                return;
            }
            AgentEvent::RoundUsage { cost, usage } => {
                // Per-round refresh of the context gauge and cost counter.
                // `cost` is the turn's cumulative cost so far; track it in
                // `turn_cost` (folded into `cost` at `TurnEnd`) so the
                // footer can show a live running total. Tokens accumulate
                // directly into the session totals; `TurnEnd` skips
                // re-adding them when `turn_has_round_usage` is set.
                self.turn_cost = cost;
                self.turn_has_round_usage = true;
                self.total_in += usage.input_tokens;
                self.total_out += usage.output_tokens;
                self.total_cache_read += usage.cache_read_tokens;
                self.total_cache_write += usage.cache_write_tokens;
                self.status_usage = Some(usage);
                self.compacted = false;
                return;
            }
            AgentEvent::TurnEnd { cost, usage, .. } => {
                // Totals are owned by the App, not the turn builder. On the
                // live path `RoundUsage` already applied this turn's tokens
                // and `turn_cost` holds its cumulative cost; fold `turn_cost`
                // and skip the bundled totals. On the resume path (no
                // `RoundUsage` events) apply the bundled totals as before.
                if self.turn_has_round_usage {
                    self.cost += self.turn_cost;
                } else {
                    self.cost += cost;
                    self.total_in += usage.input_tokens;
                    self.total_out += usage.output_tokens;
                    self.total_cache_read += usage.cache_read_tokens;
                    self.total_cache_write += usage.cache_write_tokens;
                    self.status_usage = Some(usage);
                }
                self.turn_cost = 0.0;
                self.turn_has_round_usage = false;
                // Any TurnEnd with usage data means we have a real context
                // size — clear the "just compacted" indicator.
                self.compacted = false;
            }
            AgentEvent::TurnFailed { cost, usage, .. } => {
                // A failed turn's consumed tokens count honestly. Same
                // fold logic as `TurnEnd`: the live path already applied
                // per-round tokens via `RoundUsage` and `turn_cost` holds
                // the cumulative cost; the resume path applies the bundled
                // totals. `status_usage` updates either way so the gauge
                // reflects the failed turn's last round.
                if self.turn_has_round_usage {
                    self.cost += self.turn_cost;
                    self.status_usage = Some(usage);
                } else {
                    self.cost += cost;
                    self.total_in += usage.input_tokens;
                    self.total_out += usage.output_tokens;
                    self.total_cache_read += usage.cache_read_tokens;
                    self.total_cache_write += usage.cache_write_tokens;
                    self.status_usage = Some(usage);
                }
                self.turn_cost = 0.0;
                self.turn_has_round_usage = false;
            }
            AgentEvent::ContextPressure { cost, usage, .. } => {
                // The run force-stopped at the hard cap. Fold the partial
                // turn's cost/tokens like `TurnEnd`, record the over-cap
                // usage, and arm the flag so the run loop force-compacts +
                // continues on channel close. No block is rendered — the
                // compaction marker (from `compact_now`) is the visible
                // signal, and the continue appends to this turn.
                if self.turn_has_round_usage {
                    self.cost += self.turn_cost;
                } else {
                    self.cost += cost;
                    self.total_in += usage.input_tokens;
                    self.total_out += usage.output_tokens;
                    self.total_cache_read += usage.cache_read_tokens;
                    self.total_cache_write += usage.cache_write_tokens;
                }
                self.status_usage = Some(usage);
                self.turn_cost = 0.0;
                self.turn_has_round_usage = false;
                self.context_pressure = true;
                return;
            }
            AgentEvent::Compaction { .. } => {
                // A compaction marker (live or replayed). The context
                // gauge's last reading reflects the pre-compaction fill;
                // drop it so the gauge shows "c" and waits for the next
                // round's real (smaller) usage.
                self.reset_compaction_gauges();
                self.compacted = true;
            }
            // Remaining status/marker events do not mutate App-owned
            // counters here; the shared turn builder handles them.
            _ => {}
        }
        // Everything else (and the block-building part of `TurnEnd`) goes
        // through the shared turn builder, so live and resume share one path.
        apply_event_to_turns(&mut self.turns, ev);
    }

    pub(super) fn run_finished(&mut self) {
        if let Some(turn) = self.turns.last_mut() {
            finalize_open_thinking(turn);
        }
        // The turn-end marker (model, elapsed, cost, usage) arrives as an
        // `AgentEvent::TurnEnd` emitted by the engine, which also writes it
        // to the transcript — so there is nothing to stamp or persist here.
        self.run_start = None;
        self.run = None;
        self.retry = None;
    }

    /// Reset compaction-related gauge state to the pre-compaction defaults.
    /// Called from every site that invalidates the session context: the
    /// `Compaction` event handler, `compact_now`, `/new`, `/resume`,
    /// and `/rollback`. Centralizing the reset prevents drift when a new
    /// field is added to the gauge cluster.
    pub(super) fn reset_compaction_gauges(&mut self) {
        self.status_usage = None;
        self.prev_ctx_tokens = None;
        self.last_compact_msg_count = 0;
        self.compacted = false;
    }

    /// Derive the kept-tail token budget for `plan_cut` from the compaction
    /// thresholds. Prefers the soft threshold, falls back to the hard
    /// threshold, and uses 50% of the threshold so the kept tail stays well
    /// under the cap. Returns 0 when no threshold is configured (disables the
    /// oversized-turn guard).
    pub(super) fn derive_compact_budget(&self) -> usize {
        let limit = self.ctx_limit.max(DEFAULT_CTX_LIMIT);
        self.compaction
            .soft_threshold(limit)
            .or_else(|| self.compaction.hard_threshold(limit))
            .map_or(0, |t| (t / 2) as usize)
    }

    /// Invalidate the frozen-turn cache. Call whenever `turns` is replaced
    /// wholesale (resume, `/new`, `/clear`); incremental `push` does not need
    /// it — [`ensure_frozen`] freezes the newly-superseded turn on its own.
    pub(super) fn bump_render_epoch(&mut self) {
        self.render_epoch = self.render_epoch.wrapping_add(1);
        // A wholesale transcript/layout invalidation makes both verbose-mode
        // indexes stale. Width-only invalidation is handled by ensure_frozen.
        self.frozen_heights_other_mode.clear();
    }

    /// Switch verbose layout state without throwing away the height index for
    /// the mode we are leaving. Styled viewport rows are mode-specific and
    /// cheap to rebuild, but the tiny all-turn height indexes are retained and
    /// swapped back on the next toggle.
    pub(super) fn switch_verbose_layout(&mut self) {
        self.frozen_render.clear();
        std::mem::swap(
            &mut self.frozen_heights,
            &mut self.frozen_heights_other_mode,
        );
    }

    /// Push a turn, keeping `turn_byte_ranges` parallel to `turns`.
    pub(super) fn push_turn(&mut self, turn: Turn) {
        self.turns.push(turn);
        self.turn_byte_ranges.push(None);
    }

    /// Insert a turn at `idx`, keeping `turn_byte_ranges` parallel.
    pub(super) fn insert_turn(&mut self, idx: usize, turn: Turn) {
        self.turns.insert(idx, turn);
        self.turn_byte_ranges.insert(idx, None);
    }

    /// Reconstruct a frozen turn's blocks. If the turn still holds its blocks
    /// in memory (ephemeral session, or not yet frozen), clone them. Otherwise
    /// re-parse the turn's byte range from the transcript file. On any read or
    /// parse failure the turn's prompt is preserved with empty blocks.
    pub(super) fn materialize_turn(&self, idx: usize) -> Turn {
        if let Some(turn) = self.turns.get(idx) {
            if !turn.blocks.is_empty() {
                return turn.clone();
            }
        }
        let prompt = self
            .turns
            .get(idx)
            .map(|t| t.prompt.clone())
            .unwrap_or_default();
        let empty = Turn {
            prompt,
            blocks: Vec::new(),
        };
        let Some((start, end)) = self.turn_byte_ranges.get(idx).copied().flatten() else {
            return empty;
        };
        let Some(path) = &self.session.path else {
            return empty;
        };
        // Stream one event line at a time instead of allocating a buffer as
        // large as the whole turn range. Historical turns can span several
        // MiB (many tool rounds); the old range-wide Vec made the first draw's
        // height pass leave an allocation matching the largest turn in the
        // glibc heap even though the buffer was immediately freed.
        use std::io::{BufRead, Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(path) else {
            return empty;
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            return empty;
        }
        let mut reader = std::io::BufReader::new(file.take(end - start));
        let mut line = String::new();
        let mut events = Vec::new();
        let mut exec_ids = std::collections::HashSet::new();
        loop {
            line.clear();
            let Ok(read) = reader.read_line(&mut line) else {
                return empty;
            };
            if read == 0 {
                break;
            }
            let raw = line.trim_end_matches(['\n', '\r']);
            if raw.is_empty() {
                continue;
            }
            let Ok(mut event) = lofi_core::session::store::parse_event(raw) else {
                continue;
            };
            // Successful exec results are not rendered at all in collapsed
            // mode (the nested native-tool rows already show the work). Track
            // exec call ids from assistant messages, then discard only their
            // matching hidden result bodies. Results of other tools and all
            // errors remain intact because their collapsed previews are
            // visible. Verbose mode reparses the exact durable content.
            if !self.verbose {
                if let SessionEventKind::Message(message) = &mut event.kind {
                    if message.role == Role::Assistant {
                        exec_ids.extend(message.blocks.iter().filter_map(|block| match block {
                            ContentBlock::ToolUse { id, name, .. } if name == "exec" => {
                                Some(id.clone())
                            }
                            _ => None,
                        }));
                    }
                    for block in &mut message.blocks {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error: false,
                        } = block
                        {
                            if exec_ids.contains(tool_use_id) {
                                content.clear();
                                content.shrink_to_fit();
                            }
                        }
                    }
                }
            }
            events.push(event);
        }
        turns_from_session_events(&events)
            .into_iter()
            .next()
            .unwrap_or(empty)
    }

    /// Cache a frozen turn only when its complete styled representation is
    /// reasonably small. Large /verbose turns are rendered by row window
    /// instead, preventing one tool result from dominating RSS.
    pub(super) fn ensure_frozen_turn(&mut self, idx: usize, width: usize) {
        const MAX_CACHED_TURN_ROWS: usize = 4096;
        if self.frozen_render.contains(idx)
            || self.frozen_heights.get(idx).copied().unwrap_or(usize::MAX) > MAX_CACHED_TURN_ROWS
        {
            return;
        }
        let theme = self.theme;
        let turn = self.materialize_turn(idx);
        let lines = {
            let cx = view::component::Cx {
                app: self,
                theme,
                width,
                active_turn: false,
            };
            view::blocks::render_turn_lines(&cx, &turn)
        };
        self.frozen_render.insert(idx, lines);
    }

    /// Render a frozen turn's requested row window from its file-backed
    /// source. Used for oversized turns that deliberately bypass the cache.
    pub(super) fn frozen_turn_window(
        &self,
        idx: usize,
        width: usize,
        range: std::ops::Range<usize>,
    ) -> Vec<view::RenderLine> {
        let theme = self.theme;
        let turn = self.materialize_turn(idx);
        let cx = view::component::Cx {
            app: self,
            theme,
            width,
            active_turn: false,
        };
        view::blocks::render_turn_window(&cx, &turn, range)
    }

    /// Sync the frozen-turn cache to the current `turns`. Frozen turns are all
    /// but the last (the last is the live, mutable one rebuilt each frame).
    /// On a wholesale replacement (`bump_render_epoch`) the cache is dropped;
    /// otherwise newly-superseded turns are rendered once, their height
    /// recorded permanently in `frozen_heights`. Their temporary rendered lines
    /// are dropped here; the viewport pass caches only nearby turns. Heights
    /// are kept for every frozen turn so the viewport can be located and the
    /// scroll total computed without holding all rendered lines in memory.
    /// A viewport resize (width change) also drops the cache, since wrapping
    /// and background padding depend on width.
    pub(super) fn ensure_frozen(&mut self, width: usize) {
        if self.frozen_epoch != self.render_epoch || self.frozen_width != width {
            self.frozen_render.clear();
            self.frozen_heights.clear();
            self.frozen_heights_other_mode.clear();
            self.frozen_epoch = self.render_epoch;
            self.frozen_width = width;
        }
        let n = self.turns.len();
        let target = n.saturating_sub(1);
        while self.frozen_heights.len() < target {
            let idx = self.frozen_heights.len();
            let theme = self.theme;
            let turn = self.materialize_turn(idx);
            let height = {
                let cx = view::component::Cx {
                    app: self,
                    theme,
                    width,
                    active_turn: false,
                };
                view::blocks::render_turn_height(&cx, &turn)
            };
            // Heights are the compact permanent index. Measuring a newly
            // frozen verbose turn must not materialize its complete styled
            // output; the viewport pass renders only rows it needs.
            self.frozen_heights.push(height);
        }
        // Defensive: turns shrank without an epoch bump.
        if self.frozen_heights.len() > target {
            self.frozen_heights.truncate(target);
            // Drop any cached entries beyond the new frozen range.
            self.frozen_render.map.retain(|idx, _| *idx < target);
            self.frozen_render.order.retain(|idx| *idx < target);
        }
    }

    /// Keep only rendered frozen turns near the current viewport and ensure
    /// that working set is materialized. Turn heights remain resident for the
    /// whole transcript, so locating the viewport does not require caching all
    /// rendered lines.
    pub(super) fn sync_frozen_cache_for_viewport(
        &mut self,
        off: usize,
        height: usize,
        width: usize,
    ) {
        let frozen = self.frozen_heights.len();
        if frozen == 0 {
            self.frozen_render.clear();
            return;
        }
        let end = off.saturating_add(height);
        let mut pos = 0usize;
        let mut visible: Option<(usize, usize)> = None;
        for idx in 0..frozen {
            let turn_end = pos.saturating_add(self.frozen_heights[idx]);
            if turn_end > off && pos < end {
                visible = Some(visible.map_or((idx, idx), |(first, _)| (first, idx)));
            }
            pos = turn_end.saturating_add(1); // separator before next turn
        }
        // At the bottom the viewport may contain only the live last turn. Keep
        // its nearest frozen neighbor as the scroll-up margin.
        let visible = visible.or(Some((frozen - 1, frozen - 1)));
        self.frozen_render.retain_near(visible, frozen);
        let (first, last) = visible.expect("frozen turns are non-empty");
        let first = first.saturating_sub(1);
        let last = last.saturating_add(1).min(frozen - 1);
        for idx in first..=last {
            self.ensure_frozen_turn(idx, width);
        }
    }

    pub(super) fn run_active(&self) -> bool {
        self.run.is_some()
    }

    pub(super) fn spinner_frame(&self) -> usize {
        self.run.unwrap_or(0)
    }

    /// The active retry state, if the agent is waiting out a backoff.
    #[must_use]
    pub(super) fn retry_state(&self) -> Option<&RetryState> {
        self.retry.as_ref()
    }

    /// `model` or `model:level` — the label shown on the working / turn-end
    /// lines. Mirrors [`session_model`].
    pub(super) fn run_label(&self) -> String {
        self.session_model()
    }

    /// Raw model identity for the session header (`store::create`). The App
    /// holds the rendered `model_label` (`provider/id`) transiently; this
    /// splits it back to raw fields so the header stores raw data, not a
    /// formatted string.
    pub(super) fn run_model(&self) -> RunModel {
        let (provider, id) = match self.model_label.split_once('/') {
            Some((p, r)) => (p, r),
            None => ("", self.model_label.as_str()),
        };
        RunModel {
            provider: provider.to_string(),
            id: id.to_string(),
            thinking: self.thinking,
        }
    }

    /// Elapsed since the current run started; zero when idle.
    pub(super) fn run_elapsed(&self) -> Duration {
        self.run_start.map(|s| s.elapsed()).unwrap_or_default()
    }
}

impl App {
    /// Run an offline compaction over the current session and fold the older
    /// history into a structured summary. Replaces the agent history with
    /// the summary message followed by the kept tail, appends a Compaction
    /// marker to the transcript (so a resumed session rebuilds the same
    /// compacted history), and renders a marker block on the current turn.
    /// Returns true when a compaction actually ran.
    pub(super) fn compact_now(&mut self) -> bool {
        let Some(events) = self.compaction_events() else {
            self.notify(NotifyKind::Warn, "not enough history to compact yet");
            return false;
        };
        let budget = self.derive_compact_budget();
        let opts = CompactOptions {
            max_kept_tokens: budget,
            edit: self.compaction.edit.clone(),
            hooks: vec![std::sync::Arc::new(lofi_core::CodeCompactionHook)],
        };
        let Some(c) = compact(&events, &opts) else {
            self.notify(NotifyKind::Warn, "not enough history to compact yet");
            return false;
        };
        // The full durable log can dominate memory. It is no longer needed
        // once compact() has produced the summary and edited kept tail; drop
        // it before constructing/persisting the replacement history so those
        // representations do not overlap for the rest of this operation.
        drop(events);
        // Lock before persisting so every failure leaves both sources of truth
        // unchanged: a poisoned history lock cannot strand a checkpoint that
        // the running agent never adopted, and a failed write cannot compact
        // memory while resume still reconstructs the old context. The store
        // emits the kept tail and marker as one rollback-on-error batch.
        let new_history = compacted_history(&c);
        let Ok(mut history) = self.history.lock() else {
            self.notify(NotifyKind::Error, "could not update compacted history");
            return false;
        };
        if let Some(path) = &self.session.path {
            if let Err(error) = store::append_compaction(
                path,
                &c.kept_messages,
                self.branch_hint.as_deref(),
                c.summary.clone(),
                c.summarized_range.clone().unwrap_or_default(),
                c.summarized_count,
                c.kept_count,
            ) {
                drop(history);
                self.notify(
                    NotifyKind::Error,
                    format!("could not persist compaction: {error}"),
                );
                return false;
            }
        }
        *history = new_history;
        drop(history);
        // The checkpoint marker is now the active leaf; subsequent turns
        // append from it rather than reusing the rollback branch point.
        self.branch_hint = None;
        // Render the marker. Attach to the last turn when one exists; push a
        // fresh turn otherwise (e.g. compaction invoked before any turn).
        if self.turns.is_empty() {
            self.push_turn(Turn {
                prompt: String::new(),
                blocks: vec![Block::Compaction {
                    summarized: c.summarized_count,
                    kept: c.kept_count,
                    summary: c.summary.clone(),
                }],
            });
        } else {
            self.apply_event(AgentEvent::Compaction {
                summarized: c.summarized_count,
                kept: c.kept_count,
                summary: c.summary.clone(),
            });
        }
        // The context gauge's last reading reflects the pre-compaction fill;
        // drop it so the auto-trigger does not re-fire on the same crossing
        // and the gauge waits for the next round's real (smaller) usage.
        self.reset_compaction_gauges();
        self.last_compact_msg_count = self.messages_since_last_compact();
        self.compacted = true;
        self.bump_render_epoch();
        self.debug_sample("compaction");
        true
    }

    /// Auto-compact when the latest round's input tokens cross above the
    /// configured threshold. Mirrors pi's hm-smart-compact: the trigger fires
    /// only on the upward crossing (hysteresis), so a session hovering above
    /// the threshold is not re-compacted every turn. The baseline resets to
    /// `None` after a compaction (and on rollback/resume) so the next
    /// crossing re-evaluates cleanly. Called after a run fully finishes — the
    /// lofi equivalent of pi's `agent_settled`.
    ///
    /// This is the **soft** path: speculative, post-settled, and only active
    /// when a soft cap (`max_context_tokens` / `context_ratio`) is set.
    /// The **hard** cap (`reserved_context_tokens`) is enforced mid-run by
    /// the engine (force-compact + force-continue), not here.
    /// `/recall [query]` — search the full session transcript (including
    /// messages a compaction folded away) and render the matches inline in
    /// the log. With no query, browse the most recent entries. Args:
    /// `scope:all` (whole session) / `scope:lineage` (default, active branch)
    /// / `scope:compaction:N` or `scope:compaction:latest` (within one
    /// compaction's summarized range); `page:N` for paged search results.
    ///
    /// The user-facing command renders to the log only; the model reaches
    /// the same engine via the `lofi.recall` native tool.
    pub(super) fn recall_now(&mut self, line: &str) {
        use lofi_core::recall::{recall, RecallRequest};

        let raw = line.trim().strip_prefix("/recall").unwrap_or("").trim();
        let (scope, rest) = parse_recall_args(raw);
        let page = parse_recall_page(&rest);
        let query_text = rest
            .split_whitespace()
            .filter(|t| !t.starts_with("page:"))
            .collect::<Vec<_>>()
            .join(" ");
        let req = RecallRequest {
            query: (!query_text.is_empty()).then_some(query_text),
            scope,
            page,
            expand: Vec::new(),
        };

        let outcome = if let Some(path) = &self.session.path {
            lofi_core::recall::recall_file(path, &req)
        } else {
            let Some(events) = self.compaction_events() else {
                self.notify(NotifyKind::Warn, "no session history yet");
                return;
            };
            recall(&events, &req)
        };
        // Render inline as a read-only turn so the result lives in the log
        // alongside the conversation; the prompt line echoes the invocation.
        let prompt = format!(
            "/recall{}{}",
            if rest.is_empty() {
                String::new()
            } else {
                " ".to_string()
            },
            rest
        );
        self.push_turn(Turn {
            prompt,
            blocks: vec![Block::Text(outcome.text)],
        });
        self.notify(NotifyKind::Info, outcome.status);
        self.bump_render_epoch();
    }

    pub(super) fn maybe_auto_compact(&mut self) {
        if !self.compaction.auto.enable {
            return;
        }
        let Some(usage) = self.status_usage else {
            return;
        };
        let limit = self.ctx_limit.max(DEFAULT_CTX_LIMIT);
        let Some(threshold) = self.compaction.soft_threshold(limit) else {
            return;
        };
        // Use the full prompt size (non-cached + cached) so heavy prompt
        // caching doesn't mask the real context size. Without this, a session
        // with 150k cached tokens and 9k non-cached would read as 9k — well
        // below the threshold — and never auto-compact.
        let current = usage.input_tokens + usage.cache_read_tokens;
        // `was_below` is true when the prior round was at or below the
        // threshold (or there was no prior reading). Only an upward
        // crossing — prev at/below, current above — triggers a compaction,
        // so a session hovering above the threshold is not re-compacted
        // every turn.
        let was_below = match self.prev_ctx_tokens {
            None => true,
            Some(prev) => !(prev > threshold && current > threshold),
        };
        self.prev_ctx_tokens = Some(current);
        if !was_below || current <= threshold {
            return;
        }
        // Soft cooldown: if the last compaction was too few messages ago,
        // the kept tail is likely still too large for another compaction to
        // help. Skip rather than wasting a compact that barely shrinks the
        // context (and then immediately re-triggers).
        let msgs_since = self.messages_since_last_compact();
        if self.last_compact_msg_count > 0
            && msgs_since < self.compaction.min_messages_between_hard_compacts
        {
            return;
        }
        if self.compact_now() {
            // Reset the baseline so a still-above-threshold context can
            // re-fire after the compaction (and a below-threshold one starts
            // a fresh crossing).
            self.prev_ctx_tokens = None;
        }
    }

    /// Count assistant messages on the active path produced after the most
    /// recent `Compaction` marker — i.e. new agent content since the last
    /// compaction. Used by the hard-cap cooldown: a force-compact is only
    /// allowed when at least `min_messages_between_hard_compacts` assistant
    /// messages have elapsed since the last compact, otherwise the kept tail
    /// alone is too big and compacting again cannot help (the run errors
    /// out). Returns a large count when no compaction has happened yet.
    pub(super) fn messages_since_last_compact(&self) -> usize {
        let Some(events) = self.compaction_events() else {
            return usize::MAX;
        };
        let path = store::active_path_from_leaf(&events);
        // Find the last Compaction marker on the active path, then count
        // assistant messages after it. When there is no compaction marker,
        // return a large count so the cooldown is effectively disabled.
        let last_compaction = path
            .iter()
            .rev()
            .find(|&&i| matches!(events[i].kind, SessionEventKind::Compaction { .. }))
            .copied();
        if last_compaction.is_none() {
            return usize::MAX;
        }
        count_assistant_after_compaction(&path, &events, last_compaction)
    }

    /// Derive compaction-related state from the transcript's active path
    /// after a resume or rollback. The transcript is the source of truth:
    ///
    /// - `compacted`: `true` when the active path ends with a `Compaction`
    ///   marker and no `TurnEnd`/`TurnFailed`/`ContextPressure` follows it
    ///   (i.e. we compacted but haven't run a new turn yet, so the context
    ///   gauge has no real usage to show and should display `c`).
    ///
    /// - `last_compact_msg_count`: the number of assistant messages after
    ///   the last `Compaction` marker on the active path, so the auto-compact
    ///   cooldown works immediately on resume instead of being disabled.
    ///
    /// - `prev_ctx_tokens`: the last completed turn's prompt size
    ///   (`input_tokens + cache_read_tokens`), so the auto-compact
    ///   hysteresis has a proper baseline and doesn't fire on the first
    ///   post-resume turn when the context was already above threshold.
    pub(super) fn restore_compaction_state(&mut self, events: &[SessionEvent]) {
        let path = store::active_path_from_leaf(events);
        let mut last_compaction_idx: Option<usize> = None;
        let mut last_usage: Option<Usage> = None;
        for &i in &path {
            match &events[i].kind {
                SessionEventKind::Compaction { .. } => {
                    last_compaction_idx = Some(i);
                }
                SessionEventKind::TurnEnd { usage, .. }
                | SessionEventKind::TurnFailed { usage, .. } => {
                    last_usage = Some(*usage);
                }
                _ => {}
            }
        }
        // `compacted`: true only when the last event on the active path is a
        // Compaction marker (no TurnEnd/TurnFailed after it).
        self.compacted = match last_compaction_idx {
            Some(idx) => path.last().is_some_and(|&last| last == idx),
            None => false,
        };
        // `last_compact_msg_count`: reuse the shared counting helper so
        // there is one counting implementation.
        self.last_compact_msg_count =
            count_assistant_after_compaction(&path, events, last_compaction_idx);
        // `prev_ctx_tokens`: from the last TurnEnd/TurnFailed's usage, so the
        // hysteresis has a baseline and doesn't immediately re-trigger.
        self.prev_ctx_tokens = last_usage.map(|u| u.input_tokens + u.cache_read_tokens);
        // `status_usage`: restore the last turn's usage so the context gauge
        // shows a real number on resume. When the session ends with a
        // Compaction marker (compacted == true), the gauge shows "c" instead;
        // in that case clear status_usage so a stale pre-compaction reading
        // doesn't override the "c" indicator.
        if self.compacted {
            self.status_usage = None;
        } else {
            self.status_usage = last_usage;
        }
    }

    /// Gather the active-path events for compaction: load the transcript
    /// (so native tool records are available) when a session file exists,
    /// otherwise synthesize a linear event log from the in-memory history.
    /// Returns None when the history is empty.
    fn compaction_events(&self) -> Option<Vec<SessionEvent>> {
        if let Some(path) = &self.session.path {
            // The transcript is append-only and can contain large abandoned
            // branches. Build its tiny tree index first, then deserialize only
            // the selected lineage instead of loading the entire file and
            // cloning the branch out of it.
            let (_meta, index, _size) = store::load_index(path).ok()?;
            return store::load_compaction_path(path, &index, self.branch_hint.as_deref()).ok();
        }
        let msgs = self.history.lock().ok()?;
        if msgs.is_empty() {
            return None;
        }
        let mut events = Vec::with_capacity(msgs.len());
        for (i, m) in msgs.iter().enumerate() {
            events.push(SessionEvent {
                id: i.to_string(),
                parent_id: if i == 0 {
                    None
                } else {
                    Some((i - 1).to_string())
                },
                kind: SessionEventKind::Message(m.clone()),
            });
        }
        Some(events)
    }
}

/// Count assistant messages on the active path after the last
/// Compaction marker. Shared by restore_compaction_state (resume) and
/// messages_since_last_compact (cooldown) so the counting logic lives in
/// one place. Returns 0 when there is no compaction marker.
fn count_assistant_after_compaction(
    path: &[usize],
    events: &[SessionEvent],
    last_compaction_idx: Option<usize>,
) -> usize {
    let Some(ci) = last_compaction_idx else {
        return 0;
    };
    let mut n = 0usize;
    for &i in path {
        if i == ci {
            n = 0;
            continue;
        }
        if i > ci {
            if let SessionEventKind::Message(m) = &events[i].kind {
                if m.role == Role::Assistant {
                    n += 1;
                }
            }
        }
    }
    n
}

/// Parse a `scope:…` directive out of a `/recall` argument string. Returns
/// the resolved scope and the argument text with the directive removed.
fn parse_recall_args(raw: &str) -> (lofi_core::recall::RecallScope, String) {
    use lofi_core::recall::{CompactionTarget, RecallScope};
    let mut scope = RecallScope::default();
    let mut cleaned = String::new();
    for tok in raw.split_whitespace() {
        if let Some(val) = tok.strip_prefix("scope:") {
            let val = val.trim();
            scope = match val {
                "all" => RecallScope::All,
                "lineage" => RecallScope::Lineage,
                "latest" => RecallScope::Compaction(CompactionTarget::Latest),
                other if other.starts_with("compaction:") => {
                    let n = other.strip_prefix("compaction:").unwrap_or("");
                    match n.parse::<usize>() {
                        Ok(i) => RecallScope::Compaction(CompactionTarget::Index(i)),
                        Err(_) => RecallScope::Lineage,
                    }
                }
                _ => RecallScope::Lineage,
            };
        } else {
            if !cleaned.is_empty() {
                cleaned.push(' ');
            }
            cleaned.push_str(tok);
        }
    }
    (scope, cleaned)
}

/// Pull a `page:N` token (1-based) out of the argument string, defaulting to 1.
fn parse_recall_page(rest: &str) -> usize {
    rest.split_whitespace()
        .find_map(|t| {
            t.strip_prefix("page:")
                .and_then(|n| n.parse::<usize>().ok())
        })
        .unwrap_or(1)
        .max(1)
}
