#![allow(clippy::wildcard_imports)]

use super::*;

const THINKING_LOOP_MIN_BYTES: usize = 100;
const THINKING_LOOP_REPETITIONS: usize = 3;
const THINKING_LOOP_MAX_PERIOD_BYTES: usize = 2 * 1024;
const THINKING_LOOP_HISTORY_BYTES: usize = 24 * 1024;
const TOOL_LOOP_REPETITIONS: usize = 5;
const TOOL_LOOP_MAX_CYCLE: usize = 5;
pub(super) const MAX_RECOVERY_ROUNDS: usize = 100;
const LOOP_RECOVERY_PROMPT: &str = "A potential loop was detected in your repeated reasoning or tool results. Stop repeating the same approach. Review the latest result, choose a different concrete action, and continue only if it makes forward progress.";

#[derive(Default)]
struct LoopDetector {
    thinking: Vec<u8>,
    thinking_checked_at: usize,
    tool_rounds: Vec<String>,
}

impl LoopDetector {
    fn start_round(&mut self) {
        self.thinking.clear();
        self.thinking_checked_at = 0;
    }

    fn add_thinking(&mut self, delta: &str) -> Option<String> {
        self.thinking.extend_from_slice(delta.as_bytes());
        if self.thinking.len() > THINKING_LOOP_HISTORY_BYTES {
            let excess = self.thinking.len() - THINKING_LOOP_HISTORY_BYTES;
            self.thinking.drain(..excess);
            self.thinking_checked_at = self.thinking_checked_at.saturating_sub(excess);
        }
        if self.thinking.len() < self.thinking_checked_at + THINKING_LOOP_MIN_BYTES {
            return None;
        }
        self.check_thinking()
    }

    fn finish_thinking(&mut self) -> Option<String> {
        if self.thinking.len() == self.thinking_checked_at {
            return None;
        }
        self.check_thinking()
    }

    fn check_thinking(&mut self) -> Option<String> {
        self.thinking_checked_at = self.thinking.len();
        let relevant = THINKING_LOOP_REPETITIONS * THINKING_LOOP_MAX_PERIOD_BYTES;
        let start = self.thinking.len().saturating_sub(relevant);
        let reversed = self.thinking[start..]
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>();
        let prefix_matches = z_array(&reversed);
        let max_period =
            (reversed.len() / THINKING_LOOP_REPETITIONS).min(THINKING_LOOP_MAX_PERIOD_BYTES);
        for (period, prefix_match) in prefix_matches
            .iter()
            .enumerate()
            .take(max_period + 1)
            .skip(THINKING_LOOP_MIN_BYTES)
        {
            if *prefix_match >= period * (THINKING_LOOP_REPETITIONS - 1) {
                return Some(format!(
                    "repeated thinking pattern detected ({period} bytes repeated {THINKING_LOOP_REPETITIONS} times)"
                ));
            }
        }
        None
    }

    fn add_tool_round(&mut self, fingerprint: String) -> Option<String> {
        self.tool_rounds.push(fingerprint);
        let keep = TOOL_LOOP_REPETITIONS * TOOL_LOOP_MAX_CYCLE;
        if self.tool_rounds.len() > keep {
            self.tool_rounds.remove(0);
        }
        for cycle in 1..=TOOL_LOOP_MAX_CYCLE {
            let repeated = cycle * TOOL_LOOP_REPETITIONS;
            if self.tool_rounds.len() < repeated {
                continue;
            }
            let tail = &self.tool_rounds[self.tool_rounds.len() - repeated..];
            if tail
                .chunks_exact(cycle)
                .all(|chunk| chunk == &tail[..cycle])
            {
                return Some(format!(
                    "repeated tool-result cycle detected ({cycle} round cycle repeated {TOOL_LOOP_REPETITIONS} times)"
                ));
            }
        }
        None
    }
}

fn z_array(bytes: &[u8]) -> Vec<usize> {
    let mut matches = vec![0; bytes.len()];
    let (mut left, mut right) = (0, 0);
    for index in 1..bytes.len() {
        if index < right {
            matches[index] = matches[index - left].min(right - index);
        }
        while index + matches[index] < bytes.len()
            && bytes[matches[index]] == bytes[index + matches[index]]
        {
            matches[index] += 1;
        }
        if index + matches[index] > right {
            left = index;
            right = index + matches[index];
        }
    }
    matches
}

/// Per-round tuneables that callers usually leave unset. Grouping them keeps
/// [`Agent::run_once_inner`] (and the public entry points that forward to it)
/// under the argument-count lint without a long `None`-studded call shape.
#[derive(Default)]
struct RoundOpts<'a> {
    recall: Option<RecallFn>,
    result: Option<ResultFn>,
    cancel: Option<&'a Arc<AtomicBool>>,
    prev_input_tokens: Option<u64>,
    /// Suppress the image-omit notice when true, so it fires once per turn
    /// at the continuation loop, not once per tool round in `run_once_inner`.
    suppress_omit_notice: bool,
    on_job_acquired: Option<lofi_code::JobAcquireFn>,
    recorder: Option<&'a mut SessionRecorder>,
    record_from: usize,
    loop_detector: Option<&'a mut LoopDetector>,
}

impl Agent {
    /// Seeds a fresh `[User]` history and drives [`Self::run_once`] in a loop.
    /// The configured system prompt is added to each provider request when the
    /// caller-owned history does not already contain one.
    /// If the receiver is dropped (the channel closes), the run exits gracefully.
    /// # Errors
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution failures that cannot be surfaced as a `ToolResult`.
    pub async fn run(&self, user_prompt: String, tx: Sender<AgentEvent>) -> Result<()> {
        let mut messages = Vec::new();
        self.run_continuation(
            &mut messages,
            user_prompt,
            lofi_types::PromptKind::User,
            tx,
            None,
            false,
            None,
            None,
        )
        .await
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
        self.run_continuation_with_notices(
            messages,
            user_prompt,
            prompt_kind,
            attachments,
            Vec::new(),
            tx,
            session,
            continuation,
            cancel,
            preempt,
        )
        .await
    }

