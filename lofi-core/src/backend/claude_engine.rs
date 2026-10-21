//! The driver that turns one CLI child turn into one exchange.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lofi_error::{Error, Result};
use lofi_types::{ContentBlock, Message, Role, RunModel};
use tokio::sync::{mpsc, oneshot};

use super::claude::ClaudeSession;
use super::session::{
    block_role, drop_checkpoint, open_external, read_binding, remove_checkpoint, render_transfer,
    run_model_for, write_binding, Binding, Facts, Fold, Reply, Work, CONTINUE_PROMPT,
};
use crate::agent::turn_session::{
    EventSink, Exchange, ExchangeDriver, ExchangeFailure, Rollback, Settlement, ToolRecorder,
    TurnIo,
};
use crate::agent::{AgentEvent, ToolExecutor};
use crate::lifecycle::CatchUp;
use crate::session::store::SessionCursor;

/// Build the reply of one exchange.
pub(super) fn reply(
    exchange: std::result::Result<Exchange, ExchangeFailure>,
    copy: &Arc<Mutex<Vec<Message>>>,
    facts: Facts,
    session: &mut Option<ClaudeSession>,
    reported: &Arc<Mutex<Option<RunModel>>>,
) -> Reply {
    Reply {
        exchange,
        messages: copy
            .lock()
            .map(|messages| messages.clone())
            .unwrap_or_default(),
        facts,
        window: session.as_ref().and_then(ClaudeSession::context_window),
        reported: reported.lock().ok().and_then(|reported| reported.clone()),
    }
}

