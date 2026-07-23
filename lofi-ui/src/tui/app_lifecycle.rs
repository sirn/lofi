#![allow(clippy::wildcard_imports)]

use super::*;

impl App {
    pub(super) fn new(
        model_label: String,
        thinking: ThinkingLevel,
        ctx_limit: u64,
        compaction: lofi_types::CompactionConfig,
    ) -> Self {
        let thinking_label = (thinking != ThinkingLevel::Off)
            .then(|| format!(":{}", thinking.as_str()));
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
            ctx_limit: if ctx_limit > 0 { ctx_limit } else { DEFAULT_CTX_LIMIT },
            compaction,
            prev_ctx_tokens: None,
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
            yank_notify: None,
            notify: None,
            nav_cursor: 0,
            nav_col: 0,
            select_anchor: (0, 0),
            log_total: 0,
            last_turn_height: 0,
            log_view_h: 0,
            frozen_render: FrozenCache::new(),
            frozen_heights: Vec::new(),
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
            AgentEvent::RetryStart { attempt, max_attempts, delay_ms, error } => {
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
            AgentEvent::TurnCommitted { byte_start, byte_end } => {
                // The just-finished turn is now durably in the transcript
                // file over this byte range. Record it so the turn becomes
                // file-backed when the next prompt freezes it.
                if let Some(r) = self.turn_byte_ranges.last_mut() {
                    *r = Some((byte_start, byte_end));
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

    /// Invalidate the frozen-turn cache. Call whenever `turns` is replaced
    /// wholesale (resume, `/new`, `/clear`); incremental `push` does not need
    /// it — [`ensure_frozen`] freezes the newly-superseded turn on its own.
    pub(super) fn bump_render_epoch(&mut self) {
        self.render_epoch = self.render_epoch.wrapping_add(1);
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
        let bytes = {
            use std::io::{Read, Seek, SeekFrom};
            let Ok(mut f) = std::fs::File::open(path) else {
                return empty;
            };
            if f.seek(SeekFrom::Start(start)).is_err() {
                return empty;
            }
            let mut buf =
                Vec::with_capacity(usize::try_from(end - start).unwrap_or(0));
            if f.take(end - start).read_to_end(&mut buf).is_err() {
                return empty;
            }
            buf
        };
        let events: Vec<SessionEvent> = String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| lofi_core::session::store::parse_event(l).ok())
            .collect();
        turns_from_session_events(&events)
            .into_iter()
            .next()
            .unwrap_or(empty)
    }

    /// Ensure frozen turn `idx`'s rendered lines are in the bounded cache,
    /// materializing from `turns` or the transcript file on a miss.
    /// `ensure_frozen` must have already recorded the turn's height.
    pub(super) fn ensure_frozen_turn(&mut self, idx: usize, width: usize) {
        if !self.frozen_render.contains(idx) {
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
    }

    /// Sync the frozen-turn cache to the current `turns`. Frozen turns are all
    /// but the last (the last is the live, mutable one rebuilt each frame).
    /// On a wholesale replacement (`bump_render_epoch`) the cache is dropped;
    /// otherwise newly-superseded turns are rendered once, their height
    /// recorded permanently in `frozen_heights`, and their (heavy) styled
    /// lines entered into the bounded [`FrozenCache`] (oldest evicted). Heights
    /// are kept for every frozen turn so the viewport can be located and the
    /// scroll total computed without holding all rendered lines in memory.
    /// A viewport resize (width change) also drops the cache, since wrapping
    /// and background padding depend on width.
    pub(super) fn ensure_frozen(&mut self, width: usize) {
        if self.frozen_epoch != self.render_epoch || self.frozen_width != width {
            self.frozen_render.clear();
            self.frozen_heights.clear();
            self.frozen_epoch = self.render_epoch;
            self.frozen_width = width;
        }
        let n = self.turns.len();
        let target = n.saturating_sub(1);
        while self.frozen_heights.len() < target {
            let idx = self.frozen_heights.len();
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
            self.frozen_heights.push(lines.len());
            self.frozen_render.insert(idx, lines);
        }
        // Defensive: turns shrank without an epoch bump.
        if self.frozen_heights.len() > target {
            self.frozen_heights.truncate(target);
            // Drop any cached entries beyond the new frozen range.
            self.frozen_render
                .map
                .retain(|idx, _| *idx < target);
            self.frozen_render.order.retain(|idx| *idx < target);
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
        let opts = CompactOptions {
            max_kept_tokens: 0,
            edit: self.compaction.edit.clone(),
        };
        let Some(c) = compact(&events, &opts) else {
            self.notify(NotifyKind::Warn, "not enough history to compact yet");
            return false;
        };
        let new_history = compacted_history(&c);
        if let Ok(mut g) = self.history.lock() {
            *g = new_history;
        }
        // Persist the marker so resume rebuilds the compacted history. The
        // marker chains off the active leaf; subsequent turns chain off it.
        if let Some(path) = self.session.path.clone() {
            if let Some(first_kept) = c.first_kept_event_id.clone() {
                let mut ev = SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: SessionEventKind::Compaction {
                        summary: c.summary.clone(),
                        first_kept_entry_id: first_kept,
                        summarized_range: c.summarized_range.unwrap_or_default(),
                        summarized: c.summarized_count,
                        kept: c.kept_count,
                    },
                };
                let _ = store::append_events(&path, std::slice::from_mut(&mut ev), None);
            }
        }
        // Render the marker. Attach to the last turn when one exists; push a
        // fresh turn otherwise (e.g. compaction invoked before any turn).
        if self.turns.is_empty() {
            self.push_turn(Turn {
                prompt: String::new(),
                blocks: vec![Block::Compaction { summarized: c.summarized_count, kept: c.kept_count, summary: c.summary.clone() }],
            });
        } else {
            self.apply_event(AgentEvent::Compaction { summarized: c.summarized_count, kept: c.kept_count, summary: c.summary.clone() });
        }
        // The context gauge's last reading reflects the pre-compaction fill;
        // drop it so the auto-trigger does not re-fire on the same crossing
        // and the gauge waits for the next round's real (smaller) usage.
        self.status_usage = None;
        self.prev_ctx_tokens = None;
        self.bump_render_epoch();
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

        let Some(events) = self.compaction_events() else {
            self.notify(NotifyKind::Warn, "no session history yet");
            return;
        };
        let outcome = recall(&events, &req);
        // Render inline as a read-only turn so the result lives in the log
        // alongside the conversation; the prompt line echoes the invocation.
        let prompt = format!("/recall{}{}", if rest.is_empty() { String::new() } else { " ".to_string() }, rest);
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
        let Some(usage) = self.status_usage else { return };
        let limit = self.ctx_limit.max(DEFAULT_CTX_LIMIT);
        let Some(threshold) = self.compaction.soft_threshold(limit) else { return };
        let current = usage.input_tokens;
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
        let Some(events) = self.compaction_events() else { return usize::MAX };
        let path = store::active_path_from_leaf(&events);
        // Walk root-first; start counting only after the last Compaction
        // marker on the path (the newest one, which is closest to the leaf).
        let mut counting = true;
        let mut n = 0usize;
        for &i in &path {
            match &events[i].kind {
                SessionEventKind::Compaction { .. } => {
                    counting = true;
                    n = 0;
                }
                SessionEventKind::Message(m) if counting && m.role == Role::Assistant => {
                    n += 1;
                }
                _ => {}
            }
        }
        n
    }

    /// Gather the active-path events for compaction: load the transcript
    /// (so native tool records are available) when a session file exists,
    /// otherwise synthesize a linear event log from the in-memory history.
    /// Returns None when the history is empty.
    fn compaction_events(&self) -> Option<Vec<SessionEvent>> {
        if let Some(path) = &self.session.path {
            return store::load(path).ok().map(|(_meta, events, _off, _size)| events);
        }
        let msgs = self.history.lock().ok()?;
        if msgs.is_empty() {
            return None;
        }
        let mut events = Vec::with_capacity(msgs.len());
        for (i, m) in msgs.iter().enumerate() {
            events.push(SessionEvent {
                id: i.to_string(),
                parent_id: if i == 0 { None } else { Some((i - 1).to_string()) },
                kind: SessionEventKind::Message(m.clone()),
            });
        }
        Some(events)
    }
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
        .find_map(|t| t.strip_prefix("page:").and_then(|n| n.parse::<usize>().ok()))
        .unwrap_or(1)
        .max(1)
}