#![allow(clippy::wildcard_imports)]

use super::*;

impl Agent {
    /// Seeds a fresh `[User]` history and drives [`Self::run_once`] in a loop.
    /// The system prompt is pinned on the durable transcript by the lifecycle
    /// before the first call here, so the engine never materializes it inline.
    /// If the receiver is dropped (the channel closes), the run exits gracefully.
    /// # Errors
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution failures that cannot be surfaced as a `ToolResult`.
    pub async fn run(&self, user_prompt: String, tx: Sender<AgentEvent>) -> Result<()> {
        let mut messages = vec![Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: user_prompt.clone(),
            }],
        }];
        if !emit(
            Some(&tx),
            AgentEvent::TurnStart {
                prompt: user_prompt.clone(),
            },
        )
        .await
        {
            return Ok(());
        }
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            let finished = match self
                .run_once_inner(&mut messages, Some(&tx), None, None, None, None)
                .await
            {
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

    /// Unlike [`run`](Self::run), the message history is owned by the caller
    /// (e.g. the interactive TUI) so follow-up prompts keep the full prior
    /// context — assistant turns, tool calls, and tool results — instead of
    /// starting fresh each time. The first call seeds the system prompt.
    /// The engine owns the turn timer, per-tool timers, and cost counter. On
    /// normal completion it emits a single [`AgentEvent::TurnEnd`] summarizing
    /// the turn and, when `commit` is given, appends the turn's messages plus
    /// timing/cost events to the transcript — making the engine the
    /// sole writer of the session log so a resumed session reconstructs
    /// identically to the live one.
    /// # Errors
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn run_continuation(
        &self,
        messages: &mut Vec<Message>,
        user_prompt: String,
        tx: Sender<AgentEvent>,
        session: Option<&crate::session::store::SessionCursor>,
        continuation: bool,
        cancel: Option<Arc<AtomicBool>>,
        preempt: Option<Arc<AtomicBool>>,
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
            // The system prompt is pinned on the durable transcript at each
            // context boundary (create, compact) and arrives via the restored
            // history — the engine never materializes it inline.
            messages.push(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: user_prompt }],
            });
            if !emit(
                Some(&tx),
                AgentEvent::TurnStart {
                    prompt: prompt_for_event,
                },
            )
            .await
            {
                return Ok(());
            }
        }
        let mut stats = TurnStats::new();
        let mut recorder =
            session.map(|cursor| SessionRecorder::new(cursor.clone(), self.run_model()));
        // `lofi.recall` streams the on-disk transcript through a lightweight
        // index instead of deserializing the whole append-only file. It still
        // sees compacted-away messages and abandoned branches when requested.
        let recall: Option<RecallFn> = session.map(|cursor| {
            let cursor = cursor.clone();
            Arc::new(move |req: &lofi_types::recall::RecallRequest| {
                crate::recall::recall_cursor(&cursor, req)
            }) as RecallFn
        });
        let result: Option<ResultFn> = session.map(|cursor| {
            let cursor = cursor.clone();
            Arc::new(move |id: &str| -> String {
                match cursor.event_by_id(id) {
                    Ok(Some(event)) => match event.kind {
                        lofi_types::SessionEventKind::Message(message) => {
                            crate::context_edit::recover_message_content(&message).unwrap_or_else(|| {
                                format!(
                                    "no recoverable content for event {id:?} (message held no elidable block)."
                                )
                            })
                        }
                        _ => format!(
                            "no recoverable content for event {id:?} (not a message event)."
                        ),
                    },
                    Ok(None) => format!("no recoverable content for event {id:?} (event not found)."),
                    Err(_) => "result: session file unreadable.".to_string(),
                }
            }) as ResultFn
        });
        let mut finished_normally = false;
        let mut cancelled = false;
        let mut detached = false;
        let mut context_pressure = false;
        let mut err: Option<Error> = None;
        let retry = self.retry;
        let mut retry_attempt = 0u32;
        loop {
            if tx.is_closed() {
                detached = true;
                break;
            }
            let round = self
                .run_once_inner(
                    &mut *messages,
                    Some(&tx),
                    Some(&mut stats),
                    recall.clone(),
                    result.clone(),
                    cancel.as_ref(),
                )
                .await;
            if round.is_ok() && retry_attempt > 0 {
                let _ = tx
                    .send(AgentEvent::RetryEnd {
                        success: true,
                        attempt: retry_attempt,
                        final_error: None,
                    })
                    .await;
                retry_attempt = 0;
            }
            match round {
                Ok(true) => {
                    finished_normally = true;
                    break;
                }
                Ok(false) if tx.is_closed() => {
                    detached = true;
                    break;
                }
                Ok(false) => {
                    // The assistant tool call and its results form a complete,
                    // provider-valid round. Persist that suffix now instead of
                    // retaining the entire long turn only in memory.
                    if let Some(recorder) = recorder.as_mut() {
                        let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
                        if let Some((byte_start, byte_end)) = recorder
                            .checkpoint(&messages[prev_len..], &stats.summary(elapsed_ms))?
                        {
                            if !tx.is_closed() {
                                let _ = tx
                                    .send(AgentEvent::RoundCommitted {
                                        byte_start,
                                        byte_end,
                                    })
                                    .await;
                            }
                        }
                    }
                    // Hard context cap: the round just completed (its tool
                    // result is in hand, so the latest turn is a matched
                    // tool cycle that compaction keeps verbatim). Stop before
                    // the next round would overflow the window and let the UI
                    // force-compact + continue.
                    if let Some(threshold) = self.hard_compact_threshold() {
                        let prompt_tokens =
                            stats.usage.input_tokens + stats.usage.cache_read_tokens;
                        if prompt_tokens > threshold {
                            context_pressure = true;
                            break;
                        }
                    }
                    // Preempt: the UI has a queued prompt and wants the
                    // current turn to end gracefully after this round so
                    // the queued prompt can be sent at the earliest
                    // opportunity. The round just completed (tool results
                    // are in hand), so the conversation is in a clean
                    // state for the next prompt to continue from.
                    if let Some(flag) = &preempt {
                        if flag.load(std::sync::atomic::Ordering::Relaxed) {
                            finished_normally = true;
                            break;
                        }
                    }
                }
                Err(Error::Cancelled) => {
                    // A set cancel flag is an explicit user interruption;
                    // a closed receiver is only a detached consumer. Keep
                    // those outcomes separate so Ctrl-C is durably recorded
                    // as aborted while broken output pipes stay silent.
                    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                        cancelled = true;
                    } else {
                        detached = true;
                    }
                    break;
                }
                Err(e) => {
                    if retry.can_retry(retry_attempt) && crate::retry::is_retryable_error(&e) {
                        retry_attempt += 1;
                        let delay = retry.delay_for(retry_attempt);
                        if messages.last().is_some_and(|m| m.role == Role::Assistant) {
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
                        // Cancellable backoff: stop promptly for Ctrl-C or a
                        // closed consumer instead of waiting out the retry
                        // delay and issuing another provider request.
                        let retry_ready = if let Some(flag) = cancel.as_ref() {
                            tokio::select! {
                                biased;
                                () = wait_for_cancel(flag) => false,
                                () = tx.closed() => false,
                                () = tokio::time::sleep(delay) => true,
                            }
                        } else {
                            tokio::select! {
                                biased;
                                () = tx.closed() => false,
                                () = tokio::time::sleep(delay) => true,
                            }
                        };
                        if !retry_ready {
                            if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
                                cancelled = true;
                            } else {
                                detached = true;
                            }
                            break;
                        }
                        continue;
                    }
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
        let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
        let outcome = if finished_normally {
            Some(TurnOutcome::Finished)
        } else if context_pressure {
            Some(TurnOutcome::ContextPressure)
        } else if let Some(e) = &err {
            Some(TurnOutcome::Failed(e.to_string()))
        } else if cancelled {
            Some(TurnOutcome::Cancelled)
        } else if detached {
            Some(TurnOutcome::Detached)
        } else {
            None
        };
        if let Some(outcome) = &outcome {
            if !tx.is_closed() {
                let _ = match outcome {
                    TurnOutcome::Finished => {
                        tx.send(AgentEvent::TurnEnd {
                            model: self.run_model(),
                            elapsed_ms,
                            cost: stats.cost,
                            usage: stats.usage,
                        })
                        .await
                    }
                    TurnOutcome::Failed(error) => {
                        tx.send(AgentEvent::TurnFailed {
                            model: self.run_model(),
                            elapsed_ms,
                            error: error.clone(),
                            cost: stats.cost,
                            usage: stats.usage,
                        })
                        .await
                    }
                    TurnOutcome::ContextPressure => {
                        tx.send(AgentEvent::ContextPressure {
                            elapsed_ms,
                            cost: stats.cost,
                            usage: stats.usage,
                        })
                        .await
                    }
                    TurnOutcome::Cancelled => {
                        tx.send(AgentEvent::TurnCancelled {
                            model: self.run_model(),
                            elapsed_ms,
                            cost: stats.cost,
                            usage: stats.usage,
                        })
                        .await
                    }
                    TurnOutcome::Detached => Ok(()),
                };
            }
        }
        if let Some(recorder) = recorder.as_mut() {
            let summary = stats.summary(elapsed_ms);
            let flush_outcome = outcome.clone().unwrap_or(TurnOutcome::Detached);
            match recorder.flush(&messages[prev_len..], &flush_outcome, &summary) {
                Ok(Some((byte_start, byte_end))) if !tx.is_closed() => {
                    let _ = tx
                        .send(AgentEvent::TurnCommitted {
                            byte_start,
                            byte_end,
                        })
                        .await;
                }
                _ => {}
            }
        }
        if matches!(&outcome, Some(TurnOutcome::Failed(_))) {
            // `TurnFailed` is a durable display boundary: replay keeps the
            // failed branch visible but context rebuild excludes it. User
            // cancellation deliberately does not enter this path: an aborted
            // assistant message stays in context for the next prompt.
            messages.truncate(prev_len);
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
    /// `user_prompt` is unused (the continuation appends no user message);
    /// it exists only so this can reuse [`run_continuation`].
    /// # Errors
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution (via [`run_continuation`]).
    pub async fn run_continue(
        &self,
        messages: &mut Vec<Message>,
        tx: Sender<AgentEvent>,
        session: Option<&crate::session::store::SessionCursor>,
        cancel: Option<Arc<AtomicBool>>,
        preempt: Option<Arc<AtomicBool>>,
    ) -> Result<()> {
        self.run_continuation(messages, String::new(), tx, session, true, cancel, preempt)
            .await
    }

    /// # Errors
    /// Propagates [`Error`] from provider streaming or timeouts.
    pub async fn run_once(&self, messages: &mut Vec<Message>) -> Result<bool> {
        self.run_once_inner(messages, None, None, None, None, None)
            .await
    }

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
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<bool> {
        let schema = exec_tool_schema();
        let mut model = self.model.clone();
        if let Some(mt) = self.max_output_tokens {
            model.max_tokens = Some(mt);
        }
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        let schemas = [schema];
        let stream = match cancel {
            Some(flag) => {
                tokio::select! {
                    biased;
                    () = wait_for_cancel(flag) => return Err(Error::Cancelled),
                    stream = self.provider.stream(&model, messages, &schemas) => stream?,
                }
            }
            None => self.provider.stream(&model, messages, &schemas).await?,
        };
        let mut stream = stream;

        let mut assembler = MessageAssembler::new();
        // Round usage is captured and emitted as `AgentEvent::Done` only when
        // the round is terminal (no tool calls), so `Done` remains a true
        // end-of-run signal rather than firing before tool execution.
        let mut round_usage: Option<Usage> = None;
        let mut round_bytes = 0usize;
        let mut tool_raw: HashMap<String, String> = HashMap::new();
        let mut tool_emitted: HashMap<String, usize> = HashMap::new();
        let mut tool_decoders: HashMap<String, CodePrefixDecoder> = HashMap::new();
        let collect = async {
            let mut thinking_open: Option<Instant> = None;
            loop {
                let next = match cancel {
                    Some(flag) => {
                        tokio::select! {
                            biased;
                            () = wait_for_cancel(flag) => return Err(Error::Cancelled),
                            next = tokio::time::timeout(
                                DEFAULT_STREAM_IDLE_TIMEOUT,
                                stream.next(),
                            ) => next,
                        }
                    }
                    None => tokio::time::timeout(DEFAULT_STREAM_IDLE_TIMEOUT, stream.next()).await,
                };
                match next {
                    Err(_) => return Err(Error::Provider("stream idle timeout".into())),
                    Ok(None) => break,
                    Ok(Some(ev)) => match ev {
                        Ok(e) => {
                            let terminal = matches!(e, StreamingEvent::Done(_));
                            let is_thinking_ev = matches!(
                                e,
                                StreamingEvent::ThinkingDelta(_)
                                    | StreamingEvent::ThinkingSignature(_)
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
                                    let decoded =
                                        tool_decoders.entry(id.clone()).or_default().update(raw);
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
                            assembler.push(e);
                            if terminal {
                                break;
                            }
                        }
                        Err(err) => {
                            return Err(err);
                        }
                    },
                }
            }
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

        if let Err(error) = collect.await {
            // The live UI has already rendered every accepted delta. Preserve
            // that same partial assistant message on failed/cancelled turns so
            // durable replay cannot make visible output disappear. The
            // terminal outcome decides context semantics: failures
            // are rolled back, while explicit user cancellation retains the
            // partial assistant message.
            let partial = assembler.finish();
            if !partial.blocks.is_empty() {
                messages.push(partial);
            }
            return Err(error);
        }

        let assistant_index = messages.len();
        messages.push(assembler.finish());
        let results = {
            let assistant = &messages[assistant_index];
            let tool_uses: Vec<(&str, &str, &serde_json::Value)> = assistant
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.as_str(), name.as_str(), input))
                    }
                    _ => None,
                })
                .collect();

            if tool_uses.is_empty() {
                return Ok(true);
            }

            self.execute_tools(
                &tool_uses,
                tx,
                stats,
                recall.clone(),
                result.clone(),
                cancel,
            )
            .await?
        };

        messages.push(Message {
            role: Role::Tool,
            blocks: results,
        });
        Ok(false)
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_tools(
        &self,
        tool_uses: &[(&str, &str, &serde_json::Value)],
        tx: Option<&Sender<AgentEvent>>,
        mut stats: Option<&mut TurnStats>,
        recall: Option<RecallFn>,
        result: Option<ResultFn>,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<Vec<ContentBlock>> {
        let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        // Native tool events are emitted from a *sync* `on_tool_event`
        // callback inside the sandbox, so they can't `await` on the bounded
        // UI channel. `try_send` would drop events when the channel fills
        // (e.g. a `Promise.all` burst of Start/End events), leaving native
        // tiles stuck "running" even after the exec succeeds. Relay them
        // through an unbounded channel whose forwarder drains onto the
        // bounded channel with `send().await` — lossless and order-preserving.
        let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
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
            let native_tx = relay_tx.clone();
            let parent = id.to_string();
            let native_pending: Arc<Mutex<HashMap<u64, (String, String)>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let native_completed: Arc<Mutex<Vec<NativeToolRecord>>> =
                Arc::new(Mutex::new(Vec::new()));
            let on_tool_event: Arc<dyn Fn(ToolEvent) + Send + Sync> = {
                let native_pending = native_pending.clone();
                let native_completed = native_completed.clone();
                let event_parent = parent.clone();
                Arc::new(move |ev: ToolEvent| match ev {
                    ToolEvent::Start { id, name, args } => {
                        lock(&native_pending).insert(id, (name.clone(), args.clone()));
                        let _ = native_tx.send(AgentEvent::NativeToolStart {
                            parent: event_parent.clone(),
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
                                parent: event_parent.clone(),
                                call_id: id,
                                name,
                                args,
                                result: result.clone(),
                                is_error,
                            });
                        }
                        let _ = native_tx.send(AgentEvent::NativeToolEnd {
                            parent: event_parent.clone(),
                            id,
                            result,
                            is_error,
                        });
                    }
                }) as Arc<dyn Fn(ToolEvent) + Send + Sync>
            };
            let confirm: Option<lofi_code::ConfirmFn> = self.confirm_tx.as_ref().map(|tx| {
                let tx = tx.clone();
                let counter = self.confirm_counter.clone();
                Arc::new(move |prompt: lofi_code::ConfirmPrompt| {
                    let tx = tx.clone();
                    let counter = counter.clone();
                    Box::pin(async move {
                        let id = counter.fetch_add(1, Ordering::SeqCst);
                        let (resp_tx, resp_rx) = oneshot::channel();
                        let req = ConfirmRequest {
                            id,
                            command: prompt.command,
                            reason: prompt.reason,
                            active: prompt.active,
                            respond: resp_tx,
                        };
                        if tx.send(req).is_err() {
                            return false;
                        }
                        resp_rx.await.unwrap_or(false)
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + Sync>>
                }) as lofi_code::ConfirmFn
            });
            let exec_ctx = ExecCtx {
                root: self.root.clone(),
                tmp_dir: self.tmp_dir.clone(),
                strings,
                recall: recall.clone(),
                result: result.clone(),
                on_tool_event: Some(on_tool_event),
                bash_env: self.bash_env.clone(),
                shell_policy: self.shell_policy.clone(),
                confirm,
                auto_mode: self.auto_mode.clone(),
                skills_dir: self.skills_dir.clone(),
            };
            let outcome = exec(
                &code,
                &exec_ctx,
                &ExecOptions {
                    timeout: lofi_code::DEFAULT_GUEST_TIMEOUT,
                    cancel: cancel.cloned(),
                },
            )
            .await;
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
            if is_error {
                // A rejected guest promise can short-circuit concurrent native
                // calls before their futures emit `ToolEvent::End`. Close every
                // still-pending row explicitly so the live UI cannot leave a
                // spinner behind after the parent exec has already failed.
                // These synthetic cancellations are also persisted, making
                // resume reproduce the settled state.
                let pending = {
                    let mut pending = lock(&native_pending);
                    pending.drain().collect::<Vec<_>>()
                };
                for (call_id, (name, args)) in pending {
                    let result = "cancelled because parent exec failed".to_string();
                    if let Some(s) = stats.as_deref_mut() {
                        s.native_tools.push(NativeToolRecord {
                            parent: parent.clone(),
                            call_id,
                            name,
                            args,
                            result: result.clone(),
                            is_error: true,
                        });
                    }
                    let _ = relay_tx.send(AgentEvent::NativeToolEnd {
                        parent: parent.clone(),
                        id: call_id,
                        result,
                        is_error: true,
                    });
                }
            }
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
}
