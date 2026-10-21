//! The turn: one loop above every transport.
//!
//! A backend opens a turn-scoped driver and the loop asks it for one exchange:
//! produce the next assistant message, run the tools it asks for, say whether
//! the turn is done. Retry, recovery, auto-continue, discard, record, usage,
//! outcome and the terminal marker are decided here, once, for every
//! transport.

pub mod contract;
pub mod io;
mod record;
mod recovery;

pub use contract::{
    Anomaly, Exchange, ExchangeDriver, ExchangeFailure, Rollback, Settlement, ToolRecorder,
    TurnPolicy,
};
pub(crate) use io::{Admission, CancelToken, EventSink, ExchangeInput, TurnIo};
pub(crate) use record::{discard_round, record_notice, record_prompt};
#[cfg(test)]
pub(in crate::agent) use recovery::MAX_RECOVERY_ROUNDS;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use lofi_error::{Error, Result};
use lofi_types::{ContentBlock, Message, PromptKind, Role, StopReason};
use tokio::sync::mpsc::Sender;

use super::agent_run::messages::{
    assistant_announces_tool_intent, assistant_is_thinking_only, close_orphaned_tool_uses,
};
use super::turn::{emit_terminal, TurnDriver, TurnTerminal};
use super::{AgentEvent, ToolCallContext, ToolExecutor};
use crate::backend::Backend;
use crate::retry::RetrySession;
use crate::session::recorder::{SessionRecorder, TurnOutcome};
use crate::session::store::SessionCursor;

use recovery::{
    handle_loop_detection, handle_thinking_only_stop, notice_message, recovery_round_exceeded,
    terminal_recovery_prompt, RecoveryAction, THINKING_ONLY_PROMPT,
};

