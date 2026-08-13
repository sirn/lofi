#![allow(clippy::wildcard_imports)]

use super::*;

/// Per-round tuneables that callers usually leave unset. Grouping them keeps
/// [`Agent::run_once_inner`] (and the public entry points that forward to it)
/// under the argument-count lint without a long `None`-studded call shape.
#[derive(Default, Clone)]
struct RoundOpts<'a> {
    recall: Option<RecallFn>,
    result: Option<ResultFn>,
    cancel: Option<&'a Arc<AtomicBool>>,
    prev_input_tokens: Option<u64>,
    /// Suppress the image-omit notice when true, so it fires once per turn
    /// at the continuation loop, not once per tool round in `run_once_inner`.
    suppress_omit_notice: bool,
    on_job_started: Option<lofi_code::JobStartedFn>,
    on_job_finished: Option<lofi_code::JobFinishedFn>,
}

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
            kind: lofi_types::PromptKind::User,
        }];
        if !emit(
            Some(&tx),
            AgentEvent::TurnStart {
                prompt: user_prompt.clone(),
                kind: lofi_types::PromptKind::User,
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
                .run_once_inner(&mut messages, Some(&tx), None, RoundOpts::default())
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
    /// Propagates [`Error`] from [`run_continuation_with_attachments`](Self::run_continuation_with_attachments).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_continuation(
        &self,
        messages: &mut Vec<Message>,
        user_prompt: String,
        prompt_kind: lofi_types::PromptKind,
        tx: Sender<AgentEvent>,
        session: Option<&crate::session::store::SessionCursor>,
        continuation: bool,
        cancel: Option<Arc<AtomicBool>>,
        preempt: Option<Arc<AtomicBool>>,
    ) -> Result<()> {
        self.run_continuation_with_attachments(
            messages,
            user_prompt,
            prompt_kind,
            Vec::new(),
            tx,
            session,
            continuation,
            cancel,
            preempt,
        )
        .await
    }

    /// [`run_continuation`](Self::run_continuation) plus attachment blocks
    /// (e.g. images) appended to the user message after its text. Attachments
    /// travel with the prompt into the durable transcript and the request
    /// history, so the send-time image guard and per-provider IR see them.
    /// # Errors
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution failures that cannot be surfaced as a `ToolResult`.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn run_continuation_with_attachments(
        &self,
        messages: &mut Vec<Message>,
        user_prompt: String,
        prompt_kind: lofi_types::PromptKind,
        attachments: Vec<ContentBlock>,
        tx: Sender<AgentEvent>,
        session: Option<&crate::session::store::SessionCursor>,
        continuation: bool,
        cancel: Option<Arc<AtomicBool>>,
        preempt: Option<Arc<AtomicBool>>,
    ) -> Result<()> {
        let prev_len = messages.len();
        let turn_start = if continuation {
            None
        } else {
            // Surface attachments on the turn header (TurnStart.prompt and,
            // via the recorder, the durable turn label) as a marker per image,
            // so the live and replayed views both show that an image was
            // attached. The image bytes themselves never appear in the label.
            let attachment_markers: Vec<String> = attachments
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Image { media_type, .. } => {
                        let short = media_type.strip_prefix("image/").unwrap_or(media_type);
                        Some(format!("[image: {short}]"))
                    }
                    _ => None,
                })
                .collect();
            let prompt_for_event = if attachment_markers.is_empty() {
                user_prompt.clone()
            } else if user_prompt.is_empty() {
                attachment_markers.join(" ")
            } else {
                format!("{}\n{}", user_prompt, attachment_markers.join(" "))
            };
            // The system prompt is pinned on the durable transcript at each
            // context boundary (create, compact) and arrives via the restored
            // history — the engine never materializes it inline.
            let mut blocks = vec![ContentBlock::Text { text: user_prompt }];
            blocks.extend(attachments);
            messages.push(Message {
                role: Role::User,
                blocks,
                kind: prompt_kind,
            });
            Some(prompt_for_event)
        };
        let mut stats = TurnStats::new();
        let mut recorder =
            session.map(|cursor| SessionRecorder::new(cursor.clone(), self.run_model()));
        if let Some(prompt) = turn_start {
            // Persist the user prompt before TurnStart so a closed consumer
            // cannot drop a submitted turn. Later checkpoints skip this suffix.
            commit_progress(recorder.as_mut(), &messages[prev_len..], &stats, &tx).await?;
            if !emit(
                Some(&tx),
                AgentEvent::TurnStart {
                    prompt,
                    kind: prompt_kind,
                },
            )
            .await
            {
                return Ok(());
            }
        } else if !emit(Some(&tx), AgentEvent::TurnContinue).await {
            return Ok(());
        }
        // JobStarted goes through the recorder's pending queue so its parent
        // chains off the current turn's events (not the pre-turn leaf). That
        // keeps the marker on the same lineage as the conversation that
        // spawned it: a /tree rollback to before the turn drops the marker
        // from the lineage, and reconcile kills the off-lineage job.
        // JobFinished fires when the job exits, which can be long after the
        // spawning turn ends — by then the recorder is dropped. Write it
        // directly to the cursor, chained off whatever the current leaf is.
        // Failures are swallowed: losing a marker never takes down a job.
        let on_job_started = recorder.as_ref().map(|r| {
            let queue = r.job_lifecycle_queue();
            std::sync::Arc::new(move |job_id: u64| {
                let mut q = queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                q.push(lofi_types::SessionEventKind::JobStarted { job_id });
            }) as lofi_code::JobStartedFn
        });
        let on_job_finished = session.map(|c| {
            let cursor = c.clone();
            std::sync::Arc::new(move |job_id: u64| {
                let mut events = [lofi_types::SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: lofi_types::SessionEventKind::JobFinished { job_id },
                }];
                let _ = cursor.append_events(&mut events);
            }) as lofi_code::JobFinishedFn
        });
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
        // The image-omit notice fires once per turn (on the first round),
        // not once per tool round.
        let mut omit_notice_sent = false;
        loop {
            if tx.is_closed() {
                detached = true;
                break;
            }
            // Feed the prior round's prompt size back in so the next request
            // clips its output cap against the remaining context window.
            let prev_input = Some(stats.usage.input_tokens + stats.usage.cache_read_tokens);
            let round = self
                .run_once_inner(
                    &mut *messages,
                    Some(&tx),
                    Some(&mut stats),
                    RoundOpts {
                        recall: recall.clone(),
                        result: result.clone(),
                        cancel: cancel.as_ref(),
                        prev_input_tokens: prev_input,
                        suppress_omit_notice: omit_notice_sent,
                        on_job_started: on_job_started.clone(),
                        on_job_finished: on_job_finished.clone(),
                    },
                )
                .await;
            omit_notice_sent = true;
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
                Ok(finished) => {
                    // Persist before inspecting the consumer so a completed
                    // round is on disk even if the UI already went away.
                    commit_progress(recorder.as_mut(), &messages[prev_len..], &stats, &tx).await?;
                    if finished {
                        finished_normally = true;
                        break;
                    }
                    if tx.is_closed() {
                        detached = true;
                        break;
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
                    // An oversized image payload stopped before send; route
                    // to the existing force-compact + continue recovery. The
                    // round never ran, so the partial-turn suffix is empty
                    // and the recorder's `ContextPressure => None` arm leaves
                    // the durable transcript untouched.
                    // Match the variant payload, not `Display`: `Provider`
                    // prefixes its message with "provider error: ".
                    if matches!(&e, Error::Provider(m) if m == IMAGE_PRESSURE_SENTINEL) {
                        context_pressure = true;
                        break;
                    }
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
        // Disk first: a crash after the UI terminal event must not lose a
        // settled turn that was still only in memory.
        let mut committed = None;
        if let Some(recorder) = recorder.as_mut() {
            let summary = stats.summary(elapsed_ms);
            let flush_outcome = outcome.clone().unwrap_or(TurnOutcome::Detached);
            match recorder.flush(&messages[prev_len..], &flush_outcome, &summary) {
                Ok(range) => committed = range,
                Err(e) if err.is_none() => return Err(e),
                Err(_) => {}
            }
        }
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
        if let Some((byte_start, byte_end)) = committed {
            if !tx.is_closed() {
                let _ = tx
                    .send(AgentEvent::TurnCommitted {
                        byte_start,
                        byte_end,
                    })
                    .await;
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
        self.run_continuation(
            messages,
            String::new(),
            lofi_types::PromptKind::User,
            tx,
            session,
            true,
            cancel,
            preempt,
        )
        .await
    }

    /// # Errors
    /// Propagates [`Error`] from provider streaming or timeouts.
    pub async fn run_once(&self, messages: &mut Vec<Message>) -> Result<bool> {
        self.run_once_inner(messages, None, None, RoundOpts::default())
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
        opts: RoundOpts<'_>,
    ) -> Result<bool> {
        let RoundOpts {
            recall,
            result,
            cancel,
            prev_input_tokens,
            suppress_omit_notice,
            on_job_started,
            on_job_finished,
        } = opts;
        let schema = exec_tool_schema();
        let mut model = self.model.clone();
        // Clip the output cap against remaining context so strict providers
        // (`max_tokens < context_window - input_tokens`) never reject the
        // request. `prev_input_tokens` is the prior round's usage, the best
        // in-hand estimate of this request's input.
        model.max_tokens = self.clipped_max_tokens(prev_input_tokens);
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        // Send-time guard: a model that does not support images cannot
        // receive them, so replace each Image block with a text marker and
        // warn the consumer once per turn (`suppress_omit_notice`). Runs at
        // send time, not attach time, so a `/model` switch mid-session is
        // honored — the model is re-read per request. The durable transcript
        // keeps the original image; only the request payload is stripped.
        let stripped: Option<Vec<Message>> = if model.supports_image {
            None
        } else {
            let omitted = count_image_blocks(messages);
            if omitted > 0 {
                if !suppress_omit_notice {
                    if let Some(tx) = tx {
                        let noun = if omitted == 1 { "image" } else { "images" };
                        let _ = tx
                            .send(AgentEvent::Notice(format!(
                                "{} does not support images; omitted {omitted} {noun} from this request",
                                model.id
                            )))
                            .await;
                    }
                }
                Some(strip_image_blocks(messages))
            } else {
                None
            }
        };
        let send_messages: &Vec<Message> = stripped.as_ref().unwrap_or(messages);
        // Send-time byte guard: the provider caps the request body, not just
        // the token count, so an image-heavy history can be rejected (HTTP
        // 413) while the token-threshold compaction never trips. Stop before
        // the doomed request and surface the sentinel; the run loop maps it
        // to `ContextPressure`, which force-compacts and continues. Runs at
        // send time (not after a round) because the oversized request is
        // usually the first one carrying fresh attachments.
        let image_bytes = image_payload_bytes(send_messages);
        if image_bytes > MAX_REQUEST_IMAGE_BYTES {
            if let Some(tx) = tx {
                let _ = tx
                    .send(AgentEvent::Notice(format!(
                        "attached images total {} MiB; compacting to fit the request size limit",
                        image_bytes / (1024 * 1024)
                    )))
                    .await;
            }
            return Err(Error::Provider(IMAGE_PRESSURE_SENTINEL.to_string()));
        }
        let schemas = [schema];
        let stream = match cancel {
            Some(flag) => {
                tokio::select! {
                    biased;
                    () = wait_for_cancel(flag) => return Err(Error::Cancelled),
                    stream = self.provider.stream(&model, send_messages, &schemas) => stream?,
                }
            }
            None => {
                self.provider
                    .stream(&model, send_messages, &schemas)
                    .await?
            }
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
            close_orphaned_tool_uses(messages);
            return Err(error);
        }

        let assistant_index = messages.len();
        messages.push(assembler.finish());
        // Collect the requested tool uses, then end the borrow so the rest
        // of the round (and any orphaned-close on early exit) can mutate
        // `messages`.
        let tool_uses: Vec<(String, String, serde_json::Value)> = {
            let assistant = &messages[assistant_index];
            assistant
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect()
        };

        if tool_uses.is_empty() {
            return Ok(true);
        }

        let tool_refs: Vec<(&str, &str, &serde_json::Value)> = tool_uses
            .iter()
            .map(|(id, name, input)| (id.as_str(), name.as_str(), input))
            .collect();
        let results = match self
            .execute_tools(
                &tool_refs,
                tx,
                stats,
                recall.clone(),
                result.clone(),
                cancel,
                on_job_started.clone(),
                on_job_finished.clone(),
            )
            .await
        {
            Ok(results) => results,
            Err(error) => {
                // Abandoning the turn here (cancel, closed channel, tool
                // error) leaves the pushed assistant message's `ToolUse`
                // blocks without matching results. Close the orphans so a
                // retained cancel partial stays provider-valid (Anthropic
                // 400s: "did not find any tool_result blocks"); a full failure
                // later truncates the whole turn, making this a no-op there.
                close_orphaned_tool_uses(messages);
                return Err(error);
            }
        };

        messages.push(Message {
            role: Role::Tool,
            blocks: results,
            kind: lofi_types::PromptKind::User,
        });
        Ok(false)
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn execute_tools(
        &self,
        tool_uses: &[(&str, &str, &serde_json::Value)],
        tx: Option<&Sender<AgentEvent>>,
        mut stats: Option<&mut TurnStats>,
        recall: Option<RecallFn>,
        result: Option<ResultFn>,
        cancel: Option<&Arc<AtomicBool>>,
        on_job_started: Option<lofi_code::JobStartedFn>,
        on_job_finished: Option<lofi_code::JobFinishedFn>,
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
                    images: Vec::new(),
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
                    images: Vec::new(),
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
                truncate: self.truncate,
                jobs: self.jobs.clone(),
                on_job_started: on_job_started.clone(),
                on_job_finished: on_job_finished.clone(),
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
            // A tool may return a tagged image (`read` on an image file).
            // Upgrade it to a `ToolResultImage` carried on this result block,
            // so the model sees the image in the same round as the result on
            // every provider. The heavyweight base64 is stripped out of the
            // text payload, leaving a compact `bytes` count in the transcript.
            let (content, is_error, result_images) = match outcome {
                Ok(r) => {
                    let mut value = r.value;
                    let img = upgrade_tagged_image(&mut value);
                    let payload = serde_json::json!({ "value": value, "logs": r.logs });
                    let content =
                        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
                    (cap_exec_result(&content), false, img)
                }
                Err(e) => (cap_exec_result(&e.to_string()), true, None),
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
                images: result_images.into_iter().collect(),
            });
        }
        Ok(results)
    }
}

async fn commit_progress(
    recorder: Option<&mut SessionRecorder>,
    messages: &[Message],
    stats: &TurnStats,
    tx: &tokio::sync::mpsc::Sender<AgentEvent>,
) -> Result<()> {
    let Some(recorder) = recorder else {
        return Ok(());
    };
    let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
    if let Some((byte_start, byte_end)) =
        recorder.checkpoint(messages, &stats.summary(elapsed_ms))?
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
    Ok(())
}

/// Enforces the pairing invariant: every trailing assistant `ToolUse` block
/// must be answered by a `ToolResult` before the message list is handed back
/// to the caller. Providers like the Anthropic Messages API reject an orphaned
/// `tool_use` (400 `"did not find any tool_result blocks"`), and the transcript
/// keeps cancelled partials for replay — so pairing is structural, not a
/// per-outcome fallback. Idempotent: appends an error `ToolResult` only for
/// `ToolUse` blocks in the last assistant message that have no matching result
/// after it; a fully paired tail is left untouched.
///
/// Called on every abandoned completion of `run_once_inner` (provider error,
/// cancel, closed channel). The fully-failed turn is truncated by the caller
/// afterwards, so the synthesized results are dropped there; on a cancelled
/// turn they are what keep the retained partial provider-valid.
fn close_orphaned_tool_uses(messages: &mut Vec<Message>) {
    let Some(assistant_index) = messages.iter().rposition(|m| m.role == Role::Assistant) else {
        return;
    };
    // Only the tail matters: scan the last assistant message's tool uses and
    // the results that came after it.
    let pending: Vec<String> = messages[assistant_index]
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    if pending.is_empty() {
        return;
    }
    let answered: std::collections::HashSet<&str> = messages[assistant_index + 1..]
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    let synthesized: Vec<ContentBlock> = pending
        .iter()
        .filter(|id| !answered.contains(id.as_str()))
        .map(|id| ContentBlock::ToolResult {
            tool_use_id: id.clone(),
            content: "cancelled: tool run interrupted before producing a result".to_string(),
            is_error: true,
            images: Vec::new(),
        })
        .collect();
    if synthesized.is_empty() {
        return;
    }
    messages.push(Message {
        role: Role::Tool,
        blocks: synthesized,
        kind: lofi_types::PromptKind::User,
    });
}

/// Count attached images across the request history, for the omit notice.
fn count_image_blocks(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .map(|b| match b {
            ContentBlock::Image { .. } => 1,
            ContentBlock::ToolResult { images, .. } => images.len(),
            _ => 0,
        })
        .sum()
}

/// Return a copy of `messages` with every image dropped for a model without
/// image support: standalone `Image` blocks become text markers, and the
/// images carried on a `ToolResult` are dropped with a marker appended to its
/// text. The model still sees that an attachment was present. Used only on the
/// send path; the durable transcript is untouched.
fn strip_image_blocks(messages: &[Message]) -> Vec<Message> {
    use std::fmt::Write as _;
    messages
        .iter()
        .map(|m| Message {
            role: m.role,
            kind: m.kind,
            blocks: m
                .blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Image { media_type, .. } => ContentBlock::Text {
                        text: format!("[image omitted: model does not support images; media_type={media_type}]"),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        images,
                    } if !images.is_empty() => {
                        let mut content = content.clone();
                        for img in images {
                            if !content.is_empty() {
                                content.push('\n');
                            }
                            let _ = write!(content, "[image omitted: model does not support images; media_type={}]", img.media_type);
                        }
                        ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content,
                            is_error: *is_error,
                            images: Vec::new(),
                        }
                    }
                    other => other.clone(),
                })
                .collect(),
        })
        .collect()
}

