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
        let service_label =
            (service_tier != ServiceTier::Auto).then(|| format!("@{}", service_tier.as_str()));
        Self {
            transcript: Transcript::new(),
            layout: TranscriptLayout::new(),
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
            run_model_label: None,
            retry: None,
            pending_prompt_start: false,
            lifecycle_tx: None,
            lifecycle_busy: false,
            continue_after_hard_compact: false,
            pinned: true,
            top_line: 0,
            last_base: 0,
            expanded_details: HashMap::new(),
            detail_focus: None,
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
            policy_picker: None,
            policy_override: None,
            auto_mode_configured: false,
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
            log_details: Vec::new(),
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
        if matches!(ev, AgentEvent::Prompt { .. }) {
            if let Some(turn) = self.transcript.turns.last_mut() {
                turn.blocks.clear();
            }
        }
        self.apply_event(ev);
    }

    #[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
    pub(super) fn apply_event(&mut self, ev: AgentEvent) {
        if let AgentEvent::RunStart = ev {
            self.turn_cost = 0.0;
            self.turn_has_round_usage = false;
            self.settled_usage_fresh = false;
            return;
        }
        if let AgentEvent::Prompt { prompt, kind } = ev {
            // spawn_agent_run pre-pushes the prompt at submit time so it
            // shows before the working indicator's first frame. The
            // matching Prompt is then a confirmation, not a new turn.
            let awaited = self.pending_prompt_start
                && self.transcript.turns.last().is_some_and(|turn| {
                    turn.blocks.is_empty() && turn.prompt == prompt && turn.kind == kind
                });
            self.pending_prompt_start = false;
            if !awaited {
                self.transcript.freeze_previous_file_backed_turn();
                self.transcript.push_turn(Turn {
                    prompt,
                    kind,
                    blocks: Vec::new(),
                });
            }
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
                self.transcript.merge_last_turn_range(byte_start, byte_end);
                if let Some(turn) = self.transcript.turns.last_mut() {
                    for block in &mut turn.blocks {
                        if let Block::Tool(tool) = block {
                            if tool.name == "exec" && tool.done && !tool.is_error {
                                tool.result_committed = true;
                                if tool
                                    .result
                                    .as_ref()
                                    .is_some_and(|result| !result.is_empty())
                                {
                                    tool.result_availability = ResultAvailability::Available;
                                }
                                tool.result = None;
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
                self.transcript.merge_last_turn_range(byte_start, byte_end);
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
        // `UserShellStart` (live) opens the standalone turn; a finished
        // `UserShell` does too unless it finalizes an already-streamed one.
        let starts_standalone_turn = match &ev {
            AgentEvent::UserShellStart { .. } => true,
            AgentEvent::UserShell { .. } => !user_shell_running(&self.transcript.turns),
            _ => false,
        };
        if starts_standalone_turn {
            self.transcript.freeze_previous_file_backed_turn();
        }
        let previous_turns = self.transcript.turns.len();
        let native_end = match &ev {
            AgentEvent::NativeToolEnd { parent, id, .. } => Some((parent.clone(), *id)),
            _ => None,
        };
        apply_event_to_turns(&mut self.transcript.turns, ev);
        if let Some((parent, id)) = native_end {
            if let Some(native) = self
                .transcript
                .turns
                .last_mut()
                .and_then(|turn| tool_mut(&mut turn.blocks, &parent))
                .and_then(|tool| tool.native.iter_mut().find(|native| native.id == id))
            {
                view::blocks::cache_native_preview(native);
            }
        }
        if starts_standalone_turn {
            debug_assert_eq!(self.transcript.turns.len(), previous_turns + 1);
            self.transcript.turn_byte_ranges.push(None);
            self.transcript.turn_event_offsets.push(None);
            // The output of a direct `!cmd` is the point of running it:
            // show it expanded instead of hiding it behind a detail toggle.
            // Covers the live event and the file-backed replay paths, both
            // of which flow through here.
            self.expanded_details.insert(
                DetailKey::UserShell(detail_block_id(previous_turns, 0)),
                DetailState {
                    turn: previous_turns,
                    scroll: None,
                },
            );
        }
    }

    pub(super) fn run_finished(&mut self) {
        if let Some(turn) = self.transcript.turns.last_mut() {
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
        self.run_model_label = None;
        self.retry = None;
        self.pending_prompt_start = false;
    }

    pub(super) fn reset_compaction_gauges(&mut self) {
        self.status_usage = None;
        self.settled_usage_fresh = false;
        self.compacted = false;
    }

    /// Push a submitted prompt immediately instead of waiting for the
    /// engine's `Prompt` event, so it renders before the working indicator's
    /// first frame.
    pub(super) fn begin_prompt_turn(&mut self, prompt: String, kind: lofi_types::PromptKind) {
        self.transcript.freeze_previous_file_backed_turn();
        self.transcript.push_turn(Turn {
            prompt,
            kind,
            blocks: Vec::new(),
        });
        self.pending_prompt_start = true;
    }

    pub(super) fn run_active(&self) -> bool {
        self.run.is_some()
    }

    pub(super) fn busy(&self) -> bool {
        self.run_active() || self.lifecycle_busy
    }

    pub(super) fn spinner_frame(&self) -> usize {
        self.run.unwrap_or(0)
    }

    pub(super) fn run_label(&self) -> String {
        self.run_model_label
            .clone()
            .unwrap_or_else(|| self.session_model())
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
        if self.transcript.turns.is_empty() {
            self.transcript.push_turn(Turn {
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
        let last = self.transcript.turns.len().saturating_sub(1);
        let hydrated = (*self.materialize_turn(last)).clone();
        if let Some(turn) = self.transcript.turns.get_mut(last) {
            turn.blocks = hydrated.blocks;
        }
        self.apply_event(event);
        self.bump_render_epoch();
        self.debug_sample("compaction");
    }

    fn start_lifecycle_operation(&mut self, operation: LifecycleOperation) -> bool {
        let Some(tx) = self.lifecycle_tx.clone() else {
            return false;
        };
        if self.run_active() {
            self.notify(NotifyKind::Warn, "wait for the running turn to finish");
            return true;
        }
        if self.lifecycle_busy {
            self.notify(NotifyKind::Warn, "wait for the memory operation to finish");
            return true;
        }
        self.lifecycle_busy = true;
        let mut lifecycle = self.lifecycle.clone();
        let cursor = self.session.cursor.clone();
        let system_prompt = self.system_prompt.clone();
        let task = tokio::task::spawn_blocking(move || match operation {
            LifecycleOperation::Compact => {
                LifecycleResult::Compact(lifecycle.compact(cursor.as_ref(), &system_prompt))
            }
            LifecycleOperation::AutoCompact(usage) => LifecycleResult::AutoCompact(
                lifecycle.auto_compact(usage, cursor.as_ref(), &system_prompt),
            ),
            LifecycleOperation::HardCompact => LifecycleResult::HardCompact(
                lifecycle.hard_compact(cursor.as_ref(), &system_prompt),
            ),
            LifecycleOperation::Recall(line) => LifecycleResult::Recall {
                result: lifecycle.recall_line(cursor.as_ref(), &line),
                line,
            },
        });
        tokio::task::spawn_local(async move {
            let result = task.await.unwrap_or_else(|error| {
                LifecycleResult::Failed(format!("memory operation failed: {error}"))
            });
            let _ = tx.send(result);
        });
        true
    }

    fn present_compact_result(&mut self, result: Result<Option<lofi_core::Compaction>>) -> bool {
        match result {
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

    /// Request an immediate core-owned compaction and render its outcome.
    pub(super) fn compact_now(&mut self) -> bool {
        if self.start_lifecycle_operation(LifecycleOperation::Compact) {
            return true;
        }
        let cursor = self.session.cursor.clone();
        let system_prompt = self.system_prompt.clone();
        let result = self.lifecycle.compact(cursor.as_ref(), &system_prompt);
        self.present_compact_result(result)
    }

    fn present_recall_result(
        &mut self,
        line: &str,
        result: Result<Option<lofi_core::recall::RecallOutcome>>,
    ) {
        match result {
            Ok(Some(outcome)) => {
                self.transcript.push_turn(Turn {
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

    /// Request core-owned recall and render the read-only result.
    pub(super) fn recall_now(&mut self, line: &str) {
        if self.start_lifecycle_operation(LifecycleOperation::Recall(line.to_string())) {
            return;
        }
        let cursor = self.session.cursor.as_ref();
        let result = self.lifecycle.recall_line(cursor, line);
        self.present_recall_result(line, result);
    }

    /// Let core evaluate soft-compaction policy; the UI only renders outcomes.
    pub(super) fn maybe_auto_compact(&mut self) {
        if !std::mem::take(&mut self.settled_usage_fresh) {
            return;
        }
        let Some(usage) = self.status_usage else {
            return;
        };
        if self.start_lifecycle_operation(LifecycleOperation::AutoCompact(usage)) {
            return;
        }
        let cursor = self.session.cursor.clone();
        let system_prompt = self.system_prompt.clone();
        let result = self
            .lifecycle
            .auto_compact(usage, cursor.as_ref(), &system_prompt);
        if let Ok(Some(compaction)) = result {
            self.render_compaction(&compaction);
        } else if let Err(error) = result {
            self.notify(NotifyKind::Error, format!("could not compact: {error}"));
        }
    }

    pub(super) fn hard_compact(&mut self) -> Option<HardCompactOutcome> {
        if self.start_lifecycle_operation(LifecycleOperation::HardCompact) {
            return None;
        }
        let cursor = self.session.cursor.clone();
        let system_prompt = self.system_prompt.clone();
        let result = self.lifecycle.hard_compact(cursor.as_ref(), &system_prompt);
        Some(self.present_hard_compact_result(result))
    }

    fn present_hard_compact_result(
        &mut self,
        result: Result<HardCompactOutcome>,
    ) -> HardCompactOutcome {
        match result {
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

    pub(super) fn apply_lifecycle_result(&mut self, result: LifecycleResult) -> bool {
        self.lifecycle_busy = false;
        match result {
            LifecycleResult::Compact(result) => {
                self.present_compact_result(result);
                false
            }
            LifecycleResult::AutoCompact(result) => {
                match result {
                    Ok(Some(compaction)) => self.render_compaction(&compaction),
                    Ok(None) => {}
                    Err(error) => {
                        self.notify(NotifyKind::Error, format!("could not compact: {error}"));
                    }
                }
                false
            }
            LifecycleResult::Recall { line, result } => {
                self.present_recall_result(&line, result);
                false
            }
            LifecycleResult::HardCompact(result) => {
                match self.present_hard_compact_result(result) {
                    HardCompactOutcome::Compacted(_) => true,
                    HardCompactOutcome::Cooldown => {
                        self.notify(
                            NotifyKind::Warn,
                            "context exceeded the hard cap too soon after a compaction; cannot continue",
                        );
                        false
                    }
                    HardCompactOutcome::NotEnoughHistory => {
                        self.notify(
                            NotifyKind::Warn,
                            "could not compact at the hard cap; cannot continue",
                        );
                        false
                    }
                }
            }
            LifecycleResult::Failed(error) => {
                self.notify(NotifyKind::Error, error);
                false
            }
        }
    }
}
