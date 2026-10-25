//! The provider transport: one exchange is one provider round.

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;
use lofi_error::Error;
use lofi_types::{ContentBlock, Message, Model, RunModel, StreamingEvent, Usage};

use super::agent_run::messages::{
    close_orphaned_tool_uses, count_image_blocks, image_payload_bytes, stamp_message_origin,
    strip_image_blocks, MAX_REQUEST_IMAGE_BYTES,
};
use super::turn_session::{
    Exchange, ExchangeDriver, ExchangeFailure, Settlement, TurnIo, TurnPolicy,
};
use super::{usage_cost, Agent, AgentEvent};

/// The provider transport's per-turn state. It must not leak across turns, so
/// the turn opens one driver per turn.
pub struct NativeEngine {
    agent: Agent,
    state: NativeTurnState,
}

impl NativeEngine {
    #[must_use]
    pub(crate) fn new(agent: Agent) -> Self {
        Self {
            agent,
            state: NativeTurnState::default(),
        }
    }
}

#[async_trait]
impl ExchangeDriver for NativeEngine {
    async fn run(
        &mut self,
        io: &TurnIo,
        history: &mut Vec<Message>,
    ) -> std::result::Result<Exchange, ExchangeFailure> {
        self.agent.run_exchange(io, history, &mut self.state).await
    }

    async fn settle(&mut self, _io: &TurnIo, _settled: Settlement) {}

    fn run_model(&self) -> RunModel {
        self.agent.run_model()
    }
}

/// The turn-cumulative spend of a provider turn.
#[derive(Default)]
pub(in crate::agent) struct Spend {
    pub(crate) cost: f64,
    pub(crate) usage: Usage,
}

impl Spend {
    pub(in crate::agent) fn add(&mut self, usage: Usage, model: &Model) {
        self.usage = usage;
        self.cost += usage_cost(model, usage).unwrap_or(0.0);
    }
}

/// Per-turn state of the provider transport.
#[derive(Default)]
pub(in crate::agent) struct NativeTurnState {
    /// The image-omit notice fires once per turn, not once per round.
    omit_notice_sent: bool,
    spend: Spend,
}

