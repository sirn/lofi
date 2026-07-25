//! `OpenAI` Responses request-body builder + SSE-event mapper.
//!
//! The Responses API streams typed events keyed by a `type` field inside the
//! JSON `data:` payload (e.g. `response.output_text.delta`,
//! `response.function_call_arguments.delta`, `response.completed`). This module
//! maps those to [`StreamingEvent`]s.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::{HashMap, HashSet};

use lofi_types::{Message, Model, StreamingEvent, ThinkingLevel, Usage};
use serde_json::{json, Value};

use super::block::to_openai_responses_input;
use super::chat::ToolSchema;
use lofi_error::{Error, Result};

/// Build the `POST /responses` body for a streaming turn.
#[must_use]
pub fn build_openai_responses_request(
    model: &Model,
    messages: &[Message],
    tools: &[ToolSchema],
) -> Value {
    let input = to_openai_responses_input(messages);
    let mut req = json!({
        "model": model.id,
        "input": input,
        "stream": true,
    });
    // GPT-5.6 and later use `prompt_cache_key` to route requests sharing an
    // exact prefix to the same cache shard. Without it, an append-only agent
    // history can still bounce between shards and intermittently report a
    // complete cache miss. Derive a stable, non-identifying key from the
    // model, the oldest input item, and the first user item. It stays fixed
    // while a conversation is appended, partitions unrelated sessions, and
    // naturally changes when compaction rebuilds the prefix.
    req["prompt_cache_key"] = json!(prompt_cache_key(model, &input));
    if let Some(mt) = model.max_tokens {
        req["max_output_tokens"] = json!(mt);
    }
    if !tools.is_empty() {
        let tools_arr: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                })
            })
            .collect();
        req["tools"] = json!(tools_arr);
    }
    if let Some(effort) = openai_effort(model.thinking) {
        // Request a displayable reasoning summary whenever thinking is on.
        // OpenAI does not expose raw chain-of-thought; `summary: auto`
        // enables the summary delta events handled by the stream mapper.
        req["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }
    req
}

/// Stable cache-routing key for one append-only conversation prefix.
///
/// The rendered key contains no prompt text. The first input item captures
/// the stable system/compaction prefix; the first user item partitions
/// conversations that share the same system prompt. Both remain fixed across
/// ordinary append-only turns. FNV-1a keeps the key stable across process
/// restarts without adding a hashing dependency.
fn prompt_cache_key(model: &Model, input: &[Value]) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    let mut update = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    update(model.id.as_bytes());
    if let Some(first) = input.first() {
        update(first.to_string().as_bytes());
    }
    if let Some(first_user) = input
        .iter()
        .find(|item| item.get("role").and_then(Value::as_str) == Some("user"))
    {
        update(first_user.to_string().as_bytes());
    }
    format!("lofi:{hash:016x}")
}

/// Map a thinking level to an `OpenAI` `reasoning.effort` value. `Off` returns
/// `None` (field omitted); `XHigh` clamps to `high`.
fn openai_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::XHigh => Some("high"),
    }
}

/// State accumulated across Responses events for tool-call correlation.
///
/// Tool *start*/*end* events carry `call_id`, but argument *delta* events
/// carry the distinct output-item `item_id`. We record `item.id -> call_id`
/// at `response.output_item.added` and translate subsequent `item_id` deltas
/// through that map, so interleaved function calls don't have their arguments
/// appended to the wrong tool.
#[derive(Default, Debug, Clone)]
pub struct ResponsesMapperState {
    item_to_call: HashMap<String, String>,
    reasoning_items_with_deltas: HashSet<String>,
    pub(crate) saw_completed: bool,
}