/// Input for a new prompt or a silent hard-compaction continuation.
pub enum TurnInput {
    Prompt {
        text: String,
        kind: PromptKind,
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
    pub fn prompt(text: String, kind: PromptKind) -> Self {
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
    pub cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub preempt: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Run one turn: record the input, drive the exchanges, settle the outcome.
///
/// # Errors
/// Propagates provider, tool, and transcript failures the turn cannot recover
/// from. A recovered failure is an outcome, not an error.
#[allow(clippy::too_many_lines)]
pub async fn run(
    backend: &dyn Backend,
    messages: &mut Vec<Message>,
    request: TurnRequest,
    events: Sender<AgentEvent>,
    session: Option<&SessionCursor>,
    controls: RunControls,
) -> Result<TurnOutcome> {
    let RunControls { cancel, preempt } = controls;
    let policy = backend.turn_policy();
    let executor = backend.tool_executor();
    let sink = EventSink::new(events, session.cloned());
    let run_model = backend.run_model();
    // The engine reports the model it runs so the header and the session marker
    // carry the same identity a child transport reports.
    if !sink
        .emit(AgentEvent::ModelReported {
            id: run_model.id.clone(),
            display: run_model.display_or_selected().to_string(),
        })
        .await
    {
        return Ok(TurnOutcome::Detached);
    }
    // Rejected: trim from the TUI when the channel closes. The burst is
    // allocated and dropped on this task's return paths.
    let _trim = lofi_code::memory::ReleaseFreedMemoryOnDrop;
    let prev_len = messages.len();
    let TurnRequest { input, notices } = request;
    let (prompt_message, exchange_input) = match input {
        TurnInput::Continue => (None, ExchangeInput::continuation()),
        TurnInput::Prompt {
            text,
            kind,
            attachments,
        } => {
            let mut blocks = vec![ContentBlock::Text { text: text.clone() }];
            blocks.extend(attachments.clone());
            let message = Message {
                origin: None,
                role: Role::User,
                blocks,
                kind,
            };
            let input = ExchangeInput {
                text: Some(text),
                kind,
                attachments,
                notices: notices.clone(),
            };
            (Some(message), input)
        }
    };
    // Refresh the auto-mode context from the history the turn starts with.
    // Deriving it here (instead of accumulating it as the turn runs) is what
    // makes compaction, rewind, and a new session drop the context: the scan
    // simply stops finding the old requests.
    let typed_prompt = prompt_message
        .as_ref()
        .and_then(super::auto_mode::typed_prompt)
        .map(str::to_string);
    *executor
        .auto_mode_hints
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        super::auto_mode::derive_hints(messages, typed_prompt.as_deref());
    // Background subagents and job drivers need a runtime that outlives this
    // turn's worker runtime. Both registries capture one at construction; these
    // calls repair registries built outside any runtime and are no-ops otherwise
    // (first-wins).
    let subagents = executor.subagents();
    subagents.set_host_handle(tokio::runtime::Handle::current());
    executor
        .jobs()
        .set_driver_runtime(tokio::runtime::Handle::current());
    // Stamp the live session id so spawned subagent transcripts link back to
    // this session; /new and /resume switch cursors between turns.
    if let Some(cursor) = session {
        if let Some(stem) = cursor.path().file_stem().and_then(|s| s.to_str()) {
            subagents.set_parent_session(stem);
        }
        // A restarted process rebuilds the registry from this session's
        // subagent transcripts so children stay resumable: once per parent
        // session id, before any tool of the turn runs.
        if let Some(support) = executor.subagent_support() {
            subagents.rehydrate(backend, &support);
        }
    }
    let cancel = CancelToken::new(cancel);
    let mut driver = backend.open_turn()?;
    let admission = Admission {
        frontier: session.and_then(SessionCursor::leaf_id),
        upto: prev_len,
    };
    let recorder = session.map(|cursor| SessionRecorder::new(cursor.clone(), run_model.clone()));
    let context = tool_context(&sink, session, &cancel, driver.tool_recorder());
    let io = TurnIo::new(
        sink,
        recorder,
        admission,
        exchange_input,
        context,
        cancel,
        session.cloned(),
    );
    // The turn's own lifetime reaches the transports: a driver may keep a
    // `TurnIo` clone on a worker thread, and nothing below the seam may record
    // after the turn dropped.
    let _reaper = io.reaper();
    let mut driver_view = TurnDriver::new(io.events().sender().clone());
    let mut auto_continued = false;
    let mut loop_recovered = false;
    let mut recovery_rounds = 0usize;
    let mut retry = RetrySession::new(policy.retry);
    let mut opened = false;
    let exit = 'turn: loop {
        if io.events().is_closed() {
            break Ok(TurnOutcome::Detached);
        }
        if !opened {
            // One turn, one opening: the driver announces it at the first
            // prompt and publishes every boundary after that.
            for notice in &notices {
                let message = notice_message(notice.clone());
                match record_prompt(&mut driver_view, &io, messages, message).await {
                    Ok(true) => {}
                    Ok(false) => break 'turn Ok(TurnOutcome::Detached),
                    Err(error) => break 'turn Err(error),
                }
            }
            match prompt_message.clone() {
                Some(message) => {
                    match record_prompt(&mut driver_view, &io, messages, message).await {
                        Ok(true) => {}
                        Ok(false) => break 'turn Ok(TurnOutcome::Detached),
                        Err(error) => break 'turn Err(error),
                    }
                }
                None => {
                    if !driver_view.continuation().await {
                        break 'turn Ok(TurnOutcome::Detached);
                    }
                }
            }
            opened = true;
        }
        if let Some(message) = recovery_round_exceeded(&mut recovery_rounds) {
            match record_notice(&mut driver_view, &io, messages, message).await {
                Ok(true) => {}
                Ok(false) => break 'turn Ok(TurnOutcome::Detached),
                Err(error) => break 'turn Err(error),
            }
            break Ok(TurnOutcome::Finished);
        }
        let round_start = messages.len();
        let result = driver.run(&io, messages).await;
        // Publish the identity of the attempt, also when it failed: a child
        // reports the model it actually ran before the failure, and the
        // transcript must name that model rather than the selection.
        io.set_model(driver.run_model());
        if result.is_ok() {
            if let Some(attempt) = retry.reset_after_progress() {
                let _ = io
                    .events()
                    .emit(AgentEvent::RetryEnd {
                        success: true,
                        attempt,
                        final_error: None,
                    })
                    .await;
            }
        }
        match result {
            Ok(exchange) => {
                io.events()
                    .accounting()
                    .set_stop_reason(exchange.stop_reason);
                // A settled exchange may have left subagents that finished
                // during it; report their spend now, not only at the end, so a
                // detached or killed turn has already published it.
                flush_subagent_usage(&executor, io.events()).await;
                // An explicit interrupt wins races against both a normal
                // terminal response and queued-prompt preemption. A native tool
                // can observe cancellation, settle its process group, and still
                // return a structured result; without this boundary check that
                // cancelled turn is recorded as done.
                if io.cancel().is_cancelled() {
                    break Ok(TurnOutcome::Cancelled);
                }
                // A consumer that left records nothing: a repair prompt written
                // after it closed would sit in a transcript no one sent it to.
                if io.events().is_closed() {
                    break Ok(TurnOutcome::Detached);
                }
                let assistant = terminal_assistant(messages, round_start);
                let announced = assistant
                    .is_some_and(|index| assistant_announces_tool_intent(&messages[index]));
                let thinking_only_index =
                    assistant.filter(|index| assistant_is_thinking_only(&messages[*index]));
                let terminal_prompt =
                    terminal_recovery_prompt(&exchange, announced, policy.auto_continue);
                let mut plan = exchange.anomaly.as_ref().and_then(|anomaly| match anomaly {
                    Anomaly::LoopDetected { detail, removal } => {
                        handle_loop_detection(Some(detail), *removal, &mut loop_recovered)
                    }
                });
                if plan.is_none() && exchange.finished {
                    let thinking_only_prompt = (exchange.stop_reason != Some(StopReason::Other))
                        .then_some(terminal_prompt.unwrap_or(THINKING_ONLY_PROMPT));
                    plan = handle_thinking_only_stop(
                        thinking_only_index,
                        thinking_only_prompt,
                        &mut auto_continued,
                    );
                }
                if let Some(plan) = plan {
                    // Transcript durability must precede removal from model history.
                    if let Some((index, detail)) = plan.removal {
                        if let Err(error) = discard_round(&io, messages, 1, &detail).await {
                            break 'turn Err(error);
                        }
                        messages.remove(index);
                    }
                    // An orphan reasoning item must not enter the next provider request.
                    match record_notice(&mut driver_view, &io, messages, plan.notice).await {
                        Ok(true) => {}
                        Ok(false) => break 'turn Ok(TurnOutcome::Detached),
                        Err(error) => break 'turn Err(error),
                    }
                    match plan.action {
                        // The repair asks again, so the cap that ends the turn
                        // belongs here too: a recovery round must not push a
                        // full context over the window.
                        RecoveryAction::Continue => {
                            if hard_cap_exceeded(&io, &policy) {
                                break Ok(TurnOutcome::ContextPressure);
                            }
                            continue;
                        }
                        RecoveryAction::Stop => {
                            break Ok(if io.events().is_closed() {
                                TurnOutcome::Detached
                            } else {
                                TurnOutcome::Finished
                            });
                        }
                    }
                }
                io.commit(messages).await;
                // Hard context cap: the round just completed (its tool result is
                // in hand, so the latest turn is a matched tool cycle that
                // compaction keeps verbatim). Stop before the next round would
                // overflow the window and let the UI force-compact + continue.
                if hard_cap_exceeded(&io, &policy) {
                    break Ok(TurnOutcome::ContextPressure);
                }
                if exchange.finished {
                    if !auto_continued {
                        if let Some(prompt) = terminal_prompt {
                            auto_continued = true;
                            let message = notice_message(prompt);
                            match record_notice(&mut driver_view, &io, messages, message).await {
                                Ok(true) => {}
                                Ok(false) => break 'turn Ok(TurnOutcome::Detached),
                                Err(error) => break 'turn Err(error),
                            }
                            continue;
                        }
                    }
                    break Ok(if io.events().is_closed() {
                        TurnOutcome::Detached
                    } else {
                        TurnOutcome::Finished
                    });
                }
                if io.events().is_closed() {
                    break Ok(TurnOutcome::Detached);
                }
                recovery_rounds = 0;
                // Preempt: the UI has a queued prompt and wants the current turn
                // to end gracefully after this round so the queued prompt can be
                // sent at the earliest opportunity.
                if let Some(flag) = &preempt {
                    if flag.load(Ordering::Relaxed) {
                        break Ok(TurnOutcome::Finished);
                    }
                }
            }
            Err(ExchangeFailure {
                error: Error::Cancelled,
                ..
            }) => {
                keep_partial(messages, round_start);
                // A set cancel flag is an explicit user interruption; a closed
                // receiver is only a detached consumer.
                break Ok(if io.cancel().is_cancelled() {
                    TurnOutcome::Cancelled
                } else {
                    TurnOutcome::Detached
                });
            }
            Err(ExchangeFailure {
                error,
                progress,
                rollback,
            }) => {
                // A cancellation or a closed consumer is not a retryable
                // transport failure: mapping it before any recovery path keeps
                // the partial the user saw, exactly as the cancelled exchange
                // does, and a recovery the user cannot see never runs.
                if io.cancel().is_cancelled() {
                    keep_partial(messages, round_start);
                    break Ok(TurnOutcome::Cancelled);
                }
                if io.events().is_closed() {
                    keep_partial(messages, round_start);
                    break Ok(TurnOutcome::Detached);
                }
                // An oversized image payload stopped before send; route to the
                // force-compact + continue recovery. The round never ran, so the
                // partial-turn suffix is empty and the recorder's
                // ContextPressure arm leaves the durable transcript untouched.
                if is_context_pressure(&error) {
                    break Ok(TurnOutcome::ContextPressure);
                }
                if progress {
                    if let Some(attempt) = retry.reset_after_progress() {
                        let _ = io
                            .events()
                            .emit(AgentEvent::RetryEnd {
                                success: false,
                                attempt,
                                final_error: Some(error.to_string()),
                            })
                            .await;
                    }
                }
                let retrying = matches!(rollback, Rollback::Retry);
                let plan = if retrying { retry.retry(&error) } else { None };
                if let Some(plan) = plan {
                    if messages.len() > round_start {
                        let discarded = discard_round(
                            &io,
                            messages,
                            messages.len() - round_start,
                            &format!("discarding partial response before retry: {error}"),
                        )
                        .await;
                        if let Err(discard_error) = discarded {
                            // The round is still here, so the retry that would
                            // have followed it never opened a pair.
                            retry.abandon();
                            keep_partial(messages, round_start);
                            let _ = io.commit_durable(messages).await;
                            break 'turn Err(discard_error);
                        }
                        messages.truncate(round_start);
                    }
                    let _ = io
                        .events()
                        .emit(AgentEvent::RetryStart {
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
                        "request failed; retry scheduled"
                    );
                    // Cancellable backoff: stop promptly for Ctrl-C or a closed
                    // consumer instead of waiting out the retry delay and
                    // issuing another request.
                    let ready = tokio::select! {
                        biased;
                        () = io.cancel().wait() => false,
                        () = io.events().closed() => false,
                        () = tokio::time::sleep(plan.delay) => true,
                    };
                    if !ready {
                        // The retry never ran: close the pair the start opened,
                        // so no consumer waits for a round that will not come.
                        // The reset is what closes it, so `finish` stays quiet.
                        let _ = retry.reset_after_progress();
                        let _ = io
                            .events()
                            .emit(AgentEvent::RetryEnd {
                                success: false,
                                attempt: plan.attempt,
                                final_error: Some(error.to_string()),
                            })
                            .await;
                        break Ok(if io.cancel().is_cancelled() {
                            TurnOutcome::Cancelled
                        } else {
                            TurnOutcome::Detached
                        });
                    }
                    continue;
                }
                if let Some(attempt) = retry.reset_after_progress() {
                    tracing::warn!(
                        attempt,
                        error = %error,
                        "request failed; retry budget exhausted"
                    );
                    let _ = io
                        .events()
                        .emit(AgentEvent::RetryEnd {
                            success: false,
                            attempt,
                            final_error: Some(error.to_string()),
                        })
                        .await;
                }
                // A transport that may not be replayed keeps what its exchange
                // completed: it can run several tool cycles before the call that
                // failed, and one discard marker cannot describe that. A retried
                // exchange leaves only its own partial messages behind, and the
                // discard marker bounds the failed-turn skip on replay so live
                // and resumed context agree.
                // A discard boundary exists only where the history grew: a
                // request that never appended anything leaves nothing to bound,
                // and replay reads the marker as "skip the tail".
                if rollback == Rollback::Retry && messages.len() > round_start {
                    let discarded = discard_round(
                        &io,
                        messages,
                        messages.len() - round_start,
                        &format!("discarding partial response after failure: {error}"),
                    )
                    .await;
                    if let Err(discard_error) = discarded {
                        // Nothing was removed, so the round stays and its
                        // unanswered calls close before the next request.
                        keep_partial(messages, round_start);
                        let _ = io.commit_durable(messages).await;
                        break 'turn Err(discard_error);
                    }
                    messages.truncate(round_start);
                }
                // A stop keeps what the exchange completed, so the tail must be
                // provider-valid: a call whose result never arrived — the child
                // died mid-tool — closes with the synthetic result, exactly as
                // a cancellation does. The next request on either transport
                // then carries a matched pair. The closing result is durable
                // before the turn ends, so a resumed session rebuilds the pair
                // the live history has.
                if rollback == Rollback::Stop && messages.len() > round_start {
                    keep_partial(messages, round_start);
                    if let Err(commit_error) = io.commit_durable(messages).await {
                        break 'turn Err(commit_error);
                    }
                }
                break Err(error);
            }
        }
    };
    finish(&io, &executor, &mut driver, &mut retry, messages, exit).await
}

/// The terminal answer an exchange appended, when it appended one.
///
/// A CLI child flushes reasoning as its own assistant message before a tool
/// cycle, so the round may start with a message that did not answer.
pub(super) fn terminal_assistant(messages: &[Message], round_start: usize) -> Option<usize> {
    (round_start..messages.len()).rfind(|index| messages[*index].role == Role::Assistant)
}

/// The round's partial stays in the history: its unanswered tool calls must
/// be closed, or a provider that requires a result for every call rejects the
/// next request.
fn keep_partial(messages: &mut Vec<Message>, round_start: usize) {
    if messages.len() > round_start {
        close_orphaned_tool_uses(messages);
    }
}

fn hard_cap_exceeded(io: &TurnIo, policy: &TurnPolicy) -> bool {
    policy
        .hard_cap
        .is_some_and(|cap| io.fill().is_some_and(|fill| fill > cap))
}

fn is_context_pressure(error: &Error) -> bool {
    matches!(error, Error::ContextPressure(_))
}

/// The sources a tool call needs: the transcript it may read, the message it
/// may recover, the job it may own.
fn tool_context(
    sink: &EventSink,
    session: Option<&SessionCursor>,
    cancel: &CancelToken,
    record_tool: Option<super::ToolRecorder>,
) -> ToolCallContext {
    let recall = session.map(|cursor| {
        let cursor = cursor.clone();
        Arc::new(move |request: &lofi_types::recall::RecallRequest| {
            crate::recall::recall_cursor(&cursor, request)
        }) as lofi_code::RecallFn
    });
    let result = session.map(|cursor| {
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
        }) as lofi_code::ResultFn
    });
    let on_job_acquired = session.map(|cursor| {
        let cursor = cursor.clone();
        Arc::new(move |id| {
            let owner = cursor.record_job_started(id).ok()?;
            let cursor = cursor.clone();
            Some(Arc::new(move |id| {
                let _ = cursor.record_job_finished(&owner, id);
            }) as lofi_code::JobReleaseFn)
        }) as lofi_code::JobAcquireFn
    });
    ToolCallContext {
        events: Some(sink.clone()),
        session: session.cloned(),
        recall,
        result,
        cancel: cancel.flag().cloned(),
        on_job_acquired,
        record_tool,
        // Sealed by `TurnIo::new` once the turn exists.
        alive: None,
    }
}

async fn flush_subagent_usage(executor: &ToolExecutor, sink: &EventSink) {
    let Some((cost, usage)) = executor.subagents().take_unreported() else {
        return;
    };
    sink.emit(AgentEvent::SubagentUsage { cost, usage }).await;
}

/// Settle the turn: last subagent flush, the transcript, the transport's own
/// bookkeeping, then the terminal marker.
async fn finish(
    io: &TurnIo,
    executor: &ToolExecutor,
    driver: &mut Box<dyn ExchangeDriver>,
    retry: &mut RetrySession,
    messages: &[Message],
    exit: Result<TurnOutcome>,
) -> Result<TurnOutcome> {
    let (outcome, err) = match exit {
        Ok(outcome) => (outcome, None),
        Err(error) => (TurnOutcome::Failed(error.to_string()), Some(error)),
    };
    // Every break out of the loop passes here, so this is where a retry that
    // never ran closes its pair: a consumer that waits for the end of a
    // started attempt outlives the turn otherwise.
    if let Some(attempt) = retry.reset_after_progress() {
        let _ = io
            .events()
            .emit(AgentEvent::RetryEnd {
                success: false,
                attempt,
                final_error: None,
            })
            .await;
    }
    // Last chance to record spend of subagents that settled after the final
    // tool round; after this point the turn is over.
    flush_subagent_usage(executor, io.events()).await;
    // Disk first: a crash after the UI terminal event must not lose a settled
    // turn that was still only in memory.
    let mut committed = None;
    let mut record_error = None;
    match record::finish(io, messages, &outcome) {
        Ok(range) => committed = range,
        // The turn still settles and still reports its outcome: the driver must
        // learn that no transcript landed, and a consumer that never sees a
        // terminal marker waits out the turn. The caller gets the error.
        Err(error) => record_error = Some(error),
    }
    // The turn made its record decision: nothing may extend the transcript
    // after it, not even a tool result a transport worker settles late. A
    // failed write sealed just the same — the marker is missing, not the
    // decision.
    io.seal();
    let (recorded, leaf) = {
        let recorder = io.recorder();
        match recorder.as_ref() {
            // The transcript holds this turn when its leaf moved past the
            // frontier the turn was admitted at. A turn whose bytes all landed
            // in an earlier round publishes nothing here and still recorded.
            Some(recorder) => (
                record_error.is_none()
                    && (committed.is_some()
                        || recorder.cursor().leaf_id() != io.frontier().frontier),
                recorder.cursor().leaf_id(),
            ),
            None => (true, None),
        }
    };
    driver.settle(io, Settlement { recorded, leaf }).await;
    let (cost, usage, stop_reason) = {
        let accounting = io.events().accounting();
        (
            accounting.cost(),
            accounting.usage(),
            accounting.stop_reason(),
        )
    };
    emit_terminal(
        io.events().sender(),
        TurnTerminal {
            model: driver.run_model(),
            outcome: outcome.clone(),
            elapsed_ms: io.elapsed_ms(),
            cost,
            usage,
            stop_reason,
            committed,
        },
    )
    .await;
    match err.or(record_error) {
        Some(error) => Err(error),
        None => Ok(outcome),
    }
}