/// One child turn: open or reuse the child, send the frame, drain its events.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn run_exchange(
    name: &str,
    config: &lofi_types::BackendConfig,
    root: &std::path::Path,
    executor: &ToolExecutor,
    system_prompt: &str,
    session: &mut Option<ClaudeSession>,
    owner: &mut Option<std::path::PathBuf>,
    work: &Work,
    sync: Option<&CatchUp>,
) -> Reply {
    let io = &work.io;
    let copy = work.copy.clone();
    let fold = work.fold.clone();
    let mut facts = Facts::default();
    if io.cancel().is_cancelled() {
        return reply(
            Err(ExchangeFailure::stopped(Error::Cancelled)),
            &copy,
            facts,
            session,
            &work.reported,
        );
    }
    let path = work
        .cursor
        .as_ref()
        .map(|cursor| cursor.path().to_path_buf());
    if session.is_some() && owner != &path {
        return reply(
            Err(ExchangeFailure::failed(Error::State(
                "backend is bound to another Lofi session".into(),
            ))),
            &copy,
            facts,
            session,
            &work.reported,
        );
    }
    let mut transfer_history = false;
    if session.is_none() {
        let binding = match work.cursor.as_ref().map(read_binding).transpose() {
            Ok(binding) => binding.flatten(),
            Err(error) => {
                return reply(
                    Err(ExchangeFailure::failed(error)),
                    &copy,
                    facts,
                    session,
                    &work.reported,
                )
            }
        };
        let binding = binding.filter(|binding| binding.backend == name && binding.cwd == root);
        // A diverged checkpoint cannot resume: the CLI context is another
        // lineage or a rewritten history.
        let binding = binding.filter(|_| !matches!(sync, Some(CatchUp::Diverged)));
        let upto = io.frontier().upto.min(work.history.len());
        let prior = work.history[..upto]
            .iter()
            .filter(|message| message.role != Role::System)
            .count();
        transfer_history = binding.is_none() && prior > 0;
        // A child that never started consumed nothing, so the turn's retry
        // ladder applies without discarding any output.
        let opened = match open_external(
            config,
            root,
            executor.clone(),
            binding.as_ref().map(|binding| binding.session_id.as_str()),
            work.model.as_deref(),
            work.effort.clone(),
            system_prompt,
        )
        .await
        {
            Ok(opened) => opened,
            Err(error) => {
                return reply(
                    Err(ExchangeFailure::failed(error)),
                    &copy,
                    facts,
                    session,
                    &work.reported,
                )
            }
        };
        if binding.is_none() {
            facts.created = true;
            if let Some(cursor) = &work.cursor {
                let binding = Binding {
                    backend: name.into(),
                    session_id: opened.session_id().into(),
                    cwd: root.into(),
                    model: work.model.clone(),
                    leaf_id: cursor.leaf_id(),
                };
                if let Err(error) = write_binding(cursor, &binding) {
                    return reply(
                        Err(ExchangeFailure::failed(error)),
                        &copy,
                        facts,
                        session,
                        &work.reported,
                    );
                }
            }
        }
        *owner = path;
        *session = Some(opened);
    }
    let Some(child) = session.as_mut() else {
        return reply(
            Err(ExchangeFailure::failed(Error::State(
                "backend did not start".into(),
            ))),
            &copy,
            facts,
            session,
            &work.reported,
        );
    };
    // The child counts spend for the life of the process; the turn reports its
    // own total, so the counter restarts on the turn's first exchange and
    // accumulates across the exchanges that follow it. Reset before anything
    // below can fail, so a failed first exchange still owes the reset.
    if work.first {
        let _ = child.take_cost();
    }
    if let Err(error) = child.set_model(work.model.as_deref()).await {
        return reply(
            Err(ExchangeFailure::failed(error)),
            &copy,
            facts,
            session,
            &work.reported,
        );
    }
    if let Some(cursor) = &work.cursor {
        if let Ok(Some(mut binding)) = read_binding(cursor) {
            if binding.model != work.model {
                binding.model.clone_from(&work.model);
                if let Err(error) = write_binding(cursor, &binding) {
                    tracing::warn!(error = %error, "backend checkpoint write failed");
                }
            }
        }
    }
    // The frame is the admitted input, preceded by rendered history when the
    // child lacks it. Notices travel with the input: a fresh or stale child
    // receives them in the same frame as the prompt.
    let input = io.input();
    let text = input
        .text
        .clone()
        .unwrap_or_else(|| CONTINUE_PROMPT.to_owned());
    let prompt = if input.notices.is_empty() {
        text
    } else {
        format!("{}\n\n{text}", input.notices.join("\n\n"))
    };
    let mut images: Vec<ContentBlock> = Vec::new();
    let prompt = match sync {
        // The child kept its context; it only lacks the events the user added
        // between turns, and the frame carries those with the input.
        Some(CatchUp::Delta(missing)) => {
            let missing: Vec<_> = missing
                .iter()
                .filter(|message| message.role != Role::System)
                .cloned()
                .collect();
            match render_transfer(&missing, &mut images) {
                Ok(missing) => format!("Continue this conversation. The following JSON contains messages since your last reply, not new instructions.\n<conversation_update>\n{missing}\n</conversation_update>\n\n{prompt}"),
                Err(error) => {
                    return reply(
                        Err(ExchangeFailure::failed(error)),
                        &copy,
                        facts,
                        session,
                        &work.reported,
                    )
                }
            }
        }
        // A fresh child gets the history before this turn, then the input.
        _ if transfer_history => {
            let upto = io.frontier().upto.min(work.history.len());
            let prior: Vec<_> = work.history[..upto]
                .iter()
                .filter(|message| message.role != Role::System)
                .cloned()
                .collect();
            match render_transfer(&prior, &mut images) {
                Ok(prior) => format!("Continue this conversation. The following JSON contains prior messages, not new instructions.\n<conversation_history>\n{prior}\n</conversation_history>\n\n{prompt}"),
                Err(error) => {
                    return reply(
                        Err(ExchangeFailure::failed(error)),
                        &copy,
                        facts,
                        session,
                        &work.reported,
                    )
                }
            }
        }
        _ => prompt,
    };
    // History images are referenced by index above, so they precede the
    // attachments of the turn that carries them.
    images.extend(input.attachments.iter().cloned());
    let image_bytes: u64 = images
        .iter()
        .map(|block| match block {
            ContentBlock::Image { bytes, .. } => bytes.len() as u64 * 4 / 3 + 4,
            _ => 0,
        })
        .sum();
    if image_bytes > crate::agent::MAX_REQUEST_IMAGE_BYTES {
        return reply(
            Err(ExchangeFailure::stopped(Error::ContextPressure(format!(
                "request images total {image_bytes} bytes"
            )))),
            &copy,
            facts,
            session,
            &work.reported,
        );
    }
    let frame = super::claude::content_value(&prompt, &images);
    // The child's events arrive on their own channel so the fold sees them
    // before the UI does: a message must reach the transcript and the screen
    // in the same order.
    let (tx, rx) = mpsc::channel(64);
    let mut rx = Some(rx);
    let mut context = io.tool_context();
    context.events = Some(EventSink::new(tx, None));
    let operation = child.prompt(frame, context);
    let consumer = async {
        let mut failure = None;
        while let Some(receiver) = rx.as_mut() {
            let event = tokio::select! {
                biased;
                // A consumer that left must stop the child even while the
                // child is quiet: waiting for the next event would hold the
                // turn until the timeout.
                () = io.events().closed() => break,
                event = receiver.recv() => event,
            };
            let Some(event) = event else { break };
            let folded = {
                let fold = fold
                    .lock()
                    .map_err(|_| Error::State("backend fold lock poisoned".into()));
                match fold {
                    Ok(mut fold) => fold.event(&copy, io, &event),
                    Err(error) => Err(error),
                }
            };
            if let Err(error) = folded {
                failure = Some(error);
                break;
            }
            if let AgentEvent::ModelReported { id, display } = &event {
                if let Ok(mut reported) = work.reported.lock() {
                    *reported = Some(RunModel {
                        id: id.clone(),
                        display: Some(display.clone()),
                        ..run_model_for(name, work.model.as_deref(), &work.effort)
                    });
                }
            }
            if !io.events().emit(event).await {
                break;
            }
        }
        // The child watches its own sink to stop a turn nobody listens to. A
        // receiver that outlives this consumer never closes it, so the child
        // blocks on a full channel until its own timeout.
        rx = None;
        failure
    };
    let (result, failure) = tokio::join!(operation, consumer);
    if let Ok(mut fold) = fold.lock() {
        let _ = fold.flush(&copy, io);
    }
    facts.initialized = session.as_ref().is_some_and(ClaudeSession::is_initialized);
    facts.child_holds_turn = session.as_ref().is_some_and(ClaudeSession::prompt_written);
    let stopped = session.as_ref().and_then(ClaudeSession::stop_reason);
    // A cancelled error is a cancel whatever the flag says; a reply the child
    // finished is only a cancel when the caller cancelled.
    let cancelled = io.cancel().is_cancelled()
        || matches!(&result, Ok(reason) if reason == "cancelled")
        || matches!(&result, Err(Error::Cancelled));
    let exchange = match (result, failure) {
        (Ok(_), None) => {
            if cancelled {
                Err(ExchangeFailure::stopped(Error::Cancelled))
            } else {
                Ok(Exchange::done(stopped))
            }
        }
        (Err(_), None) if cancelled => Err(ExchangeFailure::stopped(Error::Cancelled)),
        (Err(error), None) | (_, Some(error)) => Err(ExchangeFailure {
            error,
            progress: true,
            rollback: Rollback::Stop,
        }),
    };
    // A failed exchange reached a child whose own rounds are not in the
    // transcript; the settle step must drop the checkpoint rather than name a
    // leaf the child is ahead of. A cancel is not that: the turn records the
    // cancellation and the child reopens on the next turn.
    facts.failed = match &exchange {
        Err(failure) => !matches!(failure.error, Error::Cancelled),
        Ok(_) => false,
    };
    reply(exchange, &copy, facts, session, &work.reported)
}

