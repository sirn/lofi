#![allow(clippy::wildcard_imports)]

use super::messages::{
    assistant_announces_tool_intent, assistant_is_thinking_only, close_orphaned_tool_uses,
    count_image_blocks, image_payload_bytes, stamp_message_origin, strip_image_blocks,
    IMAGE_PRESSURE_SENTINEL, MAX_REQUEST_IMAGE_BYTES,
};
use super::progress::checkpoint_progress;
use super::*;

impl Agent {
    pub(super) async fn settle_turn(
        &self,
        messages: &mut [Message],
        prev_len: usize,
        mut recorder: Option<SessionRecorder>,
        stats: TurnStats,
        tx: &Sender<AgentEvent>,
        exit: Result<TurnOutcome>,
    ) -> Result<()> {
        let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
        let (outcome, err) = match exit {
            Ok(outcome) => (outcome, None),
            Err(error) => (TurnOutcome::Failed(error.to_string()), Some(error)),
        };
        // Disk first: a crash after the UI terminal event must not lose a
        // settled turn that was still only in memory.
        let mut committed = None;
        if let Some(recorder) = recorder.as_mut() {
            let summary = stats.summary(elapsed_ms);
            match recorder.flush_incremental(&messages[prev_len..], &outcome, &summary) {
                Ok(range) => committed = range,
                Err(e) if err.is_none() => return Err(e),
                Err(_) => {}
            }
        }
        if !tx.is_closed() {
            let _ = match &outcome {
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
        // A failed turn keeps the prompt and completed rounds in context:
        // the failure arm recorded the failed round as discarded and
        // truncated it already. `TurnFailed` stays a durable display
        // boundary so replay shows the failed branch while context rebuild
        // skips only the discarded round.
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
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
            origin: None,
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
    pub(super) async fn run_once_inner(
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
                                StreamingEvent::ToolUseInputComplete { id, input } => {
                                    round_bytes =
                                        round_bytes.saturating_add(input.len().saturating_sub(
                                            tool_raw.get(id).map_or(0, String::len),
                                        ));
                                    let previous = tool_decoders
                                        .get_mut(id)
                                        .map(|decoder| {
                                            decoder
                                                .update(tool_raw.get(id).map_or("", String::as_str))
                                                .to_string()
                                        })
                                        .unwrap_or_default();
                                    tool_raw.insert(id.clone(), input.clone());
                                    let mut complete_decoder = CodePrefixDecoder::default();
                                    let complete_code = complete_decoder.update(input).to_string();
                                    tool_decoders.insert(id.clone(), complete_decoder);
                                    if let Some(chunk) = complete_code.strip_prefix(&previous) {
                                        if !chunk.is_empty()
                                            && !emit(
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
                                    }
                                    tool_emitted.insert(id.clone(), complete_code.len());
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
                                StreamingEvent::ToolUseInputComplete { id, .. }
                                | StreamingEvent::ToolUseEnd { id } => id.len(),
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
            // terminal outcome decides context semantics: failures discard
            // this failed round but retain the turn's completed rounds, while
            // explicit user cancellation retains the partial assistant
            // message.
            let mut partial = assembler.finish();
            stamp_message_origin(&mut partial, &model);
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
        let mut assistant = assembler.finish();
        stamp_message_origin(&mut assistant, &model);
        messages.push(assistant);
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
            let assistant = &messages[assistant_index];
            let announced_tool_intent = assistant_announces_tool_intent(assistant);
            return Ok(RoundOutcome {
                finished: true,
                stop_reason: round_stop_reason,
                announced_tool_intent,
                thinking_only_index: assistant_is_thinking_only(assistant)
                    .then_some(assistant_index),
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
            .tool_executor()
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
            origin: None,
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
            thinking_only_index: None,
            loop_detail: round_loop_detail.or(tool_loop_detail),
            interrupted_thinking_index: None,
        })
    }
}

pub(super) struct RoundFailure {
    pub(super) error: Error,
    pub(super) provider_progress: bool,
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
pub(super) struct RoundOutcome {
    pub(super) finished: bool,
    pub(super) stop_reason: Option<lofi_types::StopReason>,
    pub(super) announced_tool_intent: bool,
    pub(super) thinking_only_index: Option<usize>,
    pub(super) loop_detail: Option<String>,
    pub(super) interrupted_thinking_index: Option<usize>,
}
