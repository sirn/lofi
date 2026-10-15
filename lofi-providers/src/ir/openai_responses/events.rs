use std::collections::{HashMap, HashSet};

use lofi_types::{StreamingEvent, Usage};
use serde_json::Value;

use lofi_error::{Error, Result};

#[derive(Default, Debug, Clone)]
pub(crate) struct ResponsesMapperState {
    item_to_call: HashMap<String, String>,
    completed_tool_arguments: HashSet<String>,
    started_tool_calls: HashSet<String>,
    reasoning_items_with_deltas: HashSet<String>,
    pub(crate) saw_completed: bool,
}

// A completed response that still ended early carries the reason in
// `incomplete_details` (e.g. max_output_tokens); a plain completed status
// is an end of turn.
fn map_response_completed(v: &Value) -> StreamingEvent {
    let response = v.get("response").unwrap_or(v);
    let usage = response.get("usage").or_else(|| v.get("usage"));
    let stop_reason = match response
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
    {
        Some("max_output_tokens" | "max_tool_calls") => Some(lofi_types::StopReason::MaxTokens),
        Some(_) => Some(lofi_types::StopReason::Other),
        None => Some(lofi_types::StopReason::EndTurn),
    };
    StreamingEvent::Done {
        usage: usage.map(usage_from_openai_responses).unwrap_or_default(),
        stop_reason,
    }
}

/// # Errors
/// Returns [`Error::Provider`] for `response.failed` or `error` events so a
/// provider-reported failure fails the round trip.
pub(super) fn map_openai_responses_event(
    v: &Value,
    state: &mut ResponsesMapperState,
) -> Result<Vec<StreamingEvent>> {
    let mut out = Vec::new();
    let Some(ty) = v.get("type").and_then(Value::as_str) else {
        return Ok(out);
    };
    match ty {
        "response.output_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    out.push(StreamingEvent::TextDelta(delta.to_string()));
                }
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    if let Some(item_id) = v.get("item_id").and_then(Value::as_str) {
                        state
                            .reasoning_items_with_deltas
                            .insert(item_id.to_string());
                    }
                    out.push(StreamingEvent::ThinkingDelta(delta.to_string()));
                }
            }
        }
        "response.reasoning_summary_part.done" => {
            let item_id = v.get("item_id").and_then(Value::as_str).unwrap_or("");
            if state.reasoning_items_with_deltas.contains(item_id) {
                out.push(StreamingEvent::ThinkingDelta("\n\n".to_string()));
            }
        }
        "response.output_item.added" => {
            if let Some(item) = v.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    start_function_call(item, state, &mut out);
                }
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = v
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    let id = state.item_to_call.get(&item_id).cloned().unwrap_or(item_id);
                    if !state.completed_tool_arguments.contains(&id) {
                        out.push(StreamingEvent::ToolUseInputDelta {
                            id,
                            delta: delta.to_string(),
                        });
                    }
                }
            }
        }
        "response.function_call_arguments.done" => {
            let item_id = v.get("item_id").and_then(Value::as_str).unwrap_or("");
            if let Some(call_id) = state.item_to_call.get(item_id).cloned() {
                emit_complete_arguments_once(v, &call_id, state, &mut out);
            }
        }
        "response.output_item.done" => {
            if let Some(item) = v.get("item") {
                out.extend(map_completed_output_item(item, state));
            }
        }
        // `response.incomplete` is the canonical end-of-stream event for a
        // truncated turn (e.g. max_output_tokens), with the same payload shape
        // as `response.completed`; both map through map_response_completed and
        // continue the turn rather than hard-failing on a state the run loop
        // knows how to recover from.
        "response.completed" | "response.incomplete" => {
            state.saw_completed = true;
            out.push(map_response_completed(v));
        }
        "response.failed" | "error" => {
            return Err(Error::Provider(format!("provider stream error: {v}")));
        }
        _ => {}
    }
    Ok(out)
}

fn map_completed_output_item(
    item: &Value,
    state: &mut ResponsesMapperState,
) -> Vec<StreamingEvent> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let mut events = Vec::new();
            if let Some(call_id) = start_function_call(item, state, &mut events) {
                emit_complete_arguments_once(item, &call_id, state, &mut events);
                events.push(StreamingEvent::ToolUseEnd { id: call_id });
            }
            events
        }
        Some("reasoning") => {
            // Some Responses-compatible providers omit summary deltas but
            // include the completed item. Avoid duplicating streamed text.
            // Always take `encrypted_content` from the completed item; it is
            // not streamed as a delta.
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
            let mut events = Vec::new();
            if !state.reasoning_items_with_deltas.contains(item_id) {
                if let Some(text) = reasoning_item_text(item) {
                    events.push(StreamingEvent::ThinkingDelta(text));
                }
            }
            if let Some(blob) = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                events.push(StreamingEvent::ThinkingSignature(blob.to_string()));
            }
            events
        }
        _ => Vec::new(),
    }
}

fn start_function_call(
    item: &Value,
    state: &mut ResponsesMapperState,
    out: &mut Vec<StreamingEvent>,
) -> Option<String> {
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();
    if let Some(item_id) = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        state
            .item_to_call
            .insert(item_id.to_string(), call_id.clone());
    }
    if let Some(name) = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    {
        if state.started_tool_calls.insert(call_id.clone()) {
            out.push(StreamingEvent::ToolUseStart {
                id: call_id.clone(),
                name: name.to_string(),
            });
        }
    }
    Some(call_id)
}

fn emit_complete_arguments_once(
    item: &Value,
    call_id: &str,
    state: &mut ResponsesMapperState,
    out: &mut Vec<StreamingEvent>,
) {
    if state.completed_tool_arguments.contains(call_id) {
        return;
    }
    let Some(arguments) = item
        .get("arguments")
        .and_then(Value::as_str)
        .filter(|arguments| !arguments.is_empty())
    else {
        return;
    };
    state.completed_tool_arguments.insert(call_id.to_string());
    out.push(StreamingEvent::ToolUseInputComplete {
        id: call_id.to_string(),
        input: arguments.to_string(),
    });
}

/// Extract raw text from a completed Responses reasoning item.
/// Prefer the requested summary, falling back to exposed reasoning content
/// for compatible providers that return that shape instead. Display cleanup
/// belongs to the UI; placeholders and whitespace are retained here so the
/// transcript preserves the provider's data.
fn reasoning_item_text(item: &Value) -> Option<String> {
    ["summary", "content"].into_iter().find_map(|field| {
        let text = item
            .get(field)?
            .as_array()?
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n");
        (!text.is_empty()).then_some(text)
    })
}

fn usage_from_openai_responses(v: &Value) -> Usage {
    let prompt = v
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cached = v
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: v
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: cached,
        cache_write_tokens: 0,
    }
}
