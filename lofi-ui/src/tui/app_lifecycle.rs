#![allow(clippy::wildcard_imports)]

use super::*;

impl App {
    #[allow(clippy::too_many_lines)]
    pub(super) fn new(
        model_label: String,
        thinking: ThinkingLevel,
        service_tier: ServiceTier,
        ctx_limit: u64,
        compaction: lofi_types::CompactionConfig,
        system_prompt: String,
    ) -> Self {
        let thinking_label =
            (thinking != ThinkingLevel::Off).then(|| format!(":{}", thinking.as_str()));
        let service_label = (service_tier != ServiceTier::Auto)
            .then(|| format!("@{}", service_tier.as_str()));
        Self {
            turns: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            lifecycle: AgentLifecycle::new(
                compaction,
                if ctx_limit > 0 {
                    ctx_limit
                } else {
                    DEFAULT_CTX_LIMIT
                },
            ),
            system_prompt,
            history_nav: Vec::new(),
            history_idx: None,
            input_stash: String::new(),
            model_label,
            thinking_label,
            thinking,
            service_label,
            service_tier,
            status_usage: None,
            ctx_limit: if ctx_limit > 0 {
                ctx_limit
            } else {
                DEFAULT_CTX_LIMIT
            },
            settled_usage_fresh: false,
            compacted: false,
            context_pressure: false,
            cost: 0.0,
            turn_cost: 0.0,
            turn_has_round_usage: false,

            total_in: 0,
            total_out: 0,
            prompt_queue: Vec::new(),
            startup_notices: Vec::new(),
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
                sink: None,
                cursor: None,
                cwd: PathBuf::new(),
            },
            picker: None,
            tree_picker: None,
            tree_picker_snapshot: None,
            tree_picker_pending: std::collections::HashSet::new(),
            picker_load_tx: None,
            picker_generation: Arc::new(AtomicU64::new(0)),
            model_picker: None,
            thinking_picker: None,
            service_picker: None,
            theme_picker: None,
            model_choices: Vec::new(),
            pending_model_switch: None,
            info: None,
            slash_complete: None,
            no_models_hint: None,
            theme: Theme::default(),
            theme_mode: lofi_types::ThemeMode::default(),
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
            height_remeasure_from: None,
            render_profile: Box::default(),
            // Cloned session job registry. Set by the caller from the running
            // agent; `None` until the agent attaches.
            jobs: None,
            jobs_receiver_stale: false,
            jobs_modal: None,
        }
    }

    pub(super) fn session_model(&self) -> String {
        format!(
            "{}{}{}",
            self.model_label,
            self.thinking_label.as_deref().unwrap_or(""),
            self.service_label.as_deref().unwrap_or("")
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
        if let AgentEvent::TurnStart { prompt, kind } = ev {
            self.freeze_previous_file_backed_turn();
            self.push_turn(Turn {
                prompt,
                kind,
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
            AgentEvent::Notice(msg) => {
                self.notify(NotifyKind::Warn, msg);
                return;
            }
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
            AgentEvent::TurnFailed { cost, usage, .. }
            | AgentEvent::TurnCancelled { cost, usage, .. } => {
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
        let starts_standalone_turn = matches!(&ev, AgentEvent::UserShell { .. });
        if starts_standalone_turn {
            self.freeze_previous_file_backed_turn();
        }
        let previous_turns = self.turns.len();
        apply_event_to_turns(&mut self.turns, ev);
        if starts_standalone_turn {
            debug_assert_eq!(self.turns.len(), previous_turns + 1);
            self.turn_byte_ranges.push(None);
            self.turn_event_offsets.push(None);
        }
    }

    fn freeze_previous_file_backed_turn(&mut self) {
        let Some(previous) = self.turns.len().checked_sub(1) else {
            return;
        };
        if self
            .turn_byte_ranges
            .get(previous)
            .is_some_and(Option::is_some)
        {
            self.turns[previous].blocks.clear();
        }
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
        self.settled_usage_fresh = false;
        self.compacted = false;
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
                        ..
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
            kind: lofi_types::PromptKind::User,
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
                            ..
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
        let mut turn = turns.into_iter().next().unwrap_or(empty);
        if !self.verbose {
            view::blocks::compact_native_previews(&mut turn);
        }
        let turn = Arc::new(turn);
        if !self.verbose {
            let mut cache = self.collapsed_turns.borrow_mut();
            cache.insert(idx, &turn);
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
        let epoch_changed = self.frozen_epoch != self.render_epoch;
        let width_changed = self.frozen_width != width;
        if epoch_changed {
            // Content changed wholesale: every cached height is invalid, so
            // drop and re-measure eagerly below. No incremental path here —
            // the turns themselves are different.
            self.frozen_render.clear();
            self.frozen_heights.clear();
            self.frozen_heights_other_mode.clear();
            self.frozen_epoch = self.render_epoch;
            self.frozen_width = width;
            self.height_remeasure_from = None;
        } else if width_changed {
            // Width-only resize: the styled lines in `frozen_render` are
            // bound to the old width and must be rebuilt, but the turn
            // content is identical — only its wrap changes. Keep the stale
            // heights as the total/base source so the bottom anchor and
            // thumb stay put, and re-measure incrementally (visible window
            // now, the rest on the tick loop) instead of stalling this frame.
            self.frozen_render.clear();
            self.frozen_heights_other_mode.clear();
            self.frozen_width = width;
            // Re-measure from the end: the cursor is an exclusive upper
            // bound that remeasure_heights_step walks down to zero.
            self.height_remeasure_from = Some(self.frozen_heights.len());
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
            // Heights are the compact permanent index. Measuring a newly
            // frozen verbose turn must not materialize its complete styled
            // output; the viewport pass renders only rows it needs.
            let height = self.measure_turn_height(self.frozen_heights.len(), width);
            self.frozen_heights.push(height);
        }
        if self.frozen_heights.len() > target {
            self.frozen_heights.truncate(target);
            self.frozen_render.map.retain(|idx, _| *idx < target);
            self.frozen_render.order.retain(|idx| *idx < target);
            // A truncated pending re-measure must not run past the new end.
            if let Some(hi) = self.height_remeasure_from {
                self.height_remeasure_from = if hi > target { Some(target) } else { Some(hi) };
            }
        }
    }

    /// Render-height of one frozen turn at `width`. Shared by the eager fill
    /// in `ensure_frozen` and the incremental resize re-measure.
    fn measure_turn_height(&self, idx: usize, width: usize) -> usize {
        let theme = self.theme;
        let turn = self.materialize_turn(idx);
        let cx = view::component::Cx {
            app: self,
            theme,
            width,
            active_turn: false,
        };
        view::blocks::render_turn_height(&cx, &turn)
    }

    /// Re-measure up to `budget` stale frozen heights at the current
    /// `frozen_width`, advancing `height_remeasure_from`. Runs on the tick
    /// loop after a width-only resize so no single frame pays the full cost.
    /// Returns true while work remains (so the caller keeps the frame dirty).
    pub(super) fn remeasure_heights_step(&mut self, budget: usize) -> bool {
        let next_hi = match self.height_remeasure_from {
            Some(hi) => hi.min(self.frozen_heights.len()),
            None => return false,
        };
        if next_hi == 0 {
            self.height_remeasure_from = None;
            return false;
        }
        let width = self.frozen_width;
        // Work back-to-front: the viewport is almost always pinned to the
        // bottom, so the last frozen turns are the visible ones. Measuring
        // those first makes the on-screen content exact on the first tick;
        // the off-screen prefix converges over the following ticks.
        let lo = next_hi.saturating_sub(budget);
        for idx in lo..next_hi {
            self.frozen_heights[idx] = self.measure_turn_height(idx, width);
        }
        if lo == 0 {
            self.height_remeasure_from = None;
            false
        } else {
            self.height_remeasure_from = Some(lo);
            true
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
            thinking: self.thinking.clone(),
            service_tier: self.service_tier.clone(),
        }
    }

    pub(super) fn run_elapsed(&self) -> Duration {
        self.run_start.map(|s| s.elapsed()).unwrap_or_default()
    }
}

impl App {
    fn render_compaction(&mut self, compaction: &lofi_core::Compaction) {
        let event = AgentEvent::Compaction {
            summarized: compaction.summarized_count,
            kept: compaction.kept_count,
            summary: compaction.summary.clone(),
        };
        if self.turns.is_empty() {
            self.push_turn(Turn {
                prompt: String::new(),
                kind: lofi_types::PromptKind::User,
                blocks: Vec::new(),
            });
        }
        // A file-backed turn renders from disk on demand; appending the
        // compaction marker to its empty live shell would make that lone block
        // the turn's entire live content, so the renderer stops re-reading the
        // real response from disk and the transcript collapses to just the
        // marker. Hydrate the turn first so the marker appends to, rather than
        // replaces, the visible content.
        let last = self.turns.len().saturating_sub(1);
        let hydrated = (*self.materialize_turn(last)).clone();
        if let Some(turn) = self.turns.get_mut(last) {
            turn.blocks = hydrated.blocks;
        }
        self.apply_event(event);
        self.bump_render_epoch();
        self.debug_sample("compaction");
    }

    /// Request an immediate core-owned compaction and render its outcome.
    pub(super) fn compact_now(&mut self) -> bool {
        let cursor = self.session.cursor.clone();
        let system_prompt = self.system_prompt.clone();
        match self.lifecycle.compact(cursor.as_ref(), &system_prompt) {
            Ok(Some(compaction)) => {
                self.render_compaction(&compaction);
                true
            }
            Ok(None) => {
                self.notify(NotifyKind::Warn, "not enough history to compact yet");
                false
            }
            Err(error) => {
                self.notify(NotifyKind::Error, format!("could not compact: {error}"));
                false
            }
        }
    }

    /// Request core-owned recall and render the read-only result.
    pub(super) fn recall_now(&mut self, line: &str) {
        let cursor = self.session.cursor.as_ref();
        match self.lifecycle.recall_line(cursor, line) {
            Ok(Some(outcome)) => {
                self.push_turn(Turn {
                    prompt: line.trim().to_string(),
                    kind: lofi_types::PromptKind::User,
                    blocks: vec![Block::Text(outcome.text)],
                });
                self.notify(NotifyKind::Info, outcome.status);
                self.bump_render_epoch();
            }
            Ok(None) => self.notify(NotifyKind::Warn, "no session history yet"),
            Err(error) => self.notify(NotifyKind::Error, format!("recall failed: {error}")),
        }
    }

    /// Let core evaluate soft-compaction policy; the UI only renders outcomes.
    pub(super) fn maybe_auto_compact(&mut self) {
        if !std::mem::take(&mut self.settled_usage_fresh) {
            return;
        }
        let Some(usage) = self.status_usage else {
            return;
        };
        let cursor = self.session.cursor.clone();
        let system_prompt = self.system_prompt.clone();
        match self
            .lifecycle
            .auto_compact(usage, cursor.as_ref(), &system_prompt)
        {
            Ok(Some(compaction)) => self.render_compaction(&compaction),
            Ok(None) => {}
            Err(error) => self.notify(NotifyKind::Error, format!("could not compact: {error}")),
        }
    }

    pub(super) fn hard_compact(&mut self) -> HardCompactOutcome {
        let cursor = self.session.cursor.clone();
        let system_prompt = self.system_prompt.clone();
        match self.lifecycle.hard_compact(cursor.as_ref(), &system_prompt) {
            Ok(HardCompactOutcome::Compacted(compaction)) => {
                self.render_compaction(&compaction);
                HardCompactOutcome::Compacted(compaction)
            }
            Ok(outcome) => outcome,
            Err(error) => {
                self.notify(NotifyKind::Error, format!("could not compact: {error}"));
                HardCompactOutcome::NotEnoughHistory
            }
        }
    }
}