/// Map a single Responses stream event payload to zero or more
/// [`StreamingEvent`]s.
///
/// Recognized types: `response.output_item.added` (tool-use start),
/// `response.function_call_arguments.delta` (tool input), `response.output_item.done`
/// (tool-use end), `response.output_text.delta` (text), and `response.completed`
/// (terminal usage). `response.failed`/`response.incomplete`/`error` become
/// errors. Other event types are ignored.
///
/// # Errors
///
/// Returns [`Error::Provider`] for `response.failed`, `response.incomplete`,
/// or `error` events so a provider-reported failure fails the round trip.
pub fn map_openai_responses_event(
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
        // Reasoning summaries (o-series / GPT-5 with `reasoning.summary` set)
        // arrive as their own delta stream; route them to a thinking block.
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
            // Preserve summary-part boundaries. Besides rendering paragraphs
            // correctly, this lets the UI discard standalone empty placeholder
            // parts without hiding literal comments embedded in real content.
            let item_id = v.get("item_id").and_then(Value::as_str).unwrap_or("");
            if state.reasoning_items_with_deltas.contains(item_id) {
                out.push(StreamingEvent::ThinkingDelta("\n\n".to_string()));
            }
        }
        "response.output_item.added" => {
            if let Some(item) = v.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let item_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !item_id.is_empty() && !call_id.is_empty() {
                        state.item_to_call.insert(item_id, call_id.clone());
                    }
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !name.is_empty() && !call_id.is_empty() {
                        out.push(StreamingEvent::ToolUseStart { id: call_id, name });
                    }
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
                    // Translate the output-item id to the call id so
                    // `assemble_message` correlates args with the right call.
                    let id = state.item_to_call.get(&item_id).cloned().unwrap_or(item_id);
                    out.push(StreamingEvent::ToolUseInputDelta {
                        id,
                        delta: delta.to_string(),
                    });
                }
            }
        }
        "response.output_item.done" => {
            if let Some(event) = v
                .get("item")
                .and_then(|item| map_completed_output_item(item, state))
            {
                out.push(event);
            }
        }
        "response.completed" => {
            state.saw_completed = true;
            let usage = v
                .get("response")
                .and_then(|r| r.get("usage"))
                .or_else(|| v.get("usage"));
            out.push(StreamingEvent::Done(
                usage.map(usage_from_openai_responses).unwrap_or_default(),
            ));
        }
        "response.failed" | "response.incomplete" | "error" => {
            return Err(Error::Provider(format!("provider stream error: {v}")));
        }
        _ => {}
    }
    Ok(out)
}

/// Map the event produced when an output item completes.
fn map_completed_output_item(item: &Value, state: &ResponsesMapperState) -> Option<StreamingEvent> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => item
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(|id| StreamingEvent::ToolUseEnd { id: id.to_string() }),
        Some("reasoning") => {
            // Some Responses-compatible providers omit summary deltas but
            // include the completed item. Avoid duplicating streamed text.
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
            (!state.reasoning_items_with_deltas.contains(item_id))
                .then(|| reasoning_item_text(item))
                .flatten()
                .map(StreamingEvent::ThinkingDelta)
        }
        _ => None,
    }
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

