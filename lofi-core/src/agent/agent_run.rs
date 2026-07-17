#![allow(clippy::wildcard_imports)]

use super::*;

impl Agent {
    /// Run the interactive loop for a single user prompt, streaming events to
    /// `tx` until the assistant finishes a turn with no tool calls.
    ///
    /// Builds the initial `system` + `user` message history, then drives
    /// [`Self::run_once`] in a loop. If the receiver is dropped (the channel
    /// closes), the run exits gracefully.
    ///
    /// # Errors
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution failures that cannot be surfaced as a `ToolResult`.
    pub async fn run(&self, user_prompt: String, tx: Sender<AgentEvent>) -> Result<()> {
        let mut messages = initial_history(&self.system_prompt, &user_prompt);
        if !emit(Some(&tx), AgentEvent::TurnStart { prompt: user_prompt.clone() }).await {
            return Ok(());
        }
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            let finished = match self.run_once_inner(&mut messages, Some(&tx), None, None, None).await {
                Ok(f) => f,
                // A gone receiver is a graceful cancellation, not a provider
                // error: stop the run cleanly instead of surfacing it.
                Err(Error::Cancelled) => return Ok(()),
                Err(e) => return Err(e),
            };
            if finished || tx.is_closed() {
                return Ok(());
            }
        }
    }

    /// Run one user turn appended to an existing conversation history.
    ///
    /// Unlike [`run`](Self::run), the message history is owned by the caller
    /// (e.g. the interactive TUI) so follow-up prompts keep the full prior
    /// context — assistant turns, tool calls, and tool results — instead of
    /// starting fresh each time. The first call seeds the system prompt.
    ///
    /// The engine owns the turn timer, per-tool timers, and cost counter. On
    /// normal completion it emits a single [`AgentEvent::TurnEnd`] summarizing
    /// the turn and, when `commit` is given, appends the turn's messages plus
    /// timing/cost events to the transcript — making the engine the
    /// sole writer of the session log so a resumed session reconstructs
    /// identically to the live one.
    ///
    /// # Errors
    ///
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution.
    // The continuation loop threads mutable round state (usage, byte budget,
    // event log) through one async dispatch; helpers would fan it out.
    #[allow(clippy::too_many_lines)]
    pub async fn run_continuation(
        &self,
        messages: &mut Vec<Message>,
        user_prompt: String,
        tx: Sender<AgentEvent>,
        commit: Option<&SessionCommit>,
        continuation: bool,
    ) -> Result<()> {
        let prev_len = messages.len();
        if continuation {
            // Force-continue after a hard-cap force-compact: resume the
            // loop on the compacted history (which ends in a tool result)
            // without appending a new user prompt, and signal the UI to
            // append to the current turn rather than push a new one.
            if !emit(Some(&tx), AgentEvent::TurnContinue).await {
                return Ok(());
            }
        } else {
            let prompt_for_event = user_prompt.clone();
            if messages.is_empty() {
                *messages = initial_history(&self.system_prompt, &user_prompt);
            } else {
                messages.push(Message {
                    role: Role::User,
                    blocks: vec![ContentBlock::Text {
                        text: user_prompt,
                    }],
                });
            }
            // Previously a `checkpoint = messages.len()` was captured here so a
            // cancel/receiver-drop could `messages.truncate(checkpoint)` and roll
            // back the partial turn. Failed and cancelled turns are now recorded
            // as branches (see `TurnOutcome`), so the caller's history is left in
            // place for the recorder to write — the active-path walk on resume
            // handles excluding the failed content from the agent's context.
            if !emit(Some(&tx), AgentEvent::TurnStart { prompt: prompt_for_event }).await {
                return Ok(());
            }
        }
        let mut stats = TurnStats::new();
        // `lofi.recall` reads the full on-disk transcript (including
        // compacted-away messages) fresh on each call, so the native tool
        // sees the same history `/recall` does. Built once from the commit
        // path; `None` for ephemeral runs (no session file).
        let recall: Option<RecallFn> = commit.map(|c| {
            let path = c.path.clone();
            Arc::new(move |req: &lofi_types::recall::RecallRequest| {
                match crate::session::store::load(&path) {
                    Ok((_meta, events, _off, _size)) => crate::recall::recall(&events, req),
                    Err(_) => lofi_types::recall::RecallOutcome {
                        text: "recall: session file unreadable.".to_string(),
                        status: "error".to_string(),
                    },
                }
            }) as RecallFn
        });
        let result: Option<ResultFn> = commit.map(|c| {
            let path = c.path.clone();
            Arc::new(move |id: &str| -> String {
                match crate::session::store::load(&path) {
                    Ok((_meta, events, _off, _size)) => {
                        match crate::context_edit::recover_event_content(&events, id) {
                            Some(s) => s,
                            None => format!(
                                "no recoverable content for event {id:?} (not a message event, or held no elidable block)."
                            ),
                        }
                    }
                    Err(_) => "result: session file unreadable.".to_string(),
                }
            }) as ResultFn
        });
        let mut finished_normally = false;
        let mut cancelled = false;
        let mut context_pressure = false;
        let mut err: Option<Error> = None;
        let retry = self.retry;
        let mut retry_attempt = 0u32;
        loop {
            if tx.is_closed() {
                break;
            }
            match self
                .run_once_inner(&mut *messages, Some(&tx), Some(&mut stats), recall.clone(), result.clone())
                .await
            {
                Ok(true) => {
                    finished_normally = true;
                    // A successful assistant response concludes any in-flight
                    // retry: a successful assistant response concludes any
                    // in-flight retry.
                    if retry_attempt > 0 {
                        let _ = tx
                            .send(AgentEvent::RetryEnd {
                                success: true,
                                attempt: retry_attempt,
                                final_error: None,
                            })
                            .await;
                    }
                    break;
                }
                Ok(false) if tx.is_closed() => {
                    // Receiver dropped mid-turn after a tool round. Record
                    // the partial turn as a failed branch.
                    cancelled = true;
                    break;
                }
                Ok(false) => {
                    // Hard context cap: the round just completed (its tool
                    // result is in hand, so the latest turn is a matched
                    // tool cycle that compaction keeps verbatim). Stop before
                    // the next round would overflow the window and let the UI
                    // force-compact + continue.
                    if let Some(threshold) = self.hard_compact_threshold() {
                        if stats.usage.input_tokens > threshold {
                            context_pressure = true;
                            break;
                        }
                    }
                }
                Err(Error::Cancelled) => {
                    // Abandoned by the user (Ctrl-C / receiver dropped). The
                    // rounds that ran still consumed tokens, so record the
                    // partial turn as a failed branch instead of dropping it.
                    // Fall through to the commit block, which emits a
                    // `TurnFailed` marker and writes the partial messages.
                    cancelled = true;
                    break;
                }
                Err(e) => {
                    // Transient provider errors are retried with exponential
                    // backoff: the failed
                    // assistant message is dropped and the round is restarted
                    // so the provider produces a fresh response.
                    if retry.can_retry(retry_attempt)
                        && crate::retry::is_retryable_error(&e)
                    {
                        retry_attempt += 1;
                        let delay = retry.delay_for(retry_attempt);
                        // Drop the partial assistant message the failed round
                        // appended (if any) so the retried round starts from a
                        // clean conversation tail.
                        if messages.last().is_some_and(|m| m.role == Role::Assistant)
                        {
                            messages.pop();
                        }
                        let _ = tx
                            .send(AgentEvent::RetryStart {
                                attempt: retry_attempt,
                                max_attempts: retry.max_retries,
                                delay_ms: delay.as_millis() as u64,
                                error: e.to_string(),
                            })
                            .await;
                        // Cancellable backoff: wait `delay` for the channel to
                        // stay open. If the receiver drops, roll back like a
                        // normal cancellation rather than driving a dead channel.
                        if tokio::time::timeout(delay, tx.closed()).await.is_err() {
                            // Delay elapsed and the channel is still open;
                            // proceed to the retry.
                        } else {
                            // The receiver dropped during the backoff.
                            // Treat as a user cancel: record the partial
                            // turn as a failed branch.
                            cancelled = true;
                            break;
                        }
                        continue;
                    }
                    // The error is not retryable or the budget is exhausted:
                    // emit the final failure summary if a retry was in flight.
                    if retry_attempt > 0 {
                        let _ = tx
                            .send(AgentEvent::RetryEnd {
                                success: false,
                                attempt: retry_attempt,
                                final_error: Some(e.to_string()),
                            })
                            .await;
                    }
                    err = Some(e);
                    break;
                }
            }
        }
        // Commit the turn's messages and tool timings. A terminal marker
        // (`TurnEnd` for success, `TurnFailed` for error/cancel) is written
        // and emitted when the turn produced something to record — failed
        // turns are now kept as branches so their consumed tokens are
        // honestly accounted for and the failed attempt is inspectable.
        let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
        let outcome = if finished_normally {
            Some(TurnOutcome::Finished)
        } else if context_pressure {
            Some(TurnOutcome::ContextPressure)
        } else if let Some(e) = &err {
            Some(TurnOutcome::Failed(e.to_string()))
        } else if cancelled {
            Some(TurnOutcome::Failed("cancelled".to_string()))
        } else {
            None
        };
        if let Some(outcome) = &outcome {
            if !tx.is_closed() {
                let _ = match outcome {
                    TurnOutcome::Finished => tx
                        .send(AgentEvent::TurnEnd {
                            model: self.run_model(),
                            elapsed_ms,
                            cost: stats.cost,
                            usage: stats.usage,
                        })
                        .await,
                    TurnOutcome::Failed(error) => tx
                        .send(AgentEvent::TurnFailed {
                            model: self.run_model(),
                            elapsed_ms,
                            error: error.clone(),
                            cost: stats.cost,
                            usage: stats.usage,
                        })
                        .await,
                    TurnOutcome::ContextPressure => tx
                        .send(AgentEvent::ContextPressure {
                            elapsed_ms,
                            cost: stats.cost,
                            usage: stats.usage,
                        })
                        .await,
                    TurnOutcome::Cancelled => Ok(()),
                };
            }
        }
        // The durable translation lives in `SessionRecorder`: hand it the
        // finalized message slice + a snapshot of the engine's accumulators
        // and it shapes/writes the `SessionEvent`s. The agent loop stays free
        // of on-disk-format concerns.
        if let Some(commit) = commit {
            let summary = stats.summary(elapsed_ms);
            // Branch off an explicit parent when the caller picked one (e.g.
            // resuming from a non-leaf entry in the tree picker); otherwise
            // append to the file's active leaf.
            let mut recorder = match commit.parent_hint.as_deref() {
                Some(id) => SessionRecorder::with_parent(
                    commit.path.clone(),
                    self.run_model(),
                    id.to_string(),
                ),
                None => SessionRecorder::new(commit.path.clone(), self.run_model()),
            };
            // A turn with no terminal outcome and no content writes nothing.
            let flush_outcome = outcome.clone().unwrap_or(TurnOutcome::Cancelled);
            match recorder.flush(&messages[prev_len..], &flush_outcome, &summary) {
                Ok(Some((byte_start, byte_end))) if !tx.is_closed() => {
                    let _ = tx
                        .send(AgentEvent::TurnCommitted { byte_start, byte_end })
                        .await;
                }
                _ => {}
            }
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Resume the agent loop on an existing (compacted) history without a new
    /// user prompt — the silent continuation after a hard-cap force-compact.
    /// The history is expected to end in a tool result, so the model picks up
    /// where it left off. Emits [`AgentEvent::TurnContinue`] at the start so
    /// the UI appends to the current turn instead of pushing a new one. The
    /// hard-cap check remains active, so a continued run that crosses the
    /// hard cap again triggers another `ContextPressure` (gated by the UI's
    /// `min_messages_between_hard_compacts` cooldown).
    ///
    /// `user_prompt` is unused (the continuation appends no user message);
    /// it exists only so this can reuse [`run_continuation`].
    ///
    /// # Errors
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution (via [`run_continuation`]).
    pub async fn run_continue(
        &self,
        messages: &mut Vec<Message>,
        tx: Sender<AgentEvent>,
        commit: Option<&SessionCommit>,
    ) -> Result<()> {
        self.run_continuation(messages, String::new(), tx, commit, true).await
    }

    /// A single provider round-trip: stream one assistant turn, append it to
    /// `messages`, execute any tool calls, and append a `user`-role message
    /// carrying the `ToolResult` blocks.
    ///
    /// Returns `Ok(true)` when the assistant turn had no tool calls (the loop
    /// should stop), or `Ok(false)` when a tool was invoked and the loop
    /// should continue. This is the primitive subagents call via
    /// [`RoundTrip`]; it emits no events.
    ///
    /// # Errors
    /// Propagates [`Error`] from provider streaming or timeouts.
    pub async fn run_once(&self, messages: &mut Vec<Message>) -> Result<bool> {
        self.run_once_inner(messages, None, None, None, None).await
    }

    /// Shared core of [`run_once`] with an optional event sender.
    ///
    /// Events are emitted via an awaited [`Sender::send`] so a slow receiver
    /// applies backpressure without dropping events; the outer
    /// [`run`](Self::run) loop checks `tx.is_closed()` to exit when the
    /// consumer is gone.
    // A single large async dispatch over streaming events; splitting it
    // across helpers would scatter the shared mutable round state (usage,
    // byte budget, event log) through many params and hurt readability more
    // than the line count hurts here.
    #[allow(clippy::too_many_lines)]
    async fn run_once_inner(
        &self,
        messages: &mut Vec<Message>,
        tx: Option<&Sender<AgentEvent>>,
        mut stats: Option<&mut TurnStats>,
        recall: Option<RecallFn>,
        result: Option<ResultFn>,
    ) -> Result<bool> {
        let schema = exec_tool_schema();
        // Effective output limit: an explicit agent override wins over the
        // model's static `max_tokens`, and is encoded per API by the IR
        // builders (previously OpenAI ignored it entirely).
        let mut model = self.model.clone();
        if let Some(mt) = self.max_output_tokens {
            model.max_tokens = Some(mt);
        }
        let stream = self.provider.stream(&model, messages, &[schema]).await?;
        let mut stream = stream;

        let mut events: Vec<StreamingEvent> = Vec::new();
        // Round usage is captured and emitted as `AgentEvent::Done` only when
        // the round is terminal (no tool calls), so `Done` remains a true
        // end-of-run signal rather than firing before tool execution.
        let mut round_usage: Option<Usage> = None;
        let mut round_bytes = 0usize;
        // Per tool-call accumulated raw input JSON and the byte length of the
        // `code` field already streamed to the UI, for live exec streaming.
        let mut tool_raw: HashMap<String, String> = HashMap::new();
        let mut tool_emitted: HashMap<String, usize> = HashMap::new();
        let collect = async {
            // Open thinking block timer: started on the first ThinkingDelta
            // of a run, closed when a non-thinking event arrives (or the
            // stream ends). Each closed duration is recorded in `stats` so it
            // can be persisted as a `ThinkingTiming` event.
            let mut thinking_open: Option<Instant> = None;
            loop {
                match tokio::time::timeout(
                    DEFAULT_STREAM_IDLE_TIMEOUT,
                    stream.next(),
                )
                .await
                {
                    Err(_) => return Err(Error::Provider("stream idle timeout".into())),
                    Ok(None) => break,
                    Ok(Some(ev)) => match ev {
                    Ok(e) => {
                        let is_thinking_ev = matches!(
                            e,
                            StreamingEvent::ThinkingDelta(_) | StreamingEvent::ThinkingSignature(_)
                        );
                        if !is_thinking_ev {
                            if let (Some(start), Some(s)) =
                                (thinking_open.take(), stats.as_deref_mut())
                            {
                                let elapsed = start.elapsed();
                                s.thinking_elapsed.push(elapsed);
                                if !emit(
                                    tx,
                                    AgentEvent::ThinkingEnd {
                                        elapsed_ms: elapsed.as_millis() as u64,
                                    },
                                )
                                .await
                                {
                                    return Err(Error::Cancelled);
                                }
                            }
                        }
                        match &e {
                            StreamingEvent::TextDelta(d) => {
                                if !emit(tx, AgentEvent::Text(d.clone())).await {
                                    return Err(Error::Cancelled);
                                }
                            }
                            StreamingEvent::ThinkingDelta(d) => {
                                if thinking_open.is_none() {
                                    thinking_open = Some(Instant::now());
                                }
                                if !emit(tx, AgentEvent::Thinking(d.clone())).await {
                                    return Err(Error::Cancelled);
                                }
                            }
                            StreamingEvent::ToolUseStart { id, name } => {
                                if let Some(s) = stats.as_deref_mut() {
                                    s.tool_start(id);
                                }
                                if !emit(
                                    tx,
                                    AgentEvent::ToolStart {
                                        id: id.clone(),
                                        name: name.clone(),
                                    },
                                )
                                .await
                                {
                                    return Err(Error::Cancelled);
                                }
                            }
                            StreamingEvent::ToolUseInputDelta { id, delta } => {
                                let raw = tool_raw.entry(id.clone()).or_default();
                                raw.push_str(delta);
                                let decoded = extract_code_prefix(raw);
                                let prev = tool_emitted.get(id).copied().unwrap_or(0);
                                if let Some(chunk) = decoded.get(prev..) {
                                    if !chunk.is_empty() {
                                        if !emit(
                                            tx,
                                            AgentEvent::ToolInputDelta {
                                                id: id.clone(),
                                                delta: chunk.to_string(),
                                            },
                                        )
                                        .await
                                        {
                                            return Err(Error::Cancelled);
                                        }
                                        tool_emitted.insert(id.clone(), decoded.len());
                                    }
                                }
                            }
                            StreamingEvent::Done(usage) => {
                                round_usage = Some(*usage);
                                if let Some(s) = stats.as_deref_mut() {
                                    s.add_usage(*usage, &self.model);
                                    // Emit the turn's cumulative cost and
                                    // this round's usage immediately so the
                                    // UI's context gauge and cost counter
                                    // refresh per round, not just at turn end.
                                    if !emit(
                                        tx,
                                        AgentEvent::RoundUsage {
                                            cost: s.cost,
                                            usage: s.usage,
                                        },
                                    )
                                    .await
                                    {
                                        return Err(Error::Cancelled);
                                    }
                                }
                            }
                            StreamingEvent::Error(msg) => {
                                if !emit(tx, AgentEvent::Error(msg.clone())).await {
                                    return Err(Error::Cancelled);
                                }
                                return Err(Error::Provider(msg.clone()));
                            }
                            _ => {}
                        }
                        // Bound retained round bytes so a stream of many
                        // small valid events cannot exhaust memory.
                        // Charge every owned string plus a fixed per-event
                        // allowance for Vec/enum/dispatch overhead, so large
                        // IDs/signatures or many zero-text events are caught.
                        round_bytes = round_bytes.saturating_add(PER_EVENT_OVERHEAD);
                        round_bytes = round_bytes.saturating_add(match &e {
                            StreamingEvent::TextDelta(s)
                            | StreamingEvent::ThinkingDelta(s)
                            | StreamingEvent::ThinkingSignature(s)
                            | StreamingEvent::Error(s) => s.len(),
                            StreamingEvent::ToolUseStart { id, name } => id.len() + name.len(),
                            StreamingEvent::ToolUseInputDelta { id, delta } => {
                                id.len() + delta.len()
                            }
                            StreamingEvent::ToolUseEnd { id } => id.len(),
                            StreamingEvent::Done(_) => 0,
                        });
                        if round_bytes > MAX_ROUND_BYTES {
                            return Err(Error::Provider(format!(
                                "round exceeded {MAX_ROUND_BYTES} byte budget"
                            )));
                        }
                        events.push(e);
                    }
                    Err(err) => {
                        return Err(err);
                    }
                    },
                }
            }
            // Stream ended: close any still-open thinking block.
            if let (Some(start), Some(s)) = (thinking_open.take(), stats.as_deref_mut()) {
                let elapsed = start.elapsed();
                s.thinking_elapsed.push(elapsed);
                if !emit(
                    tx,
                    AgentEvent::ThinkingEnd {
                        elapsed_ms: elapsed.as_millis() as u64,
                    },
                )
                .await
                {
                    return Err(Error::Cancelled);
                }
            }
            Ok(())
        };

        collect.await?;

        let assistant = assemble_message(&events);
        messages.push(assistant.clone());

        let tool_uses: Vec<(&str, &str, &serde_json::Value)> = assistant
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect();

        if tool_uses.is_empty() {
            // The turn-end marker (and cost/usage) is emitted once by the
            // outer `run_continuation` from the accumulated `TurnStats`, not
            // per round, so a multi-round turn produces a single summary.
            return Ok(true);
        }

        let results = self.execute_tools(&tool_uses, tx, stats, recall.clone(), result.clone()).await?;

        // Tool results travel under the dedicated Tool role: each provider
        // converter emits them from its Role::Tool arm (Chat Completions
        // role:"tool", Responses function_call_output, Anthropic
        // tool_result). A User role would be skipped — collect_text ignores
        // ToolResult blocks — and the next round would 400 with "No tool
        // output found for function call".
        messages.push(Message {
            role: Role::Tool,
            blocks: results,
        });
        Ok(false)
    }

    /// Execute each tool call in `tool_uses` against the sandbox, emitting
    /// [`AgentEvent::ToolInput`] (the code) and [`AgentEvent::ToolEnd`] (the
    /// result) per call, and return the `tool_result` blocks to append.
    #[allow(clippy::too_many_lines)]
    async fn execute_tools(
        &self,
        tool_uses: &[(&str, &str, &serde_json::Value)],
        tx: Option<&Sender<AgentEvent>>,
        mut stats: Option<&mut TurnStats>,
        recall: Option<RecallFn>,
        result: Option<ResultFn>,
    ) -> Result<Vec<ContentBlock>> {
        let agent_fn = self.make_agent_fn();
        let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        // Native tool events are emitted from a *sync* `on_tool_event`
        // callback inside the sandbox, so they can't `await` on the bounded
        // UI channel. `try_send` would drop events when the channel fills
        // (e.g. a `Promise.all` burst of Start/End events), leaving native
        // tiles stuck "running" even after the exec succeeds. Relay them
        // through an unbounded channel whose forwarder drains onto the
        // bounded channel with `send().await` — lossless and order-preserving.
        let (relay_tx, mut relay_rx) =
            tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let relay_dst = tx.cloned();
        let _forwarder = tokio::spawn(async move {
            while let Some(ev) = relay_rx.recv().await {
                match &relay_dst {
                    Some(dst) => {
                        if dst.send(ev).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        drop(ev);
                    }
                }
            }
        });
        for (id, name, input) in tool_uses {
            // Only `exec` is advertised; an unknown tool name is a protocol
            // error, not a silent empty success.
            if *name != "exec" {
                let content = format!("unknown tool: {name}");
                let elapsed_ms = stats.as_deref_mut().map_or(0, |s| s.tool_end(id));
                if !emit(
                    tx,
                    AgentEvent::ToolEnd {
                        id: id.to_string(),
                        result: content.clone(),
                        is_error: true,
                        elapsed_ms,
                    },
                )
                .await
                {
                    return Err(Error::Cancelled);
                }
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content,
                    is_error: true,
                });
                continue;
            }
            let (code, strings, display) = parse_exec_input(input);
            if code.is_empty() {
                let content = "exec: missing required 'code' argument".to_string();
                let elapsed_ms = stats.as_deref_mut().map_or(0, |s| s.tool_end(id));
                if !emit(
                    tx,
                    AgentEvent::ToolEnd {
                        id: id.to_string(),
                        result: content.clone(),
                        is_error: true,
                        elapsed_ms,
                    },
                )
                .await
                {
                    return Err(Error::Cancelled);
                }
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content,
                    is_error: true,
                });
                continue;
            }
            // The ToolInput event must land before we invoke the sandbox;
            // if the consumer is gone, stop instead of running tools (which
            // may shell out or mutate files) for a dead receiver.
            if !emit(
                tx,
                AgentEvent::ToolInput {
                    id: id.to_string(),
                    code: code.clone(),
                    label: exec_label(&display),
                },
            )
            .await
            {
                return Err(Error::Cancelled);
            }
            // Forward native tool-call events (one per `lofi.<tool>`
            // invocation inside the sandbox) to the UI under this exec id, and
            // capture each completed call so it can be written to the
            // transcript and restored on resume. `pending` holds a Start's
            // name/args until the matching End supplies the result.
            let native_tx = relay_tx.clone();
            let parent = id.to_string();
            let native_pending: Arc<Mutex<HashMap<u64, (String, String)>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let native_completed: Arc<Mutex<Vec<NativeToolRecord>>> =
                Arc::new(Mutex::new(Vec::new()));
            let on_tool_event: Arc<dyn Fn(ToolEvent) + Send + Sync> = {
                let native_pending = native_pending.clone();
                let native_completed = native_completed.clone();
                Arc::new(move |ev: ToolEvent| match ev {
                    ToolEvent::Start { id, name, args } => {
                        lock(&native_pending).insert(id, (name.clone(), args.clone()));
                        let _ = native_tx.send(AgentEvent::NativeToolStart {
                            parent: parent.clone(),
                            id,
                            name,
                            args,
                        });
                    }
                    ToolEvent::End {
                        id,
                        result,
                        is_error,
                    } => {
                        // Cap each native tool result individually so one huge
                        // `lofi.read`/`lofi.bash` can't monopolize the exec
                        // payload, and many concurrent calls each keep a
                        // truncated slice rather than the first few whole and
                        // the rest dropped by the outer cap.
                        let result = cap_tool_result(&result);
                        if let Some((name, args)) = lock(&native_pending).remove(&id) {
                            lock(&native_completed).push(NativeToolRecord {
                                parent: parent.clone(),
                                call_id: id,
                                name,
                                args,
                                result: result.clone(),
                                is_error,
                            });
                        }
                        let _ = native_tx.send(AgentEvent::NativeToolEnd {
                            parent: parent.clone(),
                            id,
                            result,
                            is_error,
                        });
                    }
                }) as Arc<dyn Fn(ToolEvent) + Send + Sync>
            };
            let exec_ctx = ExecCtx {
                root: self.root.clone(),
                tmp_dir: self.tmp_dir.clone(),
                strings,
                agent: Some(agent_fn.clone()),
                recall: recall.clone(),
                result: result.clone(),
                on_tool_event: Some(on_tool_event),
                bash_env: self.bash_env.clone(),
            };
            let outcome = exec(&code, &exec_ctx, &ExecOptions::default()).await;
            // Drain the native tool calls that completed inside this exec into
            // the turn stats, so they are persisted with the turn.
            let captured = lock(&native_completed).drain(..).collect::<Vec<_>>();
            if let Some(s) = stats.as_deref_mut() {
                s.native_tools.extend(captured);
            }
            let (content, is_error) = match outcome {
                Ok(r) => {
                    let payload = serde_json::json!({ "value": r.value, "logs": r.logs });
                    let content =
                        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
                    (cap_exec_result(&content), false)
                }
                Err(e) => (cap_exec_result(&e.to_string()), true),
            };
            let elapsed_ms = stats.as_deref_mut().map_or(0, |s| s.tool_end(id));
            if !emit(
                tx,
                AgentEvent::ToolEnd {
                    id: id.to_string(),
                    result: content.clone(),
                    is_error,
                    elapsed_ms,
                },
            )
            .await
            {
                return Err(Error::Cancelled);
            }
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content,
                is_error,
            });
        }
        Ok(results)
    }

    /// Build the `lofi.agent` / `lofi.spawn` callback used by [`ExecCtx`].
    ///
    /// The closure clones the agent (cheap — `Arc` provider) and runs
    /// [`subagent::run`] with a fresh nested loop reusing the same provider,
    /// model, and workspace root. The subagent's final assistant text becomes
    /// the `lofi.agent()` return value inside the sandbox.
    fn make_agent_fn(&self) -> AgentFn {
        let self_clone = self.clone();
        Arc::new(move |req: lofi_code::AgentRequest| {
            let agent = self_clone.clone();
            Box::pin(async move {
                // Honor a caller-supplied `timeoutMs` (per round-trip); fall
                // back to the default. A total deadline of 20× the per-round
                // timeout bounds runaway loops that complete within each call.
                let per_rt_ms = req
                    .opts
                    .as_ref()
                    .and_then(|o| o.get("timeoutMs").and_then(serde_json::Value::as_u64))
                    .map_or(2 * 60 * 1000, |ms| ms.min(MAX_SUBAGENT_TIMEOUT_MS));
                let per_rt = Duration::from_millis(per_rt_ms);
                // `checked_mul` so a large (clamped) per-round value cannot
                // overflow `Duration` multiplication (which panics in debug).
                let total = per_rt.checked_mul(20).unwrap_or(per_rt);
                let opts = SubagentOptions {
                    system: agent.system_prompt.clone(),
                    timeout: per_rt,
                    total_timeout: Some(total),
                };
                let parent = SubagentCtx {
                    root: agent.root.clone(),
                    strings: HashMap::new(),
                };
                subagent::run(&parent, &agent, &req.prompt, &opts).await
            })
        })
    }
}