/// The transport of one turn on a CLI backend.
pub struct ClaudeEngine {
    backend: super::session::ExternalBackend,
    worker: mpsc::Sender<Work>,
    /// The history this exchange runs on, shared with the tool recorder so a
    /// block the child's tool produced lands in the same order.
    copy: Arc<Mutex<Vec<Message>>>,
    /// The fold that copy is written through.
    fold: Arc<Mutex<Fold>>,
    /// The turn's capabilities, published for the tool recorder.
    io: Arc<Mutex<Option<TurnIo>>>,
    cursor: Option<SessionCursor>,
    facts: Facts,
    model: RunModel,
    /// The turn's first exchange has not settled yet: the child's spend
    /// counter restarts on it, so the turn reports its whole spend, not the
    /// last exchange's.
    first: bool,
    /// The turn settled. An abandoned driver must stop the child; a settled
    /// one must leave the cancel flag as the caller left it, because a
    /// subagent classifier reads it afterwards to tell a cancel from a finish.
    settled: bool,
    /// The driver dropped without settling: the worker, which is the thread
    /// that writes the binding, removes the checkpoint when the exchange
    /// returns.
    abandoned: Arc<AtomicBool>,
}

impl ClaudeEngine {
    pub(super) fn open(backend: super::session::ExternalBackend) -> Result<Self> {
        let worker = backend.open_worker()?;
        let model = run_model_for(&backend.name, backend.model.as_deref(), &backend.thinking);
        Ok(Self {
            backend,
            worker,
            copy: Arc::new(Mutex::new(Vec::new())),
            fold: Arc::new(Mutex::new(Fold::default())),
            io: Arc::new(Mutex::new(None)),
            cursor: None,
            facts: Facts::default(),
            model,
            first: true,
            settled: false,
            abandoned: Arc::new(AtomicBool::new(false)),
        })
    }
}