/// Recognize a tagged image payload from a tool result and upgrade it to a
/// [`lofi_types::ToolResultImage`] carried on that result block. `lofi.read`
/// returns `{type:"image", media_type, data_b64}` when it reads an image file
/// (the sandbox boundary is JSON, so bytes cannot cross directly). On a match
/// the base64 is decoded into `bytes` and the `data_b64` field is replaced with
/// a compact `bytes` count, so the durable transcript keeps a small marker
/// while the decoded image rides the `ToolResult` for same-round vision.
/// Returns `None` (and leaves `value` untouched) for any non-image payload.
fn upgrade_tagged_image(value: &mut serde_json::Value) -> Option<lofi_types::ToolResultImage> {
    use base64::Engine as _;
    let obj = value.as_object()?;
    if obj.get("type")?.as_str()? != "image" {
        return None;
    }
    let media_type = obj.get("media_type")?.as_str()?.to_string();
    let data_b64 = obj.get("data_b64")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .ok()?;
    let byte_len = bytes.len();
    if let Some(obj) = value.as_object_mut() {
        obj.remove("data_b64");
        obj.insert("bytes".to_string(), serde_json::json!(byte_len));
    }
    Some(lofi_types::ToolResultImage { bytes, media_type })
}

/// Request image payload budget (base64 bytes). Providers cap the request
/// *body*, not just the token count: Anthropic rejects a body over 32 MiB
/// with HTTP 413 `request_too_large` even when the token estimate is well
/// under the context window. Images are the dominant unbounded term (text,
/// tool results, and schemas are bounded elsewhere), so the byte pressure
/// signal thresholds just the image payload. 8 MiB sits far under every
/// provider body cap after base64 inflation (x4/3) plus envelope overhead.
const MAX_REQUEST_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Sentinel error message marking a stop-before-send due to an oversized
/// image payload. The run loop intercepts it and routes to the existing
/// `ContextPressure` recovery (force-compact + continue) instead of failing
/// the turn. A sentinel string, not a new `Error` variant, because only this
/// send/loop pair needs to distinguish it and a variant would leak an
/// internal control-flow detail into the shared error type.
const IMAGE_PRESSURE_SENTINEL: &str = "__lofi_image_pressure__";