/// Extract [`Usage`] from a Responses `usage` object.
fn usage_from_openai_responses(v: &Value) -> Usage {
    // Responses reports `input_tokens` as the full prompt (cached + non-cached)
    // and the cached slice in `input_tokens_details`. Store the non-cached
    // portion in `input_tokens` so `input + cache_read` is the prompt size.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> Model {
        Model {
            id: "gpt-4o".to_string(),
            name: "gpt".to_string(),
            provider: "p".to_string(),
            api: lofi_types::Api::OpenAiResponses,
            reasoning: false,
            thinking: lofi_types::ThinkingLevel::Off,
            supports_image: false,
            context_window: None,
            max_tokens: None,
            base_url: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            per_request_price: None,
        }
    }

    #[test]
    fn request_uses_input_array() {
        let req = build_openai_responses_request(&model(), &[], &[]);
        assert_eq!(req["model"], "gpt-4o");
        assert_eq!(req["stream"], true);
        assert!(req.get("input").is_some());
        assert_eq!(
            req["prompt_cache_key"].as_str().map(str::len),
            Some("lofi:".len() + 16)
        );
        assert!(req.get("tools").is_none());
        assert!(req.get("reasoning").is_none());
    }

    #[test]
    fn request_cache_key_is_stable_while_history_is_appended() {
        let system = Message {
            role: lofi_types::Role::System,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: "stable instructions".to_string(),
            }],
        };
        let first_user = Message {
            role: lofi_types::Role::User,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: "initial prompt".to_string(),
            }],
        };
        let appended = Message {
            role: lofi_types::Role::Assistant,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: "new response".to_string(),
            }],
        };
        let first =
            build_openai_responses_request(&model(), &[system.clone(), first_user.clone()], &[]);
        let second = build_openai_responses_request(&model(), &[system, first_user, appended], &[]);
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
    }

    #[test]
    fn request_cache_key_changes_when_prefix_is_rebuilt() {
        let message = |role, text: &str| Message {
            role,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: text.to_string(),
            }],
        };
        let first = build_openai_responses_request(
            &model(),
            &[
                message(lofi_types::Role::System, "shared system"),
                message(lofi_types::Role::User, "prefix one"),
            ],
            &[],
        );
        let second = build_openai_responses_request(
            &model(),
            &[
                message(lofi_types::Role::System, "shared system"),
                message(lofi_types::Role::User, "prefix two"),
            ],
            &[],
        );
        assert_ne!(first["prompt_cache_key"], second["prompt_cache_key"]);
    }

    #[test]
    fn request_enables_reasoning_summary_when_thinking_is_on() {
        let mut model = model();
        model.thinking = ThinkingLevel::Medium;
        let req = build_openai_responses_request(&model, &[], &[]);
        assert_eq!(
            req["reasoning"],
            json!({"effort": "medium", "summary": "auto"})
        );
    }

    #[test]
    fn maps_text_delta() {
        let ev = json!({"type":"response.output_text.delta","delta":"hi"});
        assert_eq!(
            map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
            vec![StreamingEvent::TextDelta("hi".to_string())]
        );
    }

    #[test]
    fn maps_tool_use_lifecycle() {
        let mut state = ResponsesMapperState::default();
        let added = json!({
            "type":"response.output_item.added",
            "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec","arguments":""}
        });
        assert_eq!(
            map_openai_responses_event(&added, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseStart {
                id: "call_1".to_string(),
                name: "exec".to_string()
            }]
        );
        // Argument deltas carry `item_id` (the output-item id), not `call_id`;
        // the mapper must translate `fc_1` -> `call_1`.
        let d = json!({
            "type":"response.function_call_arguments.delta",
            "item_id":"fc_1",
            "delta":"{\"x\":1}"
        });
        assert_eq!(
            map_openai_responses_event(&d, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseInputDelta {
                id: "call_1".to_string(),
                delta: "{\"x\":1}".to_string()
            }]
        );
        let done = json!({
            "type":"response.output_item.done",
            "item":{"type":"function_call","call_id":"call_1","name":"exec","arguments":"{\"x\":1}"}
        });
        assert_eq!(
            map_openai_responses_event(&done, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseEnd {
                id: "call_1".to_string()
            }]
        );
    }

    #[test]
    fn maps_completed_reasoning_item_when_deltas_are_absent() {
        let ev = json!({
            "type":"response.output_item.done",
            "item":{
                "type":"reasoning",
                "id":"rs_1",
                "summary":[
                    {"type":"summary_text","text":"Checked the inputs."},
                    {"type":"summary_text","text":"Selected the result."}
                ]
            }
        });
        assert_eq!(
            map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
            vec![StreamingEvent::ThinkingDelta(
                "Checked the inputs.\n\nSelected the result.".to_string()
            )]
        );
    }

    #[test]
    fn completed_reasoning_item_preserves_raw_placeholder_parts() {
        let ev = json!({
            "type":"response.output_item.done",
            "item":{
                "type":"reasoning",
                "id":"rs_1",
                "summary":[
                    {"type":"summary_text","text":" **Checking**\n<!-- --> "},
                    {"type":"summary_text","text":"Kept <!-- --> literally."},
                    {"type":"summary_text","text":" **Done**\nResult "}
                ]
            }
        });
        assert_eq!(
            map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
            vec![StreamingEvent::ThinkingDelta(
                " **Checking**\n<!-- --> \n\nKept <!-- --> literally.\n\n **Done**\nResult "
                    .to_string()
            )]
        );
    }

    #[test]
    fn summary_part_done_separates_streamed_parts() {
        let mut state = ResponsesMapperState::default();
        let first = json!({
            "type":"response.reasoning_summary_text.delta",
            "item_id":"rs_1",
            "delta":"First"
        });
        let part_done = json!({
            "type":"response.reasoning_summary_part.done",
            "item_id":"rs_1"
        });
        let second = json!({
            "type":"response.reasoning_summary_text.delta",
            "item_id":"rs_1",
            "delta":"Second"
        });
        map_openai_responses_event(&first, &mut state).unwrap();
        assert_eq!(
            map_openai_responses_event(&part_done, &mut state).unwrap(),
            vec![StreamingEvent::ThinkingDelta("\n\n".to_string())]
        );
        assert_eq!(
            map_openai_responses_event(&second, &mut state).unwrap(),
            vec![StreamingEvent::ThinkingDelta("Second".to_string())]
        );
    }

    #[test]
    fn completed_reasoning_item_does_not_duplicate_streamed_summary() {
        let mut state = ResponsesMapperState::default();
        let delta = json!({
            "type":"response.reasoning_summary_text.delta",
            "item_id":"rs_1",
            "delta":"Checked the inputs."
        });
        map_openai_responses_event(&delta, &mut state).unwrap();
        let done = json!({
            "type":"response.output_item.done",
            "item":{
                "type":"reasoning",
                "id":"rs_1",
                "summary":[{"type":"summary_text","text":"Checked the inputs."}]
            }
        });
        assert!(map_openai_responses_event(&done, &mut state)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn failed_event_is_error() {
        let ev = json!({"type":"response.failed","error":{"message":"boom"}});
        let r = map_openai_responses_event(&ev, &mut ResponsesMapperState::default());
        assert!(r.is_err());
    }

    #[test]
    fn maps_completed_usage() {
        let ev = json!({
            "type":"response.completed",
            "response":{"usage":{"input_tokens":3,"output_tokens":7,"input_tokens_details":{"cached_tokens":1}}}
        });
        let done = map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap();
        let StreamingEvent::Done(u) = done.into_iter().next().unwrap() else {
            panic!("expected Done");
        };
        // input_tokens excludes the cached slice: input_tokens(3) - cached(1)
        assert_eq!(u.input_tokens, 2);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cache_read_tokens, 1);
    }
}
