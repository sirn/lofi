#![allow(clippy::wildcard_imports)]

use super::*;

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

pub(crate) fn cap_tool_result(content: &str) -> String {
    cap_tool_result_to(content, MAX_TOOL_RESULT_BYTES)
}

pub(crate) fn cap_exec_result(content: &str) -> String {
    cap_tool_result_to(content, MAX_EXEC_RESULT_BYTES)
}

#[must_use]
pub fn exec_tool_schema() -> ToolSchema {
    ToolSchema {
        name: lofi_code::EXEC_TOOL_NAME.to_string(),
        description: lofi_code::EXEC_TOOL_DESCRIPTION.to_string(),
        input_schema: lofi_code::exec_tool_input_schema(),
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

#[derive(Default)]
pub(crate) struct CodePrefixDecoder {
    code_start: Option<usize>,
    processed: usize,
    decoded: String,
    done: bool,
}

impl CodePrefixDecoder {
    pub(crate) fn update(&mut self, raw: &str) -> &str {
        if self.done {
            return self.decoded.trim_matches('\n');
        }
        if self.code_start.is_none() {
            let prefix = extract_code_value_start(raw);
            if let Some(start) = prefix {
                self.code_start = Some(start);
                self.processed = start;
            } else {
                return "";
            }
        }

        let suffix = &raw[self.processed..];
        let complete = complete_json_string_prefix(suffix);
        self.decoded
            .push_str(&decode_json_string(&suffix[..complete]));
        self.processed += complete;
        self.done = complete > 0 && suffix.as_bytes()[complete - 1] == b'"';
        self.decoded.trim_matches('\n')
    }
}

fn extract_code_value_start(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    let code_key = b"\"code\"";
    let mut i = 0;
    let mut in_str = false;
    let mut escape = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'\"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'\"' {
            if bytes[i..].starts_with(code_key) {
                i += code_key.len();
                while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
                    i += 1;
                }
                if bytes.get(i) != Some(&b':') {
                    return None;
                }
                i += 1;
                while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
                    i += 1;
                }
                return (bytes.get(i) == Some(&b'\"')).then_some(i + 1);
            }
            in_str = true;
        }
        i += 1;
    }
    None
}

fn complete_json_string_prefix(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\"' {
            return i + 1;
        }
        if bytes[i] == b'\\' {
            if i + 1 >= bytes.len() {
                break;
            }
            if bytes[i + 1] == b'u' && i + 6 > bytes.len() {
                break;
            }
            i += if bytes[i + 1] == b'u' { 6 } else { 2 };
        } else {
            i += 1;
        }
    }
    i
}

#[cfg(test)]
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

#[must_use]
pub fn exec_input_code_and_label(input: &serde_json::Value) -> (String, Option<String>) {
    let (code, _strings, display) = parse_exec_input(input);
    (code, exec_label(&display))
}

#[must_use]
pub fn exec_result_display(result: &str, is_error: bool) -> String {
    if is_error {
        return result.to_string();
    }
    let Ok(output) = serde_json::from_str::<serde_json::Value>(result) else {
        return result.to_string();
    };
    if let Some(value) = output.get("value") {
        return value.as_str().map_or_else(
            || serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
            str::to_string,
        );
    }
    if output.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        return output
            .get("logs")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    result.to_string()
}
