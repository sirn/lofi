#![allow(clippy::wildcard_imports)]

use super::*;

/// Truncate a tool-result string to at most `max` bytes (on a UTF-8 char
/// boundary), keeping the head and appending a truncation marker.
pub(crate) fn cap_tool_result_to(content: &str, max: usize) -> String {
    if content.len() <= max {
        return content.to_string();
    }
    let mut end = max;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[output truncated: {} bytes total, {} shown]",
        &content[..end],
        content.len(),
        end
    )
}

/// Per-native-tool cap. See [`MAX_TOOL_RESULT_BYTES`].
pub(crate) fn cap_tool_result(content: &str) -> String {
    cap_tool_result_to(content, MAX_TOOL_RESULT_BYTES)
}

/// Exec-level outer cap. See [`MAX_EXEC_RESULT_BYTES`].
pub(crate) fn cap_exec_result(content: &str) -> String {
    cap_tool_result_to(content, MAX_EXEC_RESULT_BYTES)
}

/// The single LLM-facing tool schema: `exec`.
#[must_use]
pub fn exec_tool_schema() -> ToolSchema {
    ToolSchema {
        name: "exec".to_string(),
        description: "Compile and run a TypeScript program in a sandboxed QuickJS runtime. The program has access to a `lofi` object with file/shell/search tools (read, ls, find, grep, write, edit, bash) and a `lofi.agent(prompt, opts?)` subagent helper. Top-level await and return are supported. The returned value is sent back as the tool result; keep it compact and final.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "TypeScript source. Top-level await/return supported."
                },
                "strings": {
                    "type": "object",
                    "description": "Named string constants exposed as the global `lofi_strings` object."
                },
                "display": {
                    "type": "object",
                    "description": "Optional display metadata; ignored by the runtime."
                }
            },
            "required": ["code"]
        }),
    }
}

pub(crate) fn decode_json_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    let mut escape = false;
    while let Some(c) = chars.next() {
        if escape {
            match c {
                'n' => out.push(char::from(0x0A)),
                't' => out.push(char::from(0x09)),
                'r' => out.push(char::from(0x0D)),
                'b' => out.push(char::from(0x08)),
                'f' => out.push(char::from(0x0C)),
                'u' => {
                    let mut hex = String::with_capacity(4);
                    for _ in 0..4 {
                        match chars.next() {
                            Some(h) => hex.push(h),
                            None => return out,
                        }
                    }
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                other => out.push(other),
            }
            escape = false;
        } else if c == char::from(0x5C) {
            escape = true;
        } else if c == char::from(0x22) {
            return out;
        } else {
            out.push(c);
        }
    }
    out
}

/// Best-effort incremental extraction of the `code` string field from a
/// partial tool-input JSON buffer. Returns the decoded content available so
/// far, so the exec source can be streamed live as the model writes it.
pub(crate) fn extract_code_prefix(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let n = bytes.len();
    let code_key: &[u8] = &[0x22, b'c', b'o', b'd', b'e', 0x22];
    let mut i = 0;
    let mut in_str = false;
    let mut escape = false;
    while i < n {
        let c = bytes[i];
        if in_str {
            if escape {
                escape = false;
            } else if c == 0x5C {
                escape = true;
            } else if c == 0x22 {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == 0x22 {
            if i + code_key.len() <= n && &bytes[i..i + code_key.len()] == code_key {
                i += code_key.len();
                while i < n && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= n || bytes[i] != b':' {
                    return String::new();
                }
                i += 1;
                while i < n && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= n {
                    return String::new();
                }
                if bytes[i] != 0x22 {
                    return String::new();
                }
                i += 1;
                return decode_json_string(&raw[i..]).trim_matches('\n').to_string();
            }
            in_str = true;
            i += 1;
            continue;
        }
        i += 1;
    }
    String::new()
}

/// Parse an `exec` tool input into `(code, strings, display)`.
///
/// Missing `code` yields an empty string (which compiles to a no-op).
/// `strings` values are coerced to strings via `serde_json` for non-string
/// entries. `display` is returned as-is for future use.
pub fn parse_exec_input(
    input: &serde_json::Value,
) -> (String, HashMap<String, String>, serde_json::Value) {
    let code = input
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim_matches('\n')
        .to_string();
    let mut strings = HashMap::new();
    if let Some(obj) = input.get("strings").and_then(serde_json::Value::as_object) {
        for (k, v) in obj {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            strings.insert(k.clone(), s);
        }
    }
    let display = input
        .get("display")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    (code, strings, display)
}

/// Extract a short UI label from an exec call's `display` field: a bare
/// string is used directly, an object is probed for `name`/`title`/
/// `description`.
#[must_use]
pub fn exec_label(display: &serde_json::Value) -> Option<String> {
    if let Some(s) = display.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    if let Some(obj) = display.as_object() {
        for key in ["name", "title", "description", "task"] {
            if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                let s = s.trim();
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// UI-facing extract: the TypeScript `code` and optional `display` label for
/// an `exec` tool-call input. Used by the TUI when restoring a session.
#[must_use]
pub fn exec_input_code_and_label(input: &serde_json::Value) -> (String, Option<String>) {
    let (code, _strings, display) = parse_exec_input(input);
    (code, exec_label(&display))
}

/// UI-facing extract of an `exec` call's result: on success the payload is
/// `{ "value": ..., "logs": [...] }` and we surface `value` (a string as-is,
/// anything else pretty-printed); on error the raw message is returned.
#[must_use]
pub fn exec_result_display(result: &str, is_error: bool) -> String {
    if is_error {
        return result.to_string();
    }
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|v| v.get("value").cloned())
        .map_or_else(|| result.to_string(), |v| {
            if let Some(s) = v.as_str() {
                s.to_string()
            } else {
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            }
        })
}
