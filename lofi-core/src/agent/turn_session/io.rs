//! The capabilities a transport may use inside one turn.
//!
//! A transport never sees the turn. It gets this handle: the event channel, the
//! transcript write path, the frame it must send, the tool context, and the
//! cancel token. Anything a turn decides alone -- the outcome, the terminal
//! marker, the retry ladder, the discard -- is not reachable from here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lofi_error::{Error, Result};
use lofi_types::{ContentBlock, ContextUsage, Message, PromptKind, StopReason, Usage};
use tokio::sync::mpsc::Sender;

use super::super::AgentEvent;
use crate::agent::ToolCallContext;
use crate::session::recorder::{SessionRecord, SessionRecorder, TurnSummary};
use crate::session::store::SessionCursor;

/// The turn's output channel, and the numbers the turn folds from it.
///
/// Every event a turn publishes passes through here, so usage, cost, stop
/// reason and tool/thinking timings have one ingress: the turn never asks a
/// transport for them and no transport has to remember to report them.
#[derive(Clone)]
pub struct EventSink {
    tx: Sender<AgentEvent>,
    accounting: Arc<Mutex<Accounting>>,
    /// The transcript the two session-level records go to. None on a channel
    /// that only carries events, such as a transport's internal one.
    cursor: Option<SessionCursor>,
    /// The lifetime of the turn this sink belongs to. None on a channel that
    /// only carries events.
    alive: Option<Arc<AtomicBool>>,
}

impl EventSink {
    pub(crate) fn new(tx: Sender<AgentEvent>, cursor: Option<SessionCursor>) -> Self {
        Self {
            tx,
            accounting: Arc::new(Mutex::new(Accounting::default())),
            cursor,
            alive: None,
        }
    }

    /// Publish one event. False when the consumer is gone.
    ///
    /// The turn and the core helpers it calls use this. A transport uses the
    /// named helpers below, which cannot express a terminal marker.
    pub(crate) async fn emit(&self, event: AgentEvent) -> bool {
        // The turn owns the terminal marker: a transport that reaches this
        // channel must not end a turn it does not know the outcome of.
        debug_assert!(
            !matches!(
                event,
                AgentEvent::TurnCommitted { .. }
                    | AgentEvent::TurnEnd { .. }
                    | AgentEvent::TurnFailed { .. }
                    | AgentEvent::TurnCancelled { .. }
            ),
            "only the turn writes a terminal marker"
        );
        self.fold(&event);
        self.tx.send(event).await.is_ok()
    }

    fn fold(&self, event: &AgentEvent) {
        {
            let mut accounting = self.accounting();
            match event {
                AgentEvent::RoundUsage { cost, usage } => {
                    accounting.cost = *cost;
                    accounting.usage = *usage;
                }
                AgentEvent::ToolEnd { id, elapsed_ms, .. } => {
                    accounting.tool_elapsed.push((id.clone(), *elapsed_ms));
                }
                AgentEvent::ThinkingEnd { elapsed_ms } => {
                    accounting.thinking_elapsed.push(*elapsed_ms);
                }
                _ => {}
            }
        }
        // Two records describe the session, not the turn: they travel as
        // events and land here, so a transport never writes a record.
        // A turn that is gone writes nothing, even when a worker still holds a
        // sink and reports an event after it.
        if !self
            .alive
            .as_ref()
            .is_none_or(|alive| alive.load(Ordering::Acquire))
        {
            return;
        }
        let Some(cursor) = &self.cursor else {
            return;
        };
        let record = match event {
            AgentEvent::ContextReported { context } => {
                Some(SessionRecord::ContextReported { context: *context })
            }
            AgentEvent::SubagentUsage { usage, cost } => Some(SessionRecord::SubagentUsage {
                usage: *usage,
                cost: *cost,
            }),
            _ => None,
        };
        if let Some(record) = record {
            if let Err(error) = cursor.record(record) {
                tracing::warn!(error = %error, "session record failed");
            }
        }
    }

    pub(crate) async fn text(&self, delta: String) -> bool {
        self.emit(AgentEvent::Text(delta)).await
    }

    pub(crate) async fn thinking(&self, delta: String) -> bool {
        self.emit(AgentEvent::Thinking(delta)).await
    }

    pub(crate) async fn thinking_end(&self, elapsed_ms: u64) -> bool {
        self.emit(AgentEvent::ThinkingEnd { elapsed_ms }).await
    }