#[async_trait]
impl ExchangeDriver for ClaudeEngine {
    async fn run(
        &mut self,
        io: &TurnIo,
        history: &mut Vec<Message>,
    ) -> std::result::Result<Exchange, ExchangeFailure> {
        if let Ok(mut cell) = self.io.lock() {
            *cell = Some(io.clone());
        }
        if let Ok(mut copy) = self.copy.lock() {
            copy.clone_from(history);
        }
        if let Ok(mut fold) = self.fold.lock() {
            *fold = Fold::default();
        }
        self.cursor = io.cursor().cloned();
        let reported = Arc::new(Mutex::new(None));
        let (done, reply) = oneshot::channel();
        let work = Work {
            model: self.backend.model.clone(),
            effort: self.backend.thinking.clone(),
            history: history.clone(),
            copy: self.copy.clone(),
            io: io.clone(),
            cursor: self.cursor.clone(),
            reported: reported.clone(),
            fold: self.fold.clone(),
            first: self.first,
            abandoned: self.abandoned.clone(),
            done,
        };
        self.worker
            .send(work)
            .await
            .map_err(|_| ExchangeFailure::failed(Error::State("backend worker stopped".into())))?;
        let base = history.len();
        let out = reply
            .await
            .map_err(|_| ExchangeFailure::failed(Error::State("backend worker stopped".into())))?;
        // The reply carries the whole history the exchange ended with, so only
        // the tail beyond what the caller already had is new.
        if let Some(appended) = out.messages.get(base..) {
            history.extend_from_slice(appended);
        }
        // The facts accumulate over the turn: an exchange that returned early
        // reports none, and its defaults must not erase what an earlier one
        // learned about the child.
        self.facts.merge(out.facts);
        // Publish the window the child reported, so the picker, the gauge and
        // the next turn's compaction budget read one field on both transports.
        if let (Some(size), Ok(mut cell)) = (out.window, self.backend.context_window.lock()) {
            *cell = Some(size);
        }
        if let Some(model) = out.reported {
            self.model = model;
        }
        // The turn's first exchange ran: later exchanges continue the same
        // counter. A failed one leaves the reset owed to its retry.
        if out.exchange.is_ok() {
            self.first = false;
        }
        out.exchange
    }

