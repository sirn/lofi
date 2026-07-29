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
            settled_usage_fresh: false,
            compacted: false,
            context_pressure: false,
            cost: 0.0,
            turn_cost: 0.0,
            turn_has_round_usage: false,

            total_in: 0,
            total_out: 0,
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
                cursor: None,
                cwd: PathBuf::new(),
            },
            picker: None,
            tree_picker: None,
            tree_picker_index: None,
            tree_picker_pending: std::collections::HashSet::new(),
            picker_load_tx: None,
            picker_generation: Arc::new(AtomicU64::new(0)),
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
            deferred_confirms: Vec::new(),
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
            collapsed_turns: Box::new(RefCell::new(CollapsedTurnCache::new())),
            frozen_heights: Vec::new(),
            frozen_heights_other_mode: Vec::new(),
            turn_byte_ranges: Vec::new(),
            turn_event_offsets: Vec::new(),
            render_epoch: 0,
            frozen_epoch: 0,
            frozen_width: 0,
            render_profile: Box::default(),
        }
    }

    pub(super) fn session_model(&self) -> String {
        format!(
            "{}{}",
            self.model_label,
            self.thinking_label.as_deref().unwrap_or("")
        )
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

    #[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
    pub(super) fn apply_event(&mut self, ev: AgentEvent) {
        if let AgentEvent::TurnStart { prompt } = ev {
            if let Some(previous) = self.turns.len().checked_sub(1) {
                if self
                    .turn_byte_ranges
                    .get(previous)
                    .is_some_and(Option::is_some)
                {
                    self.turns[previous].blocks.clear();
                }
            }
            self.push_turn(Turn {
                prompt,
                blocks: Vec::new(),
            });
            self.turn_cost = 0.0;
            self.turn_has_round_usage = false;
            self.settled_usage_fresh = false;
            return;
        }
        if let AgentEvent::TurnContinue = ev {
            self.turn_cost = 0.0;
            self.turn_has_round_usage = false;
            self.settled_usage_fresh = false;
            return;
        }
        match ev {
            AgentEvent::RetryStart {
                attempt,
                max_attempts,
                ..
            } => {
                self.retry = Some(RetryState {
                    attempt,
                    max_attempts,
                });
                return;
            }
            AgentEvent::RetryEnd { .. } => {
                self.retry = None;
                return;
            }
            AgentEvent::RoundCommitted {
                byte_start,
                byte_end,
            } => {
                self.merge_last_turn_range(byte_start, byte_end);
                if let Some(turn) = self.turns.last_mut() {
                    for block in &mut turn.blocks {
                        if let Block::Tool(tool) = block {
                            if tool.name == "exec" && tool.done && !tool.is_error {
                                tool.result_committed = true;
                                if !self.verbose {
                                    tool.result = None;
                                }
                            }
                        }
                    }
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
                self.merge_last_turn_range(byte_start, byte_end);
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
                self.status_usage = Some(usage);
                self.settled_usage_fresh = true;
                self.compacted = false;
                return;
            }
            AgentEvent::TurnEnd { cost, usage, .. } => {
                if self.turn_has_round_usage {
                    self.cost += self.turn_cost;
                } else {
                    self.cost += cost;
                    self.total_in += usage.input_tokens;
                    self.total_out += usage.output_tokens;
                    self.status_usage = Some(usage);
                }
                self.turn_cost = 0.0;
                self.turn_has_round_usage = false;
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
            _ => {}
        }
        apply_event_to_turns(&mut self.turns, ev);
    }

    pub(super) fn run_finished(&mut self) {
        if let Some(turn) = self.turns.last_mut() {
            finalize_open_thinking(turn);
        }
        // Keep the just-finished turn live in memory. It was built from the
        // lossless event stream and is the authoritative UI representation for
        // the frame in which the run settles. Clearing it here made visibility
        // depend on an immediate, fallible disk re-read; a range/read/replay
        // failure was intentionally mapped to an empty shell by
        // materialize_turn(), making an entire agent response disappear.
        // Submit/spawn_prompt freezes this turn when the next prompt starts, so
        // memory remains bounded to the viewport working set plus one latest
        // turn rather than growing with the session.
        // The turn-end marker (model, elapsed, cost, usage) arrives as an
        // `AgentEvent::TurnEnd` emitted by the engine, which also writes it
        // to the transcript — so there is nothing to stamp or persist here.
        self.run_start = None;
        self.run = None;
        self.retry = None;
    }

    pub(super) fn reset_compaction_gauges(&mut self) {
        self.status_usage = None;
        self.prev_ctx_tokens = None;
        self.settled_usage_fresh = false;
        self.compacted = false;
    }

    /// Derive the kept-tail token budget for `plan_cut` from the compaction
    /// thresholds. Prefers the soft threshold, falls back to the hard
    /// threshold, and uses 50% of the threshold so the kept tail stays well
    /// under the cap. Returns 0 when no threshold is configured (disables the
    /// oversized-turn guard).
    pub(super) fn derive_compact_budget(&self) -> usize {
        self.compaction
            .soft_threshold(self.ctx_limit)
            .or_else(|| self.compaction.hard_threshold(self.ctx_limit))
            .map_or(0, |t| (t / 2) as usize)
    }

    pub(super) fn bump_render_epoch(&mut self) {
        self.render_epoch = self.render_epoch.wrapping_add(1);
        self.frozen_heights_other_mode.clear();
    }

    pub(super) fn switch_verbose_layout(&mut self) {
        self.frozen_render.clear();
        std::mem::swap(
            &mut self.frozen_heights,
            &mut self.frozen_heights_other_mode,
        );
    }

    fn merge_last_turn_range(&mut self, byte_start: u64, byte_end: u64) {
        if let Some(range) = self.turn_byte_ranges.last_mut() {
            *range = Some(match *range {
                Some((start, end)) => (start.min(byte_start), end.max(byte_end)),
                None => (byte_start, byte_end),
            });
        }
    }

    pub(super) fn restore_last_committed_exec_results(&mut self) {
        let Some((start, end)) = self.turn_byte_ranges.last().copied().flatten() else {
            return;
        };
        let Some(cursor) = self.session.cursor.as_ref() else {
            return;
        };
        let Ok(events) = cursor.events_in_range(start, end) else {
            return;
        };
        let mut results = std::collections::HashMap::new();
        for event in events {
            if let SessionEventKind::Message(message) = event.kind {
                for block in message.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: false,
                    } = block
                    {
                        results.insert(tool_use_id, content);
                    }
                }
            }
        }
        let Some(turn) = self.turns.last_mut() else {
            return;
        };
        for block in &mut turn.blocks {
            if let Block::Tool(tool) = block {
                if tool.name == "exec" && tool.result_committed && tool.result.is_none() {
                    if let Some(result) = results.remove(&tool.id) {
                        tool.result = Some(result);
                    }
                }
            }
        }
    }

    pub(super) fn release_last_committed_exec_results(&mut self) {
        let Some(turn) = self.turns.last_mut() else {
            return;
        };
        for block in &mut turn.blocks {
            if let Block::Tool(tool) = block {
                if tool.name == "exec" && tool.result_committed && !tool.is_error {
                    tool.result = None;
                }
            }
        }
    }

    pub(super) fn push_turn(&mut self, turn: Turn) {
        self.turns.push(turn);
        self.turn_byte_ranges.push(None);
        self.turn_event_offsets.push(None);
    }

    pub(super) fn insert_turn(&mut self, idx: usize, turn: Turn) {
        self.collapsed_turns.get_mut().clear();
        self.turns.insert(idx, turn);
        self.turn_byte_ranges.insert(idx, None);
        self.turn_event_offsets.insert(idx, None);
    }

    /// Reconstruct a frozen turn's blocks. If the turn still holds its blocks
    /// in memory (ephemeral session, or not yet frozen), clone them. Otherwise
    /// re-parse the turn's byte range from the transcript file. On any read or
    /// parse failure the turn's prompt is preserved with empty blocks.
    pub(super) fn materialize_turn(&self, idx: usize) -> Arc<Turn> {
        if let Some(turn) = self.turns.get(idx) {
            if !turn.blocks.is_empty() {
                return Arc::new(turn.clone());
            }
        }
        let materialize_started = Instant::now();
        if !self.verbose {
            if let Some(turn) = self.collapsed_turns.borrow_mut().get(idx) {
                return turn;
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
        let Some(cursor) = self.session.cursor.as_ref() else {
            return Arc::new(empty);
        };
        let selected_offsets = self.turn_event_offsets.get(idx).and_then(Option::as_deref);
        let mut events = if let Some(offsets) = selected_offsets {
            let loaded = if self.verbose {
                cursor.events_at(offsets)
            } else {
                cursor.collapsed_events_at(offsets)
            };
            match loaded {
                Ok(events) => events,
                Err(_) => return Arc::new(empty),
            }
        } else {
            let Some((start, end)) = self.turn_byte_ranges.get(idx).copied().flatten() else {
                return Arc::new(empty);
            };
            match cursor.events_in_range(start, end) {
                Ok(events) => events,
                Err(_) => return Arc::new(empty),
            }
        };
        let mut exec_ids = std::collections::HashSet::new();
        for event in &mut events {
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
        }
        let turns = if selected_offsets.is_some() {
            turns_from_selected_session_events(&events)
        } else {
            turns_from_session_events(&events)
        };
        let turn = Arc::new(turns.into_iter().next().unwrap_or(empty));
        if !self.verbose {
            let mut cache = self.collapsed_turns.borrow_mut();
            cache.insert(idx, turn.clone());
            cache.finish_materialize(materialize_started.elapsed().as_micros());
        }
        turn
    }

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

    /// Sync the frozen-turn cache to the current `turns`. Normally frozen turns
    /// are all but the live mutable last one; an idle fully file-backed view can
    /// freeze the final turn too.
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
        // Normally the last turn is live (or retained in memory after resume),
        // so only its prefix is frozen. /tree rollback deliberately drops the
        // selected last turn's blocks too; when it is idle and file-backed,
        // include it in the frozen set so production rendering materializes it
        // through the bounded cache instead of treating an empty shell as live.
        let target = if !self.run_active()
            && self.turns.last().is_some_and(|turn| turn.blocks.is_empty())
            && self.turn_byte_ranges.last().is_some_and(Option::is_some)
        {
            n
        } else {
            n.saturating_sub(1)
        };
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
        if self.frozen_heights.len() > target {
            self.frozen_heights.truncate(target);
            self.frozen_render.map.retain(|idx, _| *idx < target);
            self.frozen_render.order.retain(|idx| *idx < target);
        }
    }

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
        let visible = visible.unwrap_or((frozen - 1, frozen - 1));
        self.frozen_render.retain_near(Some(visible), frozen);
        let (first, last) = visible;
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
        if let Some(cursor) = &self.session.cursor {
            let summarized_range = c.summarized_range.clone().unwrap_or_default();
            match cursor.append_compaction(
                &c.kept_messages,
                &c.summary,
                &summarized_range,
                store::CompactionCounts {
                    summarized: c.summarized_count,
                    represented: c.represented_count,
                    kept: c.kept_count,
                },
            ) {
                Ok((_byte_start, _byte_end)) => {}
                Err(error) => {
                    drop(history);
                    self.notify(
                        NotifyKind::Error,
                        format!("could not persist compaction: {error}"),
                    );
                    return false;
                }
            }
        }
        *history = new_history;
        drop(history);
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
        self.compacted = true;
        self.bump_render_epoch();
        self.debug_sample("compaction");
        true
    }

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

        let outcome = if let Some(cursor) = self.session.cursor.as_ref() {
            lofi_core::recall::recall_cursor(cursor, &req)
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
        if !std::mem::take(&mut self.settled_usage_fresh) || !self.compaction.auto.enable {
            return;
        }
        let Some(usage) = self.status_usage else {
            return;
        };
        let Some(threshold) = self.compaction.soft_threshold(self.ctx_limit) else {
            return;
        };
        let current = usage.input_tokens + usage.cache_read_tokens;
        if current <= threshold {
            self.prev_ctx_tokens = Some(current);
            return;
        }
        if self
            .prev_ctx_tokens
            .is_some_and(|previous| previous > threshold)
        {
            return;
        }

        self.compact_now();
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
        let path: Vec<usize> = (0..events.len()).collect();
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

    /// Gather the active-path events for compaction: load the transcript
    /// (so native tool records are available) when a session file exists,
    /// otherwise synthesize a linear event log from the in-memory history.
    /// Returns None when the history is empty.
    fn compaction_events(&self) -> Option<Vec<SessionEvent>> {
        if let Some(cursor) = self.session.cursor.as_ref() {
            return cursor.load_compaction_events().ok();
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

/// Count assistant messages on the active path after the last compaction
/// marker. Shared by the hard-cap cooldown paths so the counting logic lives
/// in one place. Returns 0 when there is no compaction marker.
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

fn parse_recall_page(rest: &str) -> usize {
    rest.split_whitespace()
        .find_map(|t| {
            t.strip_prefix("page:")
                .and_then(|n| n.parse::<usize>().ok())
        })
        .unwrap_or(1)
        .max(1)
}