    pub(crate) async fn tool_start(&self, id: String, name: String) -> bool {
        self.emit(AgentEvent::ToolStart { id, name }).await
    }

    pub(crate) async fn tool_input_delta(&self, id: String, delta: String) -> bool {
        self.emit(AgentEvent::ToolInputDelta { id, delta }).await
    }

    pub(crate) async fn round_usage(&self, cost: f64, usage: Usage) -> bool {
        self.emit(AgentEvent::RoundUsage { cost, usage }).await
    }

    pub(crate) async fn context_reported(&self, context: Option<ContextUsage>) -> bool {
        self.emit(AgentEvent::ContextReported { context }).await
    }

    pub(crate) async fn notice(&self, text: String) -> bool {
        self.emit(AgentEvent::Notice(text)).await
    }

    /// Publish one event, reporting a closed consumer as a cancellation.
    pub(crate) async fn forward(&self, event: AgentEvent) -> Result<()> {
        if self.emit(event).await {
            Ok(())
        } else {
            Err(Error::Cancelled)
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub(crate) async fn closed(&self) {
        self.tx.closed().await;
    }

    /// The raw channel, for core helpers that publish several event kinds and
    /// cannot know which ones a transport should reach.
    pub(in crate::agent) fn sender(&self) -> &Sender<AgentEvent> {
        &self.tx
    }

    pub(crate) fn accounting(&self) -> MutexGuard<'_, Accounting> {
        self.accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The turn's accumulators, folded from the events it publishes.
#[derive(Default)]
pub(crate) struct Accounting {
    cost: f64,
    usage: Usage,
    stop_reason: Option<StopReason>,
    tool_elapsed: Vec<(String, u64)>,
    thinking_elapsed: Vec<u64>,
}

impl Accounting {
    /// The fill of the latest request, when one reported usage.
    pub(crate) fn fill(&self) -> Option<u64> {
        (self.usage != Usage::default()).then(|| self.usage.context_tokens())
    }

    pub(crate) fn cost(&self) -> f64 {
        self.cost
    }

    pub(crate) fn usage(&self) -> Usage {
        self.usage
    }

    pub(crate) fn set_stop_reason(&mut self, stop_reason: Option<StopReason>) {
        self.stop_reason = stop_reason;
    }

    pub(crate) fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason
    }

    /// The snapshot the session recorder stamps on a turn marker.
    pub(crate) fn summary(&self, elapsed_ms: u64) -> TurnSummary {
        let mut tool_elapsed = self.tool_elapsed.clone();
        tool_elapsed.sort_by(|a, b| a.0.cmp(&b.0));
        TurnSummary {
            elapsed_ms,
            cost: self.cost,
            usage: self.usage,
            tool_elapsed,
            thinking_elapsed: self.thinking_elapsed.clone(),
            stop_reason: self.stop_reason,
        }
    }
}

/// The span covering both ranges, when there is one.
pub(super) fn merge_range(
    first: Option<(u64, u64)>,
    second: Option<(u64, u64)>,
) -> Option<(u64, u64)> {
    match (first, second) {
        (Some((start, _)), Some((_, end))) => Some((start, end)),
        (range @ Some(_), None) | (None, range @ Some(_)) => range,
        (None, None) => None,
    }
}

/// The cancellation flag of one turn.
///
/// Monotonic: once set it stays set, so a transport that signals it can only
/// do what abandoning its own exchange would do anyway.
#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Option<Arc<AtomicBool>>,
}

impl CancelToken {
    /// A caller that passes its own flag reads it after the turn; a caller
    /// that passes none still gets one, so dropping the turn future can stop
    /// its child.
    pub(crate) fn new(flag: Option<Arc<AtomicBool>>) -> Self {
        Self {
            flag: Some(flag.unwrap_or_default()),
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    pub(crate) fn signal(&self) {
        if let Some(flag) = &self.flag {
            flag.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn flag(&self) -> Option<&Arc<AtomicBool>> {
        self.flag.as_ref()
    }

    /// Resolve once the turn is cancelled. Polls the flag the caller sets, so
    /// a UI interrupt and a dropped caller reach it the same way.
    pub(crate) async fn wait(&self) {
        while !self.is_cancelled() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Ends a turn's recording ability when the turn drops.
///
/// The turn keeps this guard; transports keep `TurnIo` clones. Dropping the
/// guard is the one way the turn's lifetime reaches a worker thread.
pub struct TurnReaper {
    alive: Arc<AtomicBool>,
}

impl Drop for TurnReaper {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

/// The turn's frontier in the session lineage, taken before it admitted
/// anything.
#[derive(Clone, Default)]
pub struct Admission {
    /// The leaf the session recorded before this turn recorded its prompt.
    pub frontier: Option<String>,
    /// The history length at that moment; everything after it belongs to this
    /// turn.
    pub upto: usize,
}

/// The frame the next exchange sends.
#[derive(Clone)]
pub struct ExchangeInput {
    /// The prompt text, or None for a silent continuation.
    pub text: Option<String>,
    pub kind: PromptKind,
    pub attachments: Vec<ContentBlock>,
    /// Prompts that precede the input in the same turn. A transport that
    /// transfers history renders them with the input, as one frame.
    pub notices: Vec<String>,
}

impl ExchangeInput {
    pub(crate) fn continuation() -> Self {
        Self {
            text: None,
            kind: PromptKind::Notice,
            attachments: Vec::new(),
            notices: Vec::new(),
        }
    }

    pub fn is_continuation(&self) -> bool {
        self.text.is_none()
    }
}

pub struct TurnIoInner {
    /// False once the turn dropped. A transport keeps a clone of `TurnIo` on a
    /// thread of its own, so the recorder needs the turn's own lifetime to stop
    /// writing when the turn goes away.
    alive: Arc<AtomicBool>,
    sink: EventSink,
    recorder: Mutex<Option<SessionRecorder>>,
    /// Bytes a transport recorded outside the commit path. A child folds its
    /// own messages, so the turn publishes them with its next commit; a range
    /// nobody published is content the frozen transcript cannot rebuild.
    unpublished: Mutex<Option<(u64, u64)>>,
    admission: Admission,
    input: Mutex<ExchangeInput>,
    context: ToolCallContext,
    cancel: CancelToken,
    turn_start: Instant,
    /// The session this turn writes to, when it has one. The transport needs
    /// it for its own resume bookkeeping.
    cursor: Option<SessionCursor>,
}

/// Everything a transport may touch inside one turn.
#[derive(Clone)]
pub struct TurnIo {
    inner: Arc<TurnIoInner>,
}

impl TurnIo {
    pub(crate) fn new(
        sink: EventSink,
        recorder: Option<SessionRecorder>,
        admission: Admission,
        input: ExchangeInput,
        context: ToolCallContext,
        cancel: CancelToken,
        cursor: Option<SessionCursor>,
    ) -> Self {
        let alive = Arc::new(AtomicBool::new(true));
        // The tool context and the sink travel with a worker thread that can
        // outlive the turn, so both carry the same lifetime as the recorder.
        let mut sink = sink;
        sink.alive = Some(alive.clone());
        let mut context = context;
        context.alive = Some(alive.clone());
        Self {
            inner: Arc::new(TurnIoInner {
                alive,
                sink,
                recorder: Mutex::new(recorder),
                unpublished: Mutex::new(None),
                admission,
                input: Mutex::new(input),
                context,
                cancel,
                turn_start: Instant::now(),
                cursor,
            }),
        }
    }

    /// The guard that ends this turn's ability to record. The turn owns it and
    /// drops it with the turn, so a transport that outlives the turn cannot
    /// write to the transcript afterwards.
    pub(crate) fn reaper(&self) -> TurnReaper {
        TurnReaper {
            alive: self.inner.alive.clone(),
        }
    }

    fn alive(&self) -> bool {
        self.inner.alive.load(Ordering::Acquire)
    }

    /// End this turn's ability to record, while the turn still runs.
    ///
    /// The reaper ends it when the turn returns, but a transport's worker can
    /// settle a tool after the terminal flush and before that: its result
    /// would land past the marker that explains the turn, and a resumed
    /// session would rebuild a context the live one never had.
    pub(crate) fn seal(&self) {
        self.inner.alive.store(false, Ordering::Release);
    }

    /// The session this turn writes to.
    pub fn cursor(&self) -> Option<&SessionCursor> {
        self.inner.cursor.as_ref()
    }

    pub fn events(&self) -> &EventSink {
        &self.inner.sink
    }

    /// Write the turn's history so far to the transcript and publish the byte
    /// range.
    ///
    /// The turn slices at its admission frontier, so a transport cannot pick a
    /// window that disagrees with the disk. A failed write reports its error to
    /// the caller and writes nothing: the recorder keeps its counts, so the next
    /// checkpoint retries the same suffix.
    pub fn record(&self, history: &[Message]) -> Result<Option<(u64, u64)>> {
        // The turn is gone: a detached transport still holds this capability
        // but may not extend a transcript no turn will settle.
        if !self.alive() {
            return Ok(None);
        }
        let summary = self.inner.sink.accounting().summary(self.elapsed_ms());
        let range = {
            let mut guard = self.recorder();
            let Some(recorder) = guard.as_mut() else {
                return Ok(None);
            };
            let suffix = history.get(self.frontier().upto..).unwrap_or_default();
            recorder.checkpoint(suffix, &summary)?
        };
        self.unpublish(range);
        Ok(range)
    }

    /// Hold a written range for the next publish.
    pub(crate) fn unpublish(&self, range: Option<(u64, u64)>) {
        let Some(range) = range else {
            return;
        };
        let mut unpublished = self
            .inner
            .unpublished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *unpublished = merge_range(*unpublished, Some(range));
    }

    /// Take the ranges recorded since the last publish, when any.
    pub(crate) fn take_unpublished(&self) -> Option<(u64, u64)> {
        self.inner
            .unpublished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Write the history and publish the byte range. A write failure is a
    /// notice, not a failure: the recorder keeps its counts, so the next
    /// checkpoint retries the same suffix.
    pub async fn commit(&self, history: &[Message]) -> Option<(u64, u64)> {
        let range = match self.record(history) {
            Ok(range) => range,
            Err(error) => {
                self.inner
                    .sink
                    .notice(format!(
                        "transcript write failed (will retry on the next round): {error}"
                    ))
                    .await;
                return None;
            }
        };
        self.publish(range).await;
        range
    }

    /// Write the history and publish the byte range, failing when the write
    /// fails.
    ///
    /// A boundary the turn depends on is not a best-effort checkpoint: the
    /// prompt must be durable before the first tool runs, and a round must be
    /// durable before the turn removes it from the model history.
    pub async fn commit_durable(&self, history: &[Message]) -> Result<Option<(u64, u64)>> {
        let range = self.record(history)?;
        self.publish(range).await;
        Ok(range)
    }

    /// Publish what was recorded since the last publish.
    ///
    /// A prompt's bytes belong to the row its event opens, so the turn records
    /// them first and publishes after the event.
    pub(crate) async fn publish_pending(&self) {
        self.publish(None).await;
    }

    async fn publish(&self, range: Option<(u64, u64)>) {
        // Everything recorded since the last publish travels with this one, so
        // a transport that folded its own messages still hands the live
        // transcript a range that covers them.
        let Some((byte_start, byte_end)) =
            merge_range(self.take_unpublished(), range).filter(|(start, end)| end > start)
        else {
            return;
        };
        self.inner
            .sink
            .emit(AgentEvent::RoundCommitted {
                byte_start,
                byte_end,
            })
            .await;
    }

    /// The fill of the latest request, when one reported usage.
    pub fn fill(&self) -> Option<u64> {
        self.inner.sink.accounting().fill()
    }

    pub fn frontier(&self) -> &Admission {
        &self.inner.admission
    }

    pub fn input(&self) -> ExchangeInput {
        self.inner
            .input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set_input(&self, input: ExchangeInput) {
        *self
            .inner
            .input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = input;
    }

    pub fn tool_context(&self) -> ToolCallContext {
        self.inner.context.clone()
    }

    pub fn cancel(&self) -> &CancelToken {
        &self.inner.cancel
    }

    /// Stamp the identity the transport reported on this turn's marker.
    pub(crate) fn set_model(&self, model: lofi_types::RunModel) {
        if let Some(recorder) = self.recorder().as_mut() {
            recorder.set_model(model);
        }
    }

    pub(crate) fn recorder(&self) -> MutexGuard<'_, Option<SessionRecorder>> {
        self.inner
            .recorder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn elapsed_ms(&self) -> u64 {
        self.inner.turn_start.elapsed().as_millis() as u64
    }
}