/// Sum the base64 payload bytes of every attached image across the request
/// history. Base64 length is `4 * ceil(n/3)` for `n` raw bytes.
fn image_payload_bytes(messages: &[Message]) -> u64 {
    messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .map(|b| match b {
            ContentBlock::Image { bytes, .. } => 4 * bytes.len().div_ceil(3) as u64,
            ContentBlock::ToolResult { images, .. } => images
                .iter()
                .map(|img| 4 * img.bytes.len().div_ceil(3) as u64)
                .sum(),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use base64::Engine as _;
    use lofi_types::PromptKind;
    use serde_json::json;

    fn assistant_with_tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "exec".to_string(),
                input: json!({"code": "x"}),
            }],
            kind: PromptKind::default(),
        }
    }

    #[test]
    fn orphaned_tool_use_gets_a_cancelled_error_result() {
        let mut messages = vec![assistant_with_tool_use("tool-1")];
        close_orphaned_tool_uses(&mut messages);
        assert_eq!(messages.len(), 2);
        let results = &messages[1];
        assert_eq!(results.role, Role::Tool);
        match &results.blocks[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "tool-1");
                assert!(is_error);
                assert!(content.contains("cancelled"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn answered_tool_use_is_left_alone() {
        let mut messages = vec![
            assistant_with_tool_use("tool-1"),
            Message {
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "tool-1".to_string(),
                    content: "ok".to_string(),
                    is_error: false,
                    images: Vec::new(),
                }],
                kind: PromptKind::default(),
            },
        ];
        close_orphaned_tool_uses(&mut messages);
        assert_eq!(messages.len(), 2);
        match &messages[1].blocks[0] {
            ContentBlock::ToolResult { content, .. } => assert_eq!(content, "ok"),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn text_only_assistant_message_is_untouched() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "partial text".to_string(),
            }],
            kind: PromptKind::default(),
        }];
        close_orphaned_tool_uses(&mut messages);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn only_unanswered_tool_uses_are_closed() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                blocks: vec![
                    ContentBlock::ToolUse {
                        id: "tool-1".to_string(),
                        name: "exec".to_string(),
                        input: json!({"code": "a"}),
                    },
                    ContentBlock::ToolUse {
                        id: "tool-2".to_string(),
                        name: "exec".to_string(),
                        input: json!({"code": "b"}),
                    },
                ],
                kind: PromptKind::default(),
            },
            Message {
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "tool-1".to_string(),
                    content: "ok".to_string(),
                    is_error: false,
                    images: Vec::new(),
                }],
                kind: PromptKind::default(),
            },
        ];
        close_orphaned_tool_uses(&mut messages);
        assert_eq!(messages.len(), 3);
        match &messages[2].blocks[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "tool-2");
                assert!(is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn upgrade_tagged_image_decodes_and_strips_base64() {
        let raw = vec![1u8, 2, 3, 4, 5];
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let mut value = serde_json::json!({
            "ok": true,
            "type": "image",
            "media_type": "image/png",
            "data_b64": data_b64,
        });
        let img = upgrade_tagged_image(&mut value).unwrap();
        assert_eq!(img.bytes, raw);
        assert_eq!(img.media_type, "image/png");
        // The heavyweight base64 is gone; a compact byte count remains.
        assert!(value.get("data_b64").is_none());
        assert_eq!(value["bytes"], serde_json::json!(5));
    }

    #[test]
    fn upgrade_tagged_image_ignores_non_image_payloads() {
        let mut text = serde_json::json!({"ok": true, "content": "hi"});
        assert!(upgrade_tagged_image(&mut text).is_none());
        assert_eq!(text["content"], serde_json::json!("hi"));
        // Wrong type tag.
        let mut other = serde_json::json!({"type": "text", "data_b64": "AAAA"});
        assert!(upgrade_tagged_image(&mut other).is_none());
        // Bad base64 must not produce a block.
        let mut bad =
            serde_json::json!({"type": "image", "media_type": "image/png", "data_b64": "!!!"});
        assert!(upgrade_tagged_image(&mut bad).is_none());
    }

    #[test]
    fn strip_image_blocks_replaces_images_with_text_markers() {
        let messages = vec![Message {
            role: Role::User,
            blocks: vec![
                ContentBlock::Text {
                    text: "look".to_string(),
                },
                ContentBlock::Image {
                    bytes: vec![1, 2, 3],
                    media_type: "image/jpeg".to_string(),
                },
                ContentBlock::Image {
                    bytes: vec![4, 5],
                    media_type: "image/png".to_string(),
                },
            ],
            kind: PromptKind::default(),
        }];
        assert_eq!(count_image_blocks(&messages), 2);
        let stripped = strip_image_blocks(&messages);
        assert_eq!(count_image_blocks(&stripped), 0);
        // The original history is untouched.
        assert_eq!(count_image_blocks(&messages), 2);
        match &stripped[0].blocks[1] {
            ContentBlock::Text { text } => {
                assert!(text.contains("image omitted"));
                assert!(text.contains("image/jpeg"));
            }
            other => panic!("expected Text marker, got {other:?}"),
        }
        // Non-image blocks are preserved verbatim.
        assert_eq!(
            stripped[0].blocks[0],
            ContentBlock::Text {
                text: "look".to_string()
            }
        );
    }

    #[test]
    fn strip_image_blocks_downgrades_tool_result_images() {
        let messages = vec![Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "read image.png".to_string(),
                is_error: false,
                images: vec![lofi_types::ToolResultImage {
                    bytes: vec![1, 2, 3],
                    media_type: "image/png".to_string(),
                }],
            }],
            kind: PromptKind::default(),
        }];
        assert_eq!(count_image_blocks(&messages), 1);
        let stripped = strip_image_blocks(&messages);
        assert_eq!(count_image_blocks(&stripped), 0);
        match &stripped[0].blocks[0] {
            ContentBlock::ToolResult {
                content, images, ..
            } => {
                assert!(content.contains("read image.png"));
                assert!(content.contains("image omitted"));
                assert!(content.contains("image/png"));
                assert!(images.is_empty());
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        // Payload bytes count both standalone and tool-result images.
        assert!(image_payload_bytes(&messages) > 0);
        assert_eq!(image_payload_bytes(&stripped), 0);
    }

    #[test]
    fn strip_image_blocks_is_a_noop_without_images() {
        let messages = vec![Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
            kind: PromptKind::default(),
        }];
        assert_eq!(count_image_blocks(&messages), 0);
        let stripped = strip_image_blocks(&messages);
        assert_eq!(stripped, messages);
    }

    #[test]
    fn image_payload_bytes_counts_base64_length() {
        let image = |n: usize| ContentBlock::Image {
            bytes: vec![0u8; n],
            media_type: "image/jpeg".to_string(),
        };
        let messages = vec![Message {
            role: Role::User,
            blocks: vec![
                ContentBlock::Text {
                    text: "not counted".to_string(),
                },
                image(3), // 4 base64 bytes
                image(4), // 8 base64 bytes (4 -> ceil(4/3)=2 -> 8)
                image(1), // 4 base64 bytes
            ],
            kind: PromptKind::default(),
        }];
        assert_eq!(image_payload_bytes(&messages), 16);
        // Text and empty histories contribute nothing.
        assert_eq!(image_payload_bytes(&[]), 0);
    }

    #[test]
    fn image_payload_budget_distinguishes_over_and_under() {
        let image = |n: usize| ContentBlock::Image {
            bytes: vec![0u8; n],
            media_type: "image/jpeg".to_string(),
        };
        // 7 MiB raw -> ~9.3 MiB base64, over the 8 MiB budget.
        let over = vec![Message {
            role: Role::User,
            blocks: vec![image(7 * 1024 * 1024)],
            kind: PromptKind::default(),
        }];
        assert!(image_payload_bytes(&over) > MAX_REQUEST_IMAGE_BYTES);
        // 1 MiB raw stays well under.
        let under = vec![Message {
            role: Role::User,
            blocks: vec![image(1024 * 1024)],
            kind: PromptKind::default(),
        }];
        assert!(image_payload_bytes(&under) <= MAX_REQUEST_IMAGE_BYTES);
    }
}