impl Agent {
    /// Drive exactly one exchange, with no turn around it.
    ///
    /// # Errors
    /// Propagates the exchange failure.
    pub async fn run_once(&self, messages: &mut Vec<Message>) -> lofi_error::Result<bool> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let admission = super::turn_session::Admission {
            frontier: None,
            upto: messages.len(),
        };
        let io = TurnIo::new(
            super::turn_session::EventSink::new(tx, None),
            None,
            admission,
            super::turn_session::ExchangeInput::continuation(),
            super::ToolCallContext::default(),
            super::turn_session::CancelToken::default(),
            None,
        );
        let mut state = NativeTurnState::default();
        let result = self.run_exchange(&io, messages, &mut state).await;
        drop(io);
        let _ = drain.await;
        result
            .map(|exchange| exchange.finished)
            .map_err(|failure| failure.error)
    }

    /// The tuneables of one turn on this transport.
    pub(crate) fn turn_policy(&self) -> TurnPolicy {
        TurnPolicy {
            auto_continue: self.auto_continue,
            retry: self.retry,
            hard_cap: self
                .model
                .context_window
                .and_then(|window| self.compaction.hard_threshold(window)),
        }
    }

    fn request_messages(&self, messages: &[Message]) -> Option<Vec<Message>> {
        if self.system_prompt.is_empty()
            || messages.iter().any(|message| {
                message.role == lofi_types::Role::System
                    && message.blocks.iter().any(
                        |block| matches!(block, ContentBlock::Text { text } if !text.is_empty()),
                    )
            })
        {
            return None;
        }
        let mut request = Vec::with_capacity(messages.len() + 1);
        request.push(Message {
            origin: None,
            role: lofi_types::Role::System,
            blocks: vec![ContentBlock::Text {
                text: self.system_prompt.clone(),
            }],
            kind: lofi_types::PromptKind::default(),
        });
        request.extend_from_slice(messages);
        Some(request)
    }

    /// One provider round: send the history, stream the answer, run the tools
    /// the message asks for, append everything to the history.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn run_exchange(
        &self,
        io: &TurnIo,
        messages: &mut Vec<Message>,
        state: &mut NativeTurnState,
    ) -> std::result::Result<Exchange, ExchangeFailure> {
        let sink = io.events();
        let cancel = io.cancel();
        let schema = super::exec_tool_schema();
        let mut model = self.model.clone();
        // Clip the output cap against remaining context so strict providers
        // (max_tokens < context_window - input_tokens) never reject the
        // request. The fill of the prior round is the best in-hand estimate of
        // this request's input.
        model.max_tokens = self.clipped_max_tokens(io.fill());
        if cancel.is_cancelled() {
            return Err(ExchangeFailure::stopped(Error::Cancelled));
        }
        // Send-time guard: a model that does not support images cannot receive
        // them, so replace each Image block with a text marker and warn the
        // consumer once per turn. Runs at send time, not attach time, so a
        // /model switch mid-session is honored. The durable transcript keeps
        // the original image; only the request payload is stripped.
        let stripped: Option<Vec<Message>> = if model.supports_image {
            None
        } else {
            let omitted = count_image_blocks(messages);
            if omitted > 0 {
                if !state.omit_notice_sent {
                    state.omit_notice_sent = true;
                    let noun = if omitted == 1 { "image" } else { "images" };
                    sink.notice(format!(
                        "{} does not support images; omitted {omitted} {noun} from this request",
                        model.id
                    ))
                    .await;
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
        // the token count, so an image-heavy history can be rejected (HTTP 413)
        // while the token-threshold compaction never trips. Stop before the
        // doomed request; the turn maps this to ContextPressure.
        let image_bytes = image_payload_bytes(send_messages);
        if image_bytes > MAX_REQUEST_IMAGE_BYTES {
            sink.notice(format!(
                "attached images total {} MiB; compacting to fit the request size limit",
                image_bytes / (1024 * 1024)
            ))
            .await;
            return Err(ExchangeFailure::stopped(Error::ContextPressure(format!(
                "request images total {image_bytes} bytes"
            ))));
        }
        let schemas = [schema];
        let requested = async { self.provider.stream(&model, send_messages, &schemas).await };
        let stream = if cancel.flag().is_some() {
            tokio::select! {
                biased;
                () = cancel.wait() => return Err(ExchangeFailure::stopped(Error::Cancelled)),
                stream = requested => stream?,
            }
        } else {
            requested.await?
        };
        let mut stream = stream;
        let mut assembler = lofi_providers::MessageAssembler::new();
        let mut round_stop_reason: Option<lofi_types::StopReason> = None;
        let mut round_bytes = 0usize;
        let mut tool_raw: HashMap<String, String> = HashMap::new();
        let mut tool_emitted: HashMap<String, usize> = HashMap::new();
        let mut tool_decoders: HashMap<String, super::CodePrefixDecoder> = HashMap::new();
        let mut provider_progress = false;
        let collect = async {
            let mut thinking_open: Option<Instant> = None;
            loop {
                let next = if cancel.flag().is_some() {
                    tokio::select! {
                        biased;
                        () = cancel.wait() => return Err(Error::Cancelled),
                        next = stream.next() => next,
                    }
                } else {
                    stream.next().await
                };
                let Some(ev) = next else { break };
                let e = ev?;
                if !matches!(e, StreamingEvent::Error(_)) {
                    provider_progress = true;
                }
                let terminal = matches!(e, StreamingEvent::Done { .. });
                let is_thinking_ev = matches!(
                    e,
                    StreamingEvent::ThinkingDelta(_) | StreamingEvent::ThinkingSignature(_)
                );
                if !is_thinking_ev {
                    if let Some(start) = thinking_open.take() {
                        let elapsed = start.elapsed();
                        if !sink.thinking_end(elapsed.as_millis() as u64).await {
                            return Err(Error::Cancelled);
                        }
                    }
                }
                match &e {
                    StreamingEvent::TextDelta(delta) => {
                        if !sink.text(delta.clone()).await {
                            return Err(Error::Cancelled);
                        }
                    }
                    StreamingEvent::ThinkingDelta(delta) => {
                        if thinking_open.is_none() {
                            thinking_open = Some(Instant::now());
                        }
                        if !sink.thinking(delta.clone()).await {
                            // The sink published this delta before the turn
                            // stopped the round on it, so the partial must
                            // carry it: the transcript rebuilds a round from
                            // the messages the exchange appends, not from the
                            // stream it read.
                            assembler.push(StreamingEvent::ThinkingDelta(delta.clone()));
                            return Err(Error::Cancelled);
                        }
                    }
                    StreamingEvent::ToolUseStart { id, name } => {
                        if !sink.tool_start(id.clone(), name.clone()).await {
                            return Err(Error::Cancelled);
                        }
                    }
                    StreamingEvent::ToolUseInputDelta { id, delta } => {
                        let raw = tool_raw.entry(id.clone()).or_default();
                        raw.push_str(delta);
                        let decoded = tool_decoders.entry(id.clone()).or_default().update(raw);
                        let prev = tool_emitted.get(id).copied().unwrap_or(0);
                        if let Some(chunk) = decoded.get(prev..) {
                            if !chunk.is_empty() {
                                if !sink.tool_input_delta(id.clone(), chunk.to_string()).await {
                                    return Err(Error::Cancelled);
                                }
                                tool_emitted.insert(id.clone(), decoded.len());
                            }
                        }
                    }
                    StreamingEvent::ToolUseInputComplete { id, input } => {
                        round_bytes = round_bytes.saturating_add(
                            input
                                .len()
                                .saturating_sub(tool_raw.get(id).map_or(0, String::len)),
                        );
                        let previous = tool_decoders
                            .get_mut(id)
                            .map(|decoder| {
                                decoder
                                    .update(tool_raw.get(id).map_or("", String::as_str))
                                    .to_string()
                            })
                            .unwrap_or_default();
                        tool_raw.insert(id.clone(), input.clone());
                        let mut complete_decoder = super::CodePrefixDecoder::default();
                        let complete_code = complete_decoder.update(input).to_string();
                        tool_decoders.insert(id.clone(), complete_decoder);
                        if let Some(chunk) = complete_code.strip_prefix(&previous) {
                            if !chunk.is_empty()
                                && !sink.tool_input_delta(id.clone(), chunk.to_string()).await
                            {
                                return Err(Error::Cancelled);
                            }
                        }
                        tool_emitted.insert(id.clone(), complete_code.len());
                    }
                    StreamingEvent::Done { usage, stop_reason } => {
                        round_stop_reason = *stop_reason;
                        state.spend.add(*usage, &self.model);
                        if !sink.round_usage(state.spend.cost, state.spend.usage).await {
                            return Err(Error::Cancelled);
                        }
                        // The same report a child transport sends, so the gauge
                        // reads one field whichever engine ran the step.
                        let context = lofi_types::ContextUsage {
                            used: state.spend.usage.context_tokens(),
                            size: self.model.context_window.filter(|&size| size > 0),
                        };
                        if !sink.context_reported(Some(context)).await {
                            return Err(Error::Cancelled);
                        }
                    }
                    StreamingEvent::Error(message) => {
                        return Err(Error::Provider(message.clone()));
                    }
                    _ => {}
                }
                // Bound retained round bytes so a stream of many small valid
                // events cannot exhaust memory.
                round_bytes = round_bytes.saturating_add(super::PER_EVENT_OVERHEAD);
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
                    StreamingEvent::ToolUseInputDelta { id, delta } => id.len() + delta.len(),
                    StreamingEvent::ToolUseInputComplete { id, .. }
                    | StreamingEvent::ToolUseEnd { id } => id.len(),
                    StreamingEvent::Done { .. } => 0,
                });
                if round_bytes > super::MAX_ROUND_BYTES {
                    return Err(Error::Provider(format!(
                        "round exceeded {} byte budget",
                        super::MAX_ROUND_BYTES
                    )));
                }
                assembler.push(e);
                if terminal {
                    break;
                }
            }
            if let Some(start) = thinking_open.take() {
                let elapsed = start.elapsed();
                if !sink.thinking_end(elapsed.as_millis() as u64).await {
                    return Err(Error::Cancelled);
                }
            }
            Ok(())
        };
        if let Err(error) = collect.await {
            // The live UI has already rendered every accepted delta. Preserve
            // that same partial assistant message so durable replay cannot make
            // visible output disappear; the rollback decides whether it stays.
            let mut partial = assembler.finish();
            stamp_message_origin(&mut partial, &model);
            if !partial.blocks.is_empty() {
                messages.push(partial);
            }
            if matches!(error, Error::Cancelled) {
                // The partial stays in the history, so its unanswered tool
                // calls must be closed. A retryable failure is discarded whole
                // by the turn: closing here would add a tool message that no
                // discard boundary can drop on replay.
                close_orphaned_tool_uses(messages);
                return Err(ExchangeFailure::stopped(error));
            }
            let failure = ExchangeFailure::failed(error);
            return Err(if provider_progress {
                failure.with_progress()
            } else {
                failure
            });
        }
        // A tool use whose arguments never parse was cut off mid-stream.
        // Compute before finish consumes the assembler; the set is only
        // consulted on a token-cap stop.
        let cut_tool_ids: std::collections::HashSet<String> =
            if round_stop_reason == Some(lofi_types::StopReason::MaxTokens) {
                assembler.unparseable_tool_ids().into_iter().collect()
            } else {
                std::collections::HashSet::new()
            };
        let assistant_index = messages.len();
        let mut assistant = assembler.finish();
        stamp_message_origin(&mut assistant, &model);
        messages.push(assistant);
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
            return Ok(Exchange::done(round_stop_reason));
        }
        // The prompt is durable before the first tool runs.
        io.commit(messages).await;
        let context = io.tool_context();
        let run_refs: Vec<(&str, &str, &serde_json::Value)> = tool_uses
            .iter()
            .filter(|(id, _, _)| !cut_tool_ids.contains(id))
            .map(|(id, name, input)| (id.as_str(), name.as_str(), input))
            .collect();
        let executed = match self
            .tool_executor()
            .execute_tools(&run_refs, &context)
            .await
        {
            Ok(results) => results,
            Err(error) => {
                // Abandoning the turn here (cancel, closed channel, tool error)
                // leaves the pushed assistant message's ToolUse blocks without
                // matching results. Close the orphans so a retained cancel
                // partial stays provider-valid.
                close_orphaned_tool_uses(messages);
                return Err(ExchangeFailure::stopped(error));
            }
        };
        let mut executed_iter = executed.into_iter();
        let mut results = Vec::with_capacity(tool_uses.len());
        for (id, _, _) in &tool_uses {
            if cut_tool_ids.contains(id) {
                let cut_notice =
                    "cut off: tool arguments were truncated by the provider token limit; \
                     resend the tool call"
                        .to_string();
                if !sink
                    .emit(AgentEvent::ToolEnd {
                        id: id.clone(),
                        result: cut_notice.clone(),
                        is_error: true,
                        elapsed_ms: 0,
                    })
                    .await
                {
                    close_orphaned_tool_uses(messages);
                    return Err(ExchangeFailure::stopped(Error::Cancelled));
                }
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: cut_notice,
                    is_error: true,
                    images: Vec::new(),
                });
            } else {
                // execute_tools returns exactly one result per input; a short
                // iterator means the tool loop already failed.
                let Some(result) = executed_iter.next() else {
                    close_orphaned_tool_uses(messages);
                    return Err(ExchangeFailure::stopped(Error::Provider(
                        "tool result count mismatch".into(),
                    )));
                };
                results.push(result);
            }
        }
        messages.push(Message {
            origin: None,
            role: lofi_types::Role::Tool,
            blocks: results,
            kind: lofi_types::PromptKind::User,
        });
        Ok(Exchange::continued(round_stop_reason))
    }
}
