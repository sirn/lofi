#![allow(clippy::wildcard_imports)]

use super::*;

mod messages;
mod progress;
mod recovery;
mod round;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(super) use recovery::MAX_RECOVERY_ROUNDS;
use recovery::*;

use messages::IMAGE_PRESSURE_SENTINEL;
use progress::{append_prompt, commit_progress, record_round_removal};
use round::RoundFailure;

#[cfg(test)]
pub(in crate::agent) use messages::{assistant_announces_tool_intent, assistant_is_thinking_only};

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

/// Input for a new prompt or a silent hard-compaction continuation.
pub enum TurnInput {
    Prompt {
        text: String,
        kind: lofi_types::PromptKind,
        attachments: Vec<ContentBlock>,
    },
    Continue,
}

pub struct TurnRequest {
    pub input: TurnInput,
    pub notices: Vec<String>,
}

impl TurnRequest {
    #[must_use]
    pub fn prompt(text: String, kind: lofi_types::PromptKind) -> Self {
        Self {
            input: TurnInput::Prompt {
                text,
                kind,
                attachments: Vec::new(),
            },
            notices: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct RunControls {
    pub cancel: Option<Arc<AtomicBool>>,
    pub preempt: Option<Arc<AtomicBool>>,
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
        self.run_turn(
            &mut messages,
            TurnRequest::prompt(user_prompt, lofi_types::PromptKind::User),
            tx,
            None,
            RunControls::default(),
        )
        .await
    }

    /// Run a submitted prompt or continue the current turn without a prompt.
    /// Notices use the same durable prompt path as notices generated during a run.
    /// # Errors
    /// Propagates provider, tool execution, and transcript storage errors.
    #[allow(clippy::too_many_lines)]
    pub async fn run_turn(
        &self,
        messages: &mut Vec<Message>,
        request: TurnRequest,
        tx: Sender<AgentEvent>,
        session: Option<&crate::session::store::SessionCursor>,
        controls: RunControls,
    ) -> Result<()> {
        let RunControls { cancel, preempt } = controls;
        let TurnRequest { input, notices } = request;
        // Rejected: trim from the TUI when the channel closes. The burst
        // is allocated and dropped on this task's return paths.
        let _trim = lofi_code::memory::ReleaseFreedMemoryOnDrop;
        let prev_len = messages.len();
        let prompt = match input {
            TurnInput::Continue => None,
            TurnInput::Prompt {
                text,
                kind,
                attachments,
            } => {
                let mut blocks = vec![ContentBlock::Text { text }];
                blocks.extend(attachments);
                Some(Message {
                    origin: None,
                    role: Role::User,
                    blocks,
                    kind,
                })
            }
        };
        let mut stats = TurnStats::new();
        let mut recorder =
            session.map(|cursor| SessionRecorder::new(cursor.clone(), self.run_model()));
        let mut starts_run = true;
        for notice in notices {
            let message = Message {
                origin: None,
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
        let mut retry = crate::retry::RetrySession::new(self.retry);
        // The image-omit notice fires once per turn (on the first round),
        // not once per tool round.
        let mut omit_notice_sent = false;
        // Terminal and loop recovery each get one attempt. A shared budget
        // would let one failure mode disable recovery from the other.
        let mut auto_continued = false;
        let mut loop_recovered = false;
        let mut loop_detector = LoopDetector::default();
        let mut recovery_rounds = 0usize;
        let exit = loop {
            if tx.is_closed() {
                break Ok(TurnOutcome::Detached);
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
                    break Ok(TurnOutcome::Detached);
                }
                break Ok(TurnOutcome::Finished);
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
                        break Ok(TurnOutcome::Cancelled);
                    }
                    let terminal_prompt = terminal_recovery_prompt(&outcome, self.auto_continue);
                    let mut plan = handle_loop_detection(
                        outcome.loop_detail.as_deref(),
                        outcome.interrupted_thinking_index,
                        &mut loop_recovered,
                    );
                    if plan.is_none() && finished {
                        let thinking_only_prompt = (outcome.stop_reason
                            != Some(lofi_types::StopReason::Other))
                        .then_some(terminal_prompt.unwrap_or(THINKING_ONLY_CONTINUATION_PROMPT));
                        plan = handle_thinking_only_stop(
                            outcome.thinking_only_index,
                            thinking_only_prompt,
                            &mut auto_continued,
                        );
                    }
                    if let Some(plan) = plan {
                        // Transcript durability must precede removal from model history.
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
                                break Err(error);
                            }
                            messages.remove(index);
                        }
                        // An orphan reasoning item must not enter the next provider request.
                        if !append_prompt(
                            messages,
                            plan.notice,
                            false,
                            recorder.as_mut(),
                            prev_len,
                            &stats,
                            &tx,
                        )
                        .await
                        {
                            break Ok(TurnOutcome::Detached);
                        }
                        match plan.action {
                            RecoveryAction::Continue => continue,
                            RecoveryAction::Stop => {
                                break Ok(if tx.is_closed() {
                                    TurnOutcome::Detached
                                } else {
                                    TurnOutcome::Finished
                                });
                            }
                        }
                    }
                    commit_progress(recorder.as_mut(), &messages[prev_len..], &stats, &tx).await;
                    if finished {
                        if !auto_continued {
                            if let Some(prompt) = terminal_prompt {
                                auto_continued = true;
                                let message = notice_message(prompt);
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
                                    break Ok(TurnOutcome::Detached);
                                }
                                continue;
                            }
                        }
                        break Ok(TurnOutcome::Finished);
                    }
                    if tx.is_closed() {
                        break Ok(TurnOutcome::Detached);
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
                            break Ok(TurnOutcome::ContextPressure);
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
                            break Ok(TurnOutcome::Finished);
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
                    break Ok(
                        if cancel
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Relaxed))
                        {
                            TurnOutcome::Cancelled
                        } else {
                            TurnOutcome::Detached
                        },
                    );
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
                        break Ok(TurnOutcome::ContextPressure);
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
                                break Err(record_error);
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
                            break Ok(
                                if cancel
                                    .as_ref()
                                    .is_some_and(|flag| flag.load(Ordering::Relaxed))
                                {
                                    TurnOutcome::Cancelled
                                } else {
                                    TurnOutcome::Detached
                                },
                            );
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
                    // Keep the prompt and every completed round: only the
                    // failed round's partial messages leave the context.
                    // The discard marker bounds the failed-turn skip on
                    // replay so live and resumed context agree.
                    if let Err(record_error) = record_round_removal(
                        recorder.as_mut(),
                        messages,
                        prev_len,
                        messages.len() - round_start,
                        &stats,
                        &tx,
                        &format!("discarding partial response after failure: {error}"),
                    )
                    .await
                    {
                        break Err(record_error);
                    }
                    messages.truncate(round_start);
                    break Err(error);
                }
            }
        };
        self.settle_turn(messages, prev_len, recorder, stats, &tx, exit)
            .await
    }
}