    /// Runs a continuation after adding app-generated notices before the
    /// submitted prompt. Each notice uses the same durable prompt path as
    /// notices generated during the run.
    /// # Errors
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution failures that cannot be surfaced as a `ToolResult`.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn run_continuation_with_notices(
        &self,
        messages: &mut Vec<Message>,
        user_prompt: String,
        prompt_kind: lofi_types::PromptKind,
        attachments: Vec<ContentBlock>,
        notices: Vec<String>,
        tx: Sender<AgentEvent>,
        session: Option<&crate::session::store::SessionCursor>,
        continuation: bool,
        cancel: Option<Arc<AtomicBool>>,
        preempt: Option<Arc<AtomicBool>>,
    ) -> Result<()> {
        // Rejected: trim from the TUI when the channel closes. The burst
        // is allocated and dropped on this task's return paths.
        let _trim = lofi_code::memory::ReleaseFreedMemoryOnDrop;
        let prev_len = messages.len();
        let prompt = if continuation {
            None
        } else {
            // The system prompt is pinned on the durable transcript at each
            // context boundary (create, compact) and arrives via the restored
            // history — the engine never materializes it inline.
            let mut blocks = vec![ContentBlock::Text { text: user_prompt }];
            blocks.extend(attachments);
            Some(Message {
                role: Role::User,
                blocks,
                kind: prompt_kind,
            })
        };
        let mut stats = TurnStats::new();
        let mut recorder =
            session.map(|cursor| SessionRecorder::new(cursor.clone(), self.run_model()));
        let mut starts_run = true;
        for notice in notices {
            let message = Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: notice }],
                kind: lofi_types::PromptKind::Notice,
            };
            if !append_prompt(
                messages,
                message,
                starts_run,
                recorder.as_mut(),
                prev_len,
                &stats,
                &tx,
            )
            .await
            {
                return Ok(());
            }
            starts_run = false;
        }
        if let Some(prompt) = prompt {
            if !append_prompt(
                messages,
                prompt,
                starts_run,
                recorder.as_mut(),
                prev_len,
                &stats,
                &tx,
            )
            .await
            {
                return Ok(());
            }
        } else if !emit(Some(&tx), AgentEvent::TurnContinue).await {
            return Ok(());
        }
        // The user prompt is durable before tool execution starts. Record
        // each job acquisition synchronously against that cursor before its
        // driver starts, then let the job-owned completion hook record the
        // release. Failures are swallowed: losing a marker never stops a job.
        let on_job_acquired = session.map(|c| {
            let cursor = c.clone();
            std::sync::Arc::new(move |job_id: u64| {
                let owner_event_id = cursor.record_job_started(job_id).ok()?;
                let cursor = cursor.clone();
                Some(std::sync::Arc::new(move |job_id: u64| {
                    let _ = cursor.record_job_finished(&owner_event_id, job_id);
                }) as lofi_code::JobReleaseFn)
            }) as lofi_code::JobAcquireFn
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
                match cursor.recovery_message_by_id(id) {
                    Ok(Some(crate::session::store::RecoveryMessage::Message(message))) => {
                        crate::context_edit::recover_message_content(&message).unwrap_or_else(|| {
                            format!(
                                "no recoverable content for event {id:?} (message held no elidable block)."
                            )
                        })
                    }
                    Ok(Some(crate::session::store::RecoveryMessage::NotMessage)) => format!(
                            "no recoverable content for event {id:?} (not a message event)."
                        ),
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
        let mut retry = crate::retry::RetrySession::new(self.retry);
        // The image-omit notice fires once per turn (on the first round),
        // not once per tool round.
        let mut omit_notice_sent = false;
        // All automatic recovery paths share one per-turn budget so a broken
        // provider template or repeated token cap cannot loop unattended.
        // A round that made forward progress resets the budget.
        let mut auto_continued = false;
        let mut loop_recovered = false;
        let mut loop_detector = LoopDetector::default();
        let mut recovery_rounds = 0usize;
        loop {
            if tx.is_closed() {
                detached = true;
                break;
            }
            // Feed the prior round's context fill back in so the next request
            // clips its output cap against the remaining context window.
            let prev_input = Some(stats.usage.context_tokens());
            if let Some(message) = recovery_round_exceeded(&mut recovery_rounds) {
                if !append_prompt(
                    messages,
                    message,
                    false,
                    recorder.as_mut(),
                    prev_len,
                    &stats,
                    &tx,
                )
                .await
                {
                    detached = true;
                    break;
                }
                finished_normally = true;
                break;
            }
            let round_start = messages.len();
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
                        on_job_acquired: on_job_acquired.clone(),
                        recorder: recorder.as_mut(),
                        record_from: prev_len,
                        loop_detector: Some(&mut loop_detector),
                    },
                )
                .await;
            omit_notice_sent = true;
            if round.is_ok() {
                if let Some(attempt) = retry.reset_after_progress() {
                    let _ = tx
                        .send(AgentEvent::RetryEnd {
                            success: true,
                            attempt,
                            final_error: None,
                        })
                        .await;
                }
            }
            match round {
                Ok(outcome) => {
                    let finished = outcome.finished;
                    // Latest finished round wins: the recorded reason must be
                    // the round that actually ended the turn.
                    stats.stop_reason = outcome.stop_reason;
                    // An explicit interrupt wins races against both a normal
                    // terminal response and queued-prompt preemption. A native
                    // tool can observe cancellation, settle its process group,
                    // and still return a structured result; without this
                    // boundary check that cancelled turn is recorded as done.
                    if cancel
                        .as_ref()
                        .is_some_and(|flag| flag.load(Ordering::Relaxed))
                    {
                        cancelled = true;
                        break;
                    }
                    let plan = handle_loop_detection(
                        outcome.loop_detail.as_deref(),
                        outcome.interrupted_thinking_index,
                        &mut loop_recovered,
                    );
                    // What streamed must reach the transcript before it
                    // leaves the model history.
                    if let Some((index, detail)) = plan.removal {
                        if let Err(error) = record_round_removal(
                            recorder.as_mut(),
                            messages,
                            prev_len,
                            1,
                            &stats,
                            &tx,
                            &detail,
                        )
                        .await
                        {
                            err = Some(error);
                            break;
                        }
                        messages.remove(index);
                    }
                    // Loop handling can replace an interrupted, unsigned
                    // reasoning message with a provider-valid recovery prompt.
                    // Persist and surface that prompt only after the correction
                    // reaches its final form.
                    if let Some(message) = plan.recovery {
                        if !append_prompt(
                            messages,
                            message,
                            false,
                            recorder.as_mut(),
                            prev_len,
                            &stats,
                            &tx,
                        )
                        .await
                        {
                            detached = true;
                            break;
                        }
                    } else {
                        commit_progress(recorder.as_mut(), &messages[prev_len..], &stats, &tx)
                            .await;
                    }
                    match plan.action {
                        LoopAction::Continue => continue,
                        LoopAction::Stop => {
                            if tx.is_closed() {
                                detached = true;
                            } else {
                                finished_normally = true;
                            }
                            break;
                        }
                        LoopAction::None => {}
                    }
                    if finished {
                        let recovery =
                            if outcome.stop_reason == Some(lofi_types::StopReason::MaxTokens) {
                                Some(TRUNCATION_CONTINUATION_PROMPT)
                            } else if self.auto_continue.lost_tool_call
                                && outcome.stop_reason == Some(lofi_types::StopReason::ToolUse)
                            {
                                Some(LOST_TOOL_CONTINUATION_PROMPT)
                            } else if self.auto_continue.intent
                                && outcome.stop_reason == Some(lofi_types::StopReason::EndTurn)
                                && outcome.announced_tool_intent
                            {
                                Some(INTENT_CONTINUATION_PROMPT)
                            } else {
                                None
                            };
                        if !auto_continued {
                            if let Some(prompt) = recovery {
                                auto_continued = true;
                                let message = Message {
                                    role: Role::User,
                                    blocks: vec![ContentBlock::Text {
                                        text: prompt.to_string(),
                                    }],
                                    kind: lofi_types::PromptKind::Notice,
                                };
                                if !append_prompt(
                                    messages,
                                    message,
                                    false,
                                    recorder.as_mut(),
                                    prev_len,
                                    &stats,
                                    &tx,
                                )
                                .await
                                {
                                    detached = true;
                                    break;
                                }
                                continue;
                            }
                        }
                        finished_normally = true;
                        break;
                    }
                    if tx.is_closed() {
                        detached = true;
                        break;
                    }
                    recovery_rounds = 0;
                    // Hard context cap: the round just completed (its tool
                    // result is in hand, so the latest turn is a matched
                    // tool cycle that compaction keeps verbatim). Stop before
                    // the next round would overflow the window and let the UI
                    // force-compact + continue.
                    if let Some(threshold) = self.hard_compact_threshold() {
                        let prompt_tokens = stats.usage.context_tokens();
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
                Err(RoundFailure {
                    error: Error::Cancelled,
                    ..
                }) => {
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
                Err(RoundFailure {
                    error,
                    provider_progress,
                }) => {
                    // An oversized image payload stopped before send; route
                    // to the existing force-compact + continue recovery. The
                    // round never ran, so the partial-turn suffix is empty
                    // and the recorder's `ContextPressure => None` arm leaves
                    // the durable transcript untouched.
                    // Match the variant payload, not `Display`: `Provider`
                    // prefixes its message with "provider error: ".
                    if matches!(&error, Error::Provider(m) if m == IMAGE_PRESSURE_SENTINEL) {
                        context_pressure = true;
                        break;
                    }
                    if provider_progress {
                        if let Some(attempt) = retry.reset_after_progress() {
                            let _ = tx
                                .send(AgentEvent::RetryEnd {
                                    success: false,
                                    attempt,
                                    final_error: Some(error.to_string()),
                                })
                                .await;
                        }
                    }
                    if let Some(plan) = retry.retry(&error) {
                        if messages.len() > round_start {
                            if let Err(record_error) = record_round_removal(
                                recorder.as_mut(),
                                messages,
                                prev_len,
                                messages.len() - round_start,
                                &stats,
                                &tx,
                                &format!("discarding partial response before retry: {error}"),
                            )
                            .await
                            {
                                err = Some(record_error);
                                break;
                            }
                            messages.truncate(round_start);
                        }
                        let _ = tx
                            .send(AgentEvent::RetryStart {
                                attempt: plan.attempt,
                                max_attempts: plan.max_retries,
                                delay_ms: plan.delay.as_millis() as u64,
                                error: error.to_string(),
                            })
                            .await;
                        tracing::warn!(
                            attempt = plan.attempt,
                            max_retries = plan.max_retries,
                            delay_ms = plan.delay.as_millis() as u64,
                            error = %error,
                            "provider request failed; retry scheduled"
                        );
                        // Cancellable backoff: stop promptly for Ctrl-C or a
                        // closed consumer instead of waiting out the retry
                        // delay and issuing another provider request.
                        let retry_ready = if let Some(flag) = cancel.as_ref() {
                            tokio::select! {
                                biased;
                                () = wait_for_cancel(flag) => false,
                                () = tx.closed() => false,
                                () = tokio::time::sleep(plan.delay) => true,
                            }
                        } else {
                            tokio::select! {
                                biased;
                                () = tx.closed() => false,
                                () = tokio::time::sleep(plan.delay) => true,
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
                    if retry.consecutive_failures() > 0 {
                        tracing::warn!(
                            attempt = retry.consecutive_failures(),
                            error = %error,
                            "provider request failed; retry budget exhausted"
                        );
                        let _ = tx
                            .send(AgentEvent::RetryEnd {
                                success: false,
                                attempt: retry.consecutive_failures(),
                                final_error: Some(error.to_string()),
                            })
                            .await;
                    }
                    err = Some(error);
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
            match recorder.flush_incremental(&messages[prev_len..], &flush_outcome, &summary) {
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
                            stop_reason: stats.stop_reason,
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

    fn request_messages(&self, messages: &[Message]) -> Option<Vec<Message>> {
        if self.system_prompt.is_empty()
            || messages.iter().any(|message| {
                message.role == Role::System
                    && message.blocks.iter().any(
                        |block| matches!(block, ContentBlock::Text { text } if !text.is_empty()),
                    )
            })
        {
            return None;
        }
        let mut request = Vec::with_capacity(messages.len() + 1);
        request.push(Message {
            role: Role::System,
            blocks: vec![ContentBlock::Text {
                text: self.system_prompt.clone(),
            }],
            kind: lofi_types::PromptKind::default(),
        });
        request.extend_from_slice(messages);
        Some(request)
    }

    /// # Errors
    /// Propagates [`Error`] from provider streaming or timeouts.
    pub async fn run_once(&self, messages: &mut Vec<Message>) -> Result<bool> {
        Ok(self
            .run_once_inner(messages, None, None, RoundOpts::default())
            .await
            .map_err(|failure| failure.error)?
            .finished)
    }

    /// Returns a [`RoundOutcome`]: `finished` is true when the model
    /// requested no tool uses, and `stop_reason` carries the provider's
    /// reason for ending generation so the caller can tell a deliberate
    /// end-of-turn from a truncation.
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
        opts: RoundOpts<'_>,
    ) -> std::result::Result<RoundOutcome, RoundFailure> {
        let RoundOpts {
            recall,
            result,
            cancel,
            prev_input_tokens,
            suppress_omit_notice,
            on_job_acquired,
            recorder,
            record_from,
            mut loop_detector,
        } = opts;
        let schema = exec_tool_schema();
        let mut model = self.model.clone();
        // Clip the output cap against remaining context so strict providers
        // (`max_tokens < context_window - input_tokens`) never reject the
        // request. `prev_input_tokens` is the prior round's usage, the best
        // in-hand estimate of this request's input.
        model.max_tokens = self.clipped_max_tokens(prev_input_tokens);
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled.into());
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
        let visible_messages = stripped.as_deref().unwrap_or(messages);
        let with_system = self.request_messages(visible_messages);
        let send_messages = with_system.as_deref().unwrap_or(visible_messages);
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
            return Err(Error::Provider(IMAGE_PRESSURE_SENTINEL.to_string()).into());
        }
        let schemas = [schema];
        let stream = match cancel {
            Some(flag) => {
                tokio::select! {
                    biased;
                    () = wait_for_cancel(flag) => return Err(Error::Cancelled.into()),
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

        if let Some(detector) = loop_detector.as_deref_mut() {
            detector.start_round();
        }
        let mut assembler = MessageAssembler::new();
        // Round usage is captured and emitted as `AgentEvent::Done` only when
        // the round is terminal (no tool calls), so `Done` remains a true
        // end-of-run signal rather than firing before tool execution.
        let mut round_usage: Option<Usage> = None;
        let mut round_stop_reason: Option<lofi_types::StopReason> = None;
        let mut round_bytes = 0usize;
        let mut tool_raw: HashMap<String, String> = HashMap::new();
        let mut tool_emitted: HashMap<String, usize> = HashMap::new();
        let mut tool_decoders: HashMap<String, CodePrefixDecoder> = HashMap::new();
        let mut round_loop_detail: Option<String> = None;
        let mut provider_progress = false;
        let collect = async {
            let mut thinking_open: Option<Instant> = None;
            loop {
                let next = match cancel {
                    Some(flag) => {
                        tokio::select! {
                            biased;
                            () = wait_for_cancel(flag) => return Err(Error::Cancelled),
                            next = stream.next() => next,
                        }
                    }
                    None => stream.next().await,
                };
                match next {
                    None => break,
                    Some(ev) => match ev {
                        Ok(e) => {
                            if !matches!(e, StreamingEvent::Error(_)) {
                                provider_progress = true;
                            }
                            let terminal = matches!(e, StreamingEvent::Done { .. });
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
                                if let Some(detail) = loop_detector
                                    .as_deref_mut()
                                    .and_then(LoopDetector::finish_thinking)
                                {
                                    round_loop_detail = Some(detail);
                                    break;
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
                                    if let Some(detail) = loop_detector
                                        .as_deref_mut()
                                        .and_then(|detector| detector.add_thinking(d))
                                    {
                                        round_loop_detail = Some(detail);
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
                                StreamingEvent::Done { usage, stop_reason } => {
                                    round_usage = Some(*usage);
                                    round_stop_reason = *stop_reason;
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
                                StreamingEvent::ThinkingRedacted { data } => data.len(),
                                StreamingEvent::PartSignature {
                                    provider,
                                    model,
                                    format,
                                    target,
                                    signature,
                                } => {
                                    provider.len()
                                        + model.len()
                                        + match format {
                                            lofi_types::PartSignatureFormat::OpenAiExtraContent {
                                                namespace,
                                            } => namespace.len(),
                                            lofi_types::PartSignatureFormat::Google
                                            | lofi_types::PartSignatureFormat::OpenAiReasoningDetail => 0,
                                        }
                                        + target.as_ref().map_or(0, String::len)
                                        + signature.len()
                                }
                                StreamingEvent::ToolUseStart { id, name } => id.len() + name.len(),
                                StreamingEvent::ToolUseInputDelta { id, delta } => {
                                    id.len() + delta.len()
                                }
                                StreamingEvent::ToolUseEnd { id } => id.len(),
                                StreamingEvent::Done { .. } => 0,
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
                            if round_loop_detail.is_some() {
                                break;
                            }
                        }
                        Err(err) => {
                            return Err(err);
                        }
                    },
                }
            }
            if round_loop_detail.is_none() {
                round_loop_detail = loop_detector
                    .as_deref_mut()
                    .and_then(LoopDetector::finish_thinking);
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
            return Err(RoundFailure {
                error,
                provider_progress,
            });
        }

        // A tool use whose arguments never parse was cut off mid-stream.
        // Compute before `finish` consumes the assembler; the set is only
        // consulted on a token-cap stop, where the cut calls route to a
        // synthetic result instead of executing truncated arguments.
        let cut_tool_ids: std::collections::HashSet<String> =
            if round_stop_reason == Some(lofi_types::StopReason::MaxTokens) {
                assembler.unparseable_tool_ids().into_iter().collect()
            } else {
                std::collections::HashSet::new()
            };
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
            let announced_tool_intent = assistant_announces_tool_intent(&messages[assistant_index]);
            return Ok(RoundOutcome {
                finished: true,
                stop_reason: round_stop_reason,
                announced_tool_intent,
                interrupted_thinking_index: round_loop_detail.as_ref().map(|_| assistant_index),
                loop_detail: round_loop_detail,
            });
        }

        let native_cursor =
            if let (Some(recorder), Some(stats), Some(tx)) = (recorder, stats.as_deref(), tx) {
                if let Some((byte_start, byte_end)) =
                    checkpoint_progress(Some(recorder), &messages[record_from..], stats, tx).await
                {
                    let _ = tx
                        .send(AgentEvent::RoundCommitted {
                            byte_start,
                            byte_end,
                        })
                        .await;
                    Some(recorder.cursor())
                } else {
                    None
                }
            } else {
                None
            };
        let run_refs: Vec<(&str, &str, &serde_json::Value)> = tool_uses
            .iter()
            .filter(|(id, _, _)| !cut_tool_ids.contains(id))
            .map(|(id, name, input)| (id.as_str(), name.as_str(), input))
            .collect();
        let executed = match self
            .execute_tools(
                &run_refs,
                tx,
                stats.as_deref_mut(),
                native_cursor,
                recall.clone(),
                result.clone(),
                cancel,
                on_job_acquired.clone(),
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
                return Err(error.into());
            }
        };

        let mut executed_iter = executed.into_iter();
        let mut results = Vec::with_capacity(tool_uses.len());
        for (id, _, _) in &tool_uses {
            if cut_tool_ids.contains(id) {
                let content =
                    "cut off: tool arguments were truncated by the provider token limit; \
                     resend the tool call"
                        .to_string();
                let elapsed_ms = stats.as_deref_mut().map_or(0, |s| s.tool_end(id));
                if !emit(
                    tx,
                    AgentEvent::ToolEnd {
                        id: id.clone(),
                        result: content.clone(),
                        is_error: true,
                        elapsed_ms,
                    },
                )
                .await
                {
                    close_orphaned_tool_uses(messages);
                    return Err(Error::Cancelled.into());
                }
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content,
                    is_error: true,
                    images: Vec::new(),
                });
            } else {
                // execute_tools returns exactly one result per input; a
                // short iterator means the tool loop already failed.
                let Some(r) = executed_iter.next() else {
                    close_orphaned_tool_uses(messages);
                    return Err(Error::Provider("tool result count mismatch".into()).into());
                };
                results.push(r);
            }
        }

        let result_index = messages.len();
        messages.push(Message {
            role: Role::Tool,
            blocks: results,
            kind: lofi_types::PromptKind::User,
        });
        let tool_loop_detail = loop_detector.and_then(|detector| {
            detector.add_tool_round(tool_round_fingerprint(
                &messages[assistant_index],
                &messages[result_index],
            ))
        });
        Ok(RoundOutcome {
            finished: false,
            stop_reason: round_stop_reason,
            announced_tool_intent: false,
            loop_detail: round_loop_detail.or(tool_loop_detail),
            interrupted_thinking_index: None,
        })
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn execute_tools(
        &self,
        tool_uses: &[(&str, &str, &serde_json::Value)],
        tx: Option<&Sender<AgentEvent>>,
        mut stats: Option<&mut TurnStats>,
        native_cursor: Option<crate::session::store::SessionCursor>,
        recall: Option<RecallFn>,
        result: Option<ResultFn>,
        cancel: Option<&Arc<AtomicBool>>,
        on_job_acquired: Option<lofi_code::JobAcquireFn>,
    ) -> Result<Vec<ContentBlock>> {
        if tool_uses.is_empty() {
            return Ok(Vec::new());
        }
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
            let on_tool_event: Arc<dyn Fn(ToolEvent) + Send + Sync> = {
                let native_pending = native_pending.clone();
                let native_cursor = native_cursor.clone();
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
                        let display_result = cap_tool_result(&result);
                        if let Some((name, args)) = lock(&native_pending).remove(&id) {
                            if let Some(cursor) = native_cursor.as_ref() {
                                let record = NativeToolRecord {
                                    parent: event_parent.clone(),
                                    call_id: id,
                                    name,
                                    args,
                                    result,
                                    is_error,
                                };
                                if let Err(error) = cursor.record(
                                    crate::session::recorder::SessionRecord::NativeTool { record },
                                ) {
                                    let _ = native_tx.send(AgentEvent::Notice(format!(
                                        "transcript write failed for native tool result: {error}"
                                    )));
                                }
                            }
                        }
                        let _ = native_tx.send(AgentEvent::NativeToolEnd {
                            parent: event_parent.clone(),
                            id,
                            result: display_result,
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
                policy_override: self.policy_override.clone(),
                skills_dir: self.skills_dir.clone(),
                truncate: self.truncate,
                jobs: self.jobs.clone(),
                on_job_acquired: on_job_acquired.clone(),
            };
            let outcome = self
                .sandbox_worker()
                .exec(
                    &code,
                    exec_ctx,
                    ExecOptions {
                        timeout: lofi_code::DEFAULT_GUEST_TIMEOUT,
                        max_value_bytes: MAX_EXEC_RESULT_BYTES,
                        cancel: cancel.cloned(),
                    },
                )
                .await;
            // A tool may return a tagged image (`read` on an image file).
            // Upgrade it to a `ToolResultImage` carried on this result block,
            // so the model sees the image in the same round as the result on
            // every provider. The heavyweight base64 is stripped out of the
            // text payload, leaving a compact `bytes` count in the transcript.
            let (content, is_error, result_images) = match outcome {
                Ok(r) => {
                    let has_value = r.has_returned_value();
                    let mut value = r.value;
                    match upgrade_exec_image(&mut value, r.image, &self.image) {
                        Ok(img) => {
                            let is_error = !r.errors.is_empty();
                            let mut output = serde_json::Map::new();
                            output.insert("ok".into(), serde_json::Value::Bool(!is_error));
                            if is_error {
                                output.insert("errors".into(), serde_json::Value::Array(r.errors));
                            }
                            if has_value {
                                output.insert("value".into(), value);
                            }
                            if !r.logs.is_empty() {
                                output.insert("logs".into(), serde_json::Value::String(r.logs));
                            }
                            if r.value_truncated {
                                output.insert("truncated".into(), serde_json::Value::Bool(true));
                            }
                            let content = serde_json::to_string(&output)
                                .unwrap_or_else(|_| "{\"ok\":true}".to_string());
                            (cap_exec_result(&content), is_error, img)
                        }
                        Err(error) => (cap_exec_result(&error.to_string()), true, None),
                    }
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
                    if let Some(cursor) = native_cursor.as_ref() {
                        let record = NativeToolRecord {
                            parent: parent.clone(),
                            call_id,
                            name,
                            args,
                            result: result.clone(),
                            is_error: true,
                        };
                        if let Err(error) = cursor
                            .record(crate::session::recorder::SessionRecord::NativeTool { record })
                        {
                            let _ = relay_tx.send(AgentEvent::Notice(format!(
                                "transcript write failed for native tool result: {error}"
                            )));
                        }
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

/// Checkpoint the turn suffix; a write failure is notified, not fatal.
/// The recorder keeps its counts, so the same suffix is retried by the
/// next checkpoint or the final flush.
async fn commit_progress(
    recorder: Option<&mut SessionRecorder>,
    messages: &[Message],
    stats: &TurnStats,
    tx: &Sender<AgentEvent>,
) {
    if let Some((byte_start, byte_end)) = checkpoint_progress(recorder, messages, stats, tx).await {
        let _ = tx
            .send(AgentEvent::RoundCommitted {
                byte_start,
                byte_end,
            })
            .await;
    }
}

async fn checkpoint_progress(
    recorder: Option<&mut SessionRecorder>,
    messages: &[Message],
    stats: &TurnStats,
    tx: &Sender<AgentEvent>,
) -> Option<(u64, u64)> {
    let recorder = recorder?;
    let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
    match recorder.checkpoint(messages, &stats.summary(elapsed_ms)) {
        Ok(range) => range,
        Err(error) => {
            let _ = emit(
                Some(tx),
                AgentEvent::Notice(format!(
                    "transcript write failed (will retry on the next round): {error}"
                )),
            )
            .await;
            None
        }
    }
}

/// Checkpoint the preceding response, then add one durable prompt and expose
/// the same transcript boundary live that replay derives from the message.
/// Splitting the checkpoints keeps each byte range with its matching turn.
async fn append_prompt(
    messages: &mut Vec<Message>,
    prompt: Message,
    starts_run: bool,
    mut recorder: Option<&mut SessionRecorder>,
    prev_len: usize,
    stats: &TurnStats,
    tx: &Sender<AgentEvent>,
) -> bool {
    let Some(event) = AgentEvent::from_prompt(&prompt) else {
        debug_assert!(false, "append_prompt requires a user-role prompt");
        return false;
    };
    if let Some((byte_start, byte_end)) =
        checkpoint_progress(recorder.as_deref_mut(), &messages[prev_len..], stats, tx).await
    {
        if !emit(
            Some(tx),
            AgentEvent::RoundCommitted {
                byte_start,
                byte_end,
            },
        )
        .await
        {
            return false;
        }
    }
    messages.push(prompt);
    let range = checkpoint_progress(recorder, &messages[prev_len..], stats, tx).await;
    if starts_run && !emit(Some(tx), AgentEvent::RunStart).await {
        return false;
    }
    if !emit(Some(tx), event).await {
        return false;
    }
    if let Some((byte_start, byte_end)) = range {
        if !emit(
            Some(tx),
            AgentEvent::RoundCommitted {
                byte_start,
                byte_end,
            },
        )
        .await
        {
            return false;
        }
    }
    true
}

async fn record_round_removal(
    recorder: Option<&mut SessionRecorder>,
    messages: &[Message],
    prev_len: usize,
    removed_count: usize,
    stats: &TurnStats,
    tx: &Sender<AgentEvent>,
    reason: &str,
) -> Result<()> {
    let Some(recorder) = recorder else {
        return Ok(());
    };
    let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
    let suffix = messages.get(prev_len..).unwrap_or_default();
    match recorder.checkpoint(suffix, &stats.summary(elapsed_ms)) {
        Ok(Some((byte_start, byte_end))) => {
            let _ = emit(
                Some(tx),
                AgentEvent::RoundCommitted {
                    byte_start,
                    byte_end,
                },
            )
            .await;
        }
        Ok(None) => {}
        Err(error) => {
            let _ = emit(
                Some(tx),
                AgentEvent::Notice(format!("transcript write failed: {error}")),
            )
            .await;
            return Err(error);
        }
    }
    // The checkpoint just counted the round the caller is about to remove;
    // without this credit, later slices would skip the first recovery
    // message — also on a boundary-write failure, since the checkpoint
    // already made the round durable.
    recorder.note_recorded_messages_removed(removed_count);
    if let Err(error) = recorder.discard_round(reason) {
        let _ = emit(
            Some(tx),
            AgentEvent::Notice(format!("discard boundary write failed: {error}")),
        )
        .await;
    }
    Ok(())
}

struct RoundFailure {
    error: Error,
    provider_progress: bool,
}

impl From<Error> for RoundFailure {
    fn from(error: Error) -> Self {
        Self {
            error,
            provider_progress: false,
        }
    }
}

/// Result of a single provider round: whether the turn is finished (no
/// tool uses requested) and why the provider stopped generating.
struct RoundOutcome {
    finished: bool,
    stop_reason: Option<lofi_types::StopReason>,
    announced_tool_intent: bool,
    loop_detail: Option<String>,
    interrupted_thinking_index: Option<usize>,
}

fn tool_round_fingerprint(assistant: &Message, result: &Message) -> String {
    use std::hash::{Hash, Hasher};

    let mut value = String::new();
    for block in &assistant.blocks {
        if let ContentBlock::ToolUse { name, input, .. } = block {
            value.push_str(name);
            value.push('\0');
            value.push_str(&serde_json::to_string(input).unwrap_or_default());
            value.push('\0');
        }
    }
    for block in &result.blocks {
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = block
        {
            value.push_str(if *is_error { "error" } else { "ok" });
            value.push('\0');
            value.push_str(content);
            value.push('\0');
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn loop_recovery_message(detail: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: format!("{LOOP_RECOVERY_PROMPT} Detection: {detail}"),
        }],
        kind: lofi_types::PromptKind::Notice,
    }
}

enum LoopAction {
    None,
    Continue,
    Stop,
}

fn recovery_round_exceeded(recovery_rounds: &mut usize) -> Option<Message> {
    *recovery_rounds += 1;
    if *recovery_rounds <= MAX_RECOVERY_ROUNDS {
        return None;
    }
    Some(Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: format!(
            "agent stopped after {MAX_RECOVERY_ROUNDS} consecutive recovery rounds without forward progress"
            ),
        }],
        kind: lofi_types::PromptKind::Notice,
    })
}

/// Plan for one detected loop, computed without touching history so the
/// caller can arrange durability before the memory drop.
struct LoopPlan {
    action: LoopAction,
    removal: Option<(usize, String)>,
    recovery: Option<Message>,
}

fn handle_loop_detection(
    detail: Option<&str>,
    interrupted_thinking_index: Option<usize>,
    recovered: &mut bool,
) -> LoopPlan {
    let Some(detail) = detail else {
        return LoopPlan {
            action: LoopAction::None,
            removal: None,
            recovery: None,
        };
    };
    if *recovered {
        let detail = format!("agent stopped after loop recovery failed: {detail}");
        return LoopPlan {
            action: LoopAction::Stop,
            removal: interrupted_thinking_index.map(|index| (index, detail.clone())),
            recovery: Some(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: detail }],
                kind: lofi_types::PromptKind::Notice,
            }),
        };
    }
    *recovered = true;
    let notice =
        format!("potential agent loop detected; requesting a different approach: {detail}");
    LoopPlan {
        action: LoopAction::Continue,
        removal: interrupted_thinking_index.map(|index| (index, notice)),
        recovery: Some(loop_recovery_message(detail)),
    }
}

pub(super) fn assistant_announces_tool_intent(message: &Message) -> bool {
    let Some(text) = message.blocks.iter().rev().find_map(|block| match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    }) else {
        return false;
    };
    let paragraph = text.trim().rsplit("\n\n").next().unwrap_or("").trim();
    if paragraph.is_empty() || paragraph.chars().count() > 240 {
        return false;
    }
    let normalized = paragraph.replace(['’', '‘'], "'").to_ascii_lowercase();
    let sentence = normalized
        .rsplit(['.', '!', '?'])
        .find(|part| !part.trim().is_empty())
        .unwrap_or("")
        .trim();
    let action = [
        "let me ",
        "i'll ",
        "i will ",
        "i am going to ",
        "i'm going to ",
        "next, i'll ",
        "next i'll ",
    ]
    .iter()
    .find_map(|prefix| sentence.strip_prefix(prefix));
    let Some(action) = action else {
        return false;
    };
    if action.starts_with("not ") {
        return false;
    }
    let verb = action
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    matches!(
        verb,
        "inspect"
            | "check"
            | "read"
            | "load"
            | "search"
            | "run"
            | "test"
            | "verify"
            | "investigate"
            | "implement"
            | "fix"
            | "update"
            | "edit"
            | "try"
    )
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

/// Normalize an image returned out of band by the exec sandbox. `QuickJS` carries
/// only a small token, so source image bytes never become a base64 guest string
/// or a second host JSON allocation. The durable result keeps only a byte count.
fn upgrade_exec_image(
    value: &mut serde_json::Value,
    image: Option<lofi_code::ExecImage>,
    config: &lofi_types::ImageConfig,
) -> Result<Option<lofi_types::ToolResultImage>> {
    use std::io::Seek as _;

    let Some(mut image) = image else {
        return Ok(None);
    };
    image.file.rewind()?;
    let reader = std::io::BufReader::new(image.file);
    let (bytes, media_type) = crate::image::normalize_reader(reader, config)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("bytes".to_string(), serde_json::json!(bytes.len()));
        obj.insert("media_type".to_string(), serde_json::json!(&media_type));
    }
    Ok(Some(lofi_types::ToolResultImage { bytes, media_type }))
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
    fn thinking_loop_requires_three_repeated_long_suffixes() {
        let mut detector = LoopDetector::default();
        let pattern = "abcdefghij".repeat(10);
        assert!(detector.add_thinking(&pattern).is_none());
        assert!(detector.add_thinking(&pattern).is_none());
        assert!(detector
            .add_thinking(&pattern)
            .is_some_and(|detail| detail.contains("repeated thinking")));
    }

    #[test]
    fn thinking_loop_checks_a_short_final_delta() {
        let mut detector = LoopDetector::default();
        let pattern = "abcdefghij".repeat(10);
        assert!(detector
            .add_thinking(&format!("{}{}", pattern.repeat(2), &pattern[..1]))
            .is_none());
        assert!(detector.add_thinking(&pattern[1..]).is_none());
        assert!(detector
            .finish_thinking()
            .is_some_and(|detail| detail.contains("repeated thinking")));
    }

    #[test]
    fn thinking_loop_does_not_span_provider_rounds() {
        let mut detector = LoopDetector::default();
        let pattern = "abcdefghij".repeat(10);
        for _ in 0..3 {
            detector.start_round();
            assert!(detector.add_thinking(&pattern).is_none());
            assert!(detector.finish_thinking().is_none());
        }
    }

    #[test]
    fn thinking_loop_detects_varied_period_and_rejects_near_match() {
        let pattern = (0..137)
            .map(|index| char::from(b'!' + (index * 17 % 90) as u8))
            .collect::<String>();
        let mut near = format!("{pattern}{pattern}{pattern}");
        near.replace_range(near.len() - 1.., "~");

        let mut detector = LoopDetector::default();
        assert!(detector.add_thinking(&near).is_none());
        detector.start_round();
        assert!(detector
            .add_thinking(&pattern.repeat(3))
            .is_some_and(|detail| detail.contains("137 bytes")));
    }

    #[test]
    fn changing_tool_results_do_not_trigger_a_loop() {
        let mut detector = LoopDetector::default();
        for result in ["1", "2", "3", "4", "5"] {
            assert!(detector
                .add_tool_round(format!("same-call:{result}"))
                .is_none());
        }
    }

    #[test]
    fn identical_tool_result_rounds_trigger_after_five_repetitions() {
        let mut detector = LoopDetector::default();
        for _ in 0..TOOL_LOOP_REPETITIONS - 1 {
            assert!(detector.add_tool_round("same".into()).is_none());
        }
        assert!(detector
            .add_tool_round("same".into())
            .is_some_and(|detail| detail.contains("tool-result cycle")));
    }

    #[test]
    fn alternating_tool_result_cycle_is_detected() {
        let mut detector = LoopDetector::default();
        for fingerprint in ["a", "b"].into_iter().cycle().take(9) {
            assert!(detector.add_tool_round(fingerprint.into()).is_none());
        }
        assert!(detector
            .add_tool_round("b".into())
            .is_some_and(|detail| detail.contains("2 round cycle")));
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
    fn upgrade_exec_image_normalizes_file_backed_source() {
        const TINY_PNG: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let file = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut &file, TINY_PNG).unwrap();
        let image = lofi_code::ExecImage {
            file,
            bytes: TINY_PNG.len(),
        };
        let mut value = serde_json::json!({
            "type": "image",
            "media_type": "image/png",
            "bytes": TINY_PNG.len(),
        });

        let image =
            upgrade_exec_image(&mut value, Some(image), &lofi_types::ImageConfig::default())
                .unwrap()
                .unwrap();

        assert_eq!(image.media_type, "image/jpeg");
        assert!(!image.bytes.is_empty());
        assert_eq!(value["bytes"], serde_json::json!(image.bytes.len()));
        assert_eq!(value["media_type"], serde_json::json!("image/jpeg"));
    }

    #[test]
    fn upgrade_exec_image_ignores_missing_image() {
        let mut value = serde_json::json!({"ok": true, "content": "hi"});
        assert!(
            upgrade_exec_image(&mut value, None, &lofi_types::ImageConfig::default())
                .unwrap()
                .is_none()
        );
        assert_eq!(value["content"], serde_json::json!("hi"));
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