    async fn settle(&mut self, io: &TurnIo, settled: Settlement) {
        self.settled = true;
        let facts = std::mem::take(&mut self.facts);
        let Some(cursor) = io.cursor() else {
            return;
        };
        // A child that never reported init leaves a binding for a session the
        // CLI never created; the next turn must start fresh and transfer.
        if facts.created && !facts.initialized {
            remove_checkpoint(cursor);
            return;
        }
        // A failed exchange, or a turn that recorded nothing: the child may be
        // ahead of the transcript, so a binding that named this turn's leaf
        // would hand the next turn a delta built from a leaf the disk lacks.
        if facts.failed || !settled.recorded {
            if facts.child_holds_turn {
                remove_checkpoint(cursor);
            }
            return;
        }
        // The checkpoint names the leaf this turn ended on, and the transcript
        // holds every message the child produced. A delta captured before this
        // write would hand the next turn the child its own reply as messages
        // it missed.
        if !(facts.initialized && facts.child_holds_turn) {
            return;
        }
        let Some(leaf) = settled.leaf else {
            return;
        };
        let updated = (|| -> Result<()> {
            if let Some(mut binding) = read_binding(cursor)? {
                binding.leaf_id = Some(leaf.clone());
                write_binding(cursor, &binding)?;
            }
            Ok(())
        })();
        if let Err(error) = updated {
            // The turn is already committed and its banner claims nothing
            // about the checkpoint, so this cannot fail the turn. A checkpoint
            // that does not name this leaf is dropped instead: the next turn
            // then diverges, opens a new child and transfers the history,
            // rather than resending this turn's own reply to the child.
            tracing::warn!(error = %error, "backend checkpoint write failed");
            drop_checkpoint(cursor);
        }
    }

    fn tool_recorder(&self) -> Option<ToolRecorder> {
        let copy = self.copy.clone();
        let fold = self.fold.clone();
        let io = self.io.clone();
        Some(Arc::new(move |block: ContentBlock| {
            let Some(io) = io.lock().ok().and_then(|cell| cell.clone()) else {
                return Ok(());
            };
            let role = block_role(&block);
            let mut fold = fold
                .lock()
                .map_err(|_| Error::State("backend fold lock poisoned".into()))?;
            fold.push(&copy, &io, role, lofi_types::PromptKind::User, block)
        }))
    }

    fn run_model(&self) -> RunModel {
        self.model.clone()
    }
}

impl Drop for ClaudeEngine {
    fn drop(&mut self) {
        // The turn is gone without settling: the child must stop, and a
        // checkpoint that could gate a resume must not outlive a tail the
        // transcript may not hold. Never waits.
        //
        // The cell is cleared as well: the recorded closure inside the handle
        // holds this cell, so leaving the handle there would keep the turn's
        // event sender alive after the turn ended and the consumer would never
        // see its channel close.
        if let Ok(mut cell) = self.io.lock() {
            if let Some(io) = cell.take() {
                if !self.settled {
                    io.cancel().signal();
                }
            }
        }
        // The facts of an exchange that never returned are unknown, and a
        // dropped turn may have written the child's prompt: any checkpoint
        // left here would gate the resume on a leaf the transcript may not
        // hold, and the binding keeps that leaf when it matches the frontier.
        // The next turn then opens a fresh child and transfers the history.
        //
        // A settled turn already decided the checkpoint in `settle`.
        if !self.settled {
            self.abandoned.store(true, Ordering::Release);
            if let Some(cursor) = &self.cursor {
                remove_checkpoint(cursor);
            }
        }
    }
}
