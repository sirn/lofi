use std::time::Duration;

use ratatui::style::Style;
use ratatui::text::Span;

use crate::tui::{Block, DetailKey, NativePreview, NativeTool, Turn};

use super::detail::{detail_box, detail_row_count};
use super::RenderWindow;
use crate::tui::view::component::{Component, Cx};
use crate::tui::view::prim::{self, RenderLine};

const PREVIEW_LINES: usize = 3;

pub(super) struct ExecBlockBranch<'a> {
    pub(super) parent: u64,
    pub(super) nt: &'a NativeTool,
    pub(super) is_last: bool,
}

pub(super) struct NativeBody {
    pub(super) lines: Vec<String>,
    numbered: bool,
    start_line: usize,
    is_diff: bool,
    pub(super) notice: Option<String>,
}

pub(super) fn native_body(nt: &NativeTool) -> NativeBody {
    let name = nt.name.as_str();
    let raw = nt.result.as_deref().unwrap_or("");
    if nt.is_error {
        // Failures answer with a message field; bash reports its stderr as
        // output. Show that line, not the envelope.
        let field = if name == "bash" { "output" } else { "error" };
        let display = serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|value| {
                value
                    .get(field)
                    .or_else(|| value.get("error"))?
                    .as_str()
                    .map(str::to_string)
            })
            .unwrap_or_else(|| raw.to_string());
        return NativeBody {
            lines: split_lines(&display),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        };
    }
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        // Non-JSON result (e.g. a plain error string or an older
        // persisted string): show it verbatim rather than dropping it.
        Err(_) => {
            return NativeBody {
                lines: split_lines(raw),
                numbered: matches!(name, "read" | "view" | "bash_read"),
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let b = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    match name {
        "write" => NativeBody {
            lines: split_lines(s("content")),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
        "edit" => NativeBody {
            lines: edit_diff(s("old"), s("new")),
            numbered: false,
            start_line: 1,
            is_diff: true,
            notice: None,
        },
        "bash" => NativeBody {
            lines: split_lines(s("output")),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
        "read" | "view" | "bash_read" => {
            let start = n("start_line").max(1);
            let total = n("total_lines");
            let lines = split_lines(s("content"));
            let notice = b("truncated").then_some(format!(
                "(showing {start}-{} of {total}; use offset={next} to continue)",
                start + lines.len().saturating_sub(1),
                next = start + lines.len()
            ));
            NativeBody {
                lines,
                numbered: true,
                start_line: start,
                is_diff: false,
                notice,
            }
        }
        "ls" => {
            let lines = v.get("entries").and_then(|x| x.as_array()).map_or_else(
                || split_lines(raw),
                |a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                },
            );
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("truncated").then_some("(truncated)".into()),
            }
        }
        "find" => {
            let lines = v.get("matches").and_then(|x| x.as_array()).map_or_else(
                || split_lines(raw),
                |a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                },
            );
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("truncated").then_some("(truncated)".into()),
            }
        }
        // A settled run answers with result and a failed one with error;
        // neither carries the model name, so the body reports status and
        // spend instead.
        "subagent" | "subagentWait" => {
            let text = if s("result").is_empty() {
                s("error")
            } else {
                s("result")
            };
            let status = s("status");
            let cost = v
                .get("usage")
                .and_then(|u| u.get("cost"))
                .and_then(serde_json::Value::as_f64);
            let truncated = b("truncated").then(|| {
                format!(
                    "(truncated, {} bytes)",
                    v.get("full_bytes")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0)
                )
            });
            let notice = match (cost, truncated) {
                (Some(cost), Some(t)) => Some(format!("({status}, ${cost:.4}) {t}")),
                (Some(cost), None) => Some(format!("({status}, ${cost:.4})")),
                (None, truncated) => {
                    truncated.or_else(|| (!status.is_empty()).then(|| format!("({status})")))
                }
            };
            NativeBody {
                lines: split_lines(text),
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice,
            }
        }
        "grep" => {
            let mut lines = Vec::new();
            if let Some(arr) = v.get("matches").and_then(|x| x.as_array()) {
                for m in arr {
                    let file = m.get("file").and_then(|x| x.as_str()).unwrap_or("");
                    let line = m
                        .get("line")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let content = m.get("content").and_then(|x| x.as_str()).unwrap_or("");
                    lines.push(format!("{file}:{line}:{content}"));
                }
            }
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("truncated").then_some("(truncated)".into()),
            }
        }
        "jobRead" => {
            let output = s("output");
            let cursor = v
                .get("cursor")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let total = v
                .get("totalBytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let done = b("done");
            let notice = if done {
                Some(format!("(end of log; {total} bytes total)"))
            } else if total > 0 {
                Some(format!("(read {cursor} of {total} bytes)"))
            } else {
                None
            };
            NativeBody {
                lines: split_lines(output),
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice,
            }
        }
        "jobSpawn" | "jobStatus" | "jobKill" | "jobNotify" => {
            let mut lines: Vec<String> = Vec::new();
            let state = s("state");
            if !state.is_empty() {
                lines.push(format!("state: {state}"));
            }
            let mut stats: Vec<String> = Vec::new();
            if let Some(code) = v.get("exitCode").and_then(serde_json::Value::as_i64) {
                stats.push(format!("exit {code}"));
            }
            if let Some(sig) = v.get("signal").and_then(serde_json::Value::as_i64) {
                stats.push(format!("signal {sig}"));
            }
            if let Some(ms) = v.get("durationMs").and_then(serde_json::Value::as_u64) {
                stats.push(format!(
                    "duration {}",
                    prim::fmt_duration(std::time::Duration::from_millis(ms))
                ));
            }
            if !stats.is_empty() {
                lines.push(stats.join("  "));
            }
            let mut notify_bits: Vec<String> = Vec::new();
            if v.get("notify").is_some() {
                notify_bits.push(format!("notify: {}", b("notify")));
            }
            // jobNotify answers with `intervalMs`/`changed`; the job
            // snapshot behind jobSpawn/jobStatus/jobKill uses the
            // `notify*` names.
            if let Some(iv) = v
                .get("notifyIntervalMs")
                .or_else(|| v.get("intervalMs"))
                .and_then(serde_json::Value::as_u64)
            {
                notify_bits.push(format!(
                    "interval {}",
                    prim::fmt_duration(std::time::Duration::from_millis(iv))
                ));
            }
            if let Some(changed) = v
                .get("notifyChanged")
                .or_else(|| v.get("changed"))
                .and_then(serde_json::Value::as_bool)
            {
                notify_bits.push(format!("changed {changed}"));
            }
            if !notify_bits.is_empty() {
                lines.push(notify_bits.join("  "));
            }
            let log_path = s("logPath");
            if !log_path.is_empty() {
                lines.push(format!("log: {log_path}"));
            }
            if lines.is_empty() {
                let id = s("id");
                if !id.is_empty() {
                    lines.push(format!("job {id}"));
                }
            }
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        "patch" => NativeBody {
            lines: vec![format!("{} — {} hunks applied", s("path"), n("hunks"))],
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
        "skills" => {
            let lines = v
                .get("skills")
                .and_then(|x| x.as_array())
                .map_or_else(Vec::new, |a| {
                    a.iter()
                        .map(|e| {
                            let name = e.get("name").and_then(|x| x.as_str()).unwrap_or("");
                            let description =
                                e.get("description").and_then(|x| x.as_str()).unwrap_or("");
                            if description.is_empty() {
                                name.to_string()
                            } else {
                                format!("{name} — {description}")
                            }
                        })
                        .collect()
                });
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        "skill" => {
            let name = s("name");
            NativeBody {
                lines: split_lines(s("content")),
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: (!name.is_empty()).then(|| format!("({name})")),
            }
        }
        "docs" => {
            let content = s("content");
            if content.is_empty() {
                let lines =
                    v.get("entries")
                        .and_then(|x| x.as_array())
                        .map_or_else(Vec::new, |a| {
                            a.iter()
                                .map(|e| {
                                    let name = e.get("name").and_then(|x| x.as_str()).unwrap_or("");
                                    let summary =
                                        e.get("summary").and_then(|x| x.as_str()).unwrap_or("");
                                    format!("{name} — {summary}")
                                })
                                .collect()
                        });
                NativeBody {
                    lines,
                    numbered: false,
                    start_line: 1,
                    is_diff: false,
                    notice: None,
                }
            } else {
                let name = s("name");
                NativeBody {
                    lines: split_lines(content),
                    numbered: false,
                    start_line: 1,
                    is_diff: false,
                    notice: (!name.is_empty()).then(|| format!("({name})")),
                }
            }
        }
        "docsSearch" => {
            let lines = v
                .get("results")
                .and_then(|x| x.as_array())
                .map_or_else(Vec::new, |a| {
                    a.iter()
                        .map(|r| {
                            let name = r.get("name").and_then(|x| x.as_str()).unwrap_or("");
                            let excerpt = r.get("excerpt").and_then(|x| x.as_str()).unwrap_or("");
                            format!("{name} — {excerpt}")
                        })
                        .collect()
                });
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        "models" => {
            let lines = v
                .get("models")
                .and_then(|x| x.as_array())
                .map_or_else(Vec::new, |a| {
                    a.iter()
                        .map(|m| {
                            let model = m.get("model").and_then(|x| x.as_str()).unwrap_or("");
                            let name = m.get("name").and_then(|x| x.as_str()).unwrap_or("");
                            if name.is_empty() {
                                model.to_string()
                            } else {
                                format!("{model} ({name})")
                            }
                        })
                        .collect()
                });
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        "recall" => {
            let status = s("status");
            NativeBody {
                lines: split_lines(s("text")),
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: (!status.is_empty() && status != "ok").then(|| format!("({status})")),
            }
        }
        "result" => {
            let text = v.as_str().map_or_else(|| raw.to_string(), String::from);
            NativeBody {
                lines: split_lines(&text),
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        "jobList" => {
            let lines = v
                .get("jobs")
                .and_then(|x| x.as_array())
                .map_or_else(Vec::new, |a| {
                    a.iter()
                        .map(|j| {
                            let id = j.get("id").and_then(|x| x.as_str()).unwrap_or("");
                            let state = j.get("state").and_then(|x| x.as_str()).unwrap_or("");
                            let cmd = j.get("command").and_then(|x| x.as_str()).unwrap_or("");
                            format!("job {id} {state}: {cmd}")
                        })
                        .collect()
                });
            let empty = lines.is_empty();
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: empty.then(|| "(no jobs)".to_string()),
            }
        }
        "jobScreen" => {
            let lines = v
                .get("lines")
                .and_then(|x| x.as_array())
                .map_or_else(Vec::new, |a| {
                    a.iter()
                        .filter_map(|l| l.as_str().map(String::from))
                        .collect()
                });
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: Some(format!("({}x{})", n("cols"), n("rows"))),
            }
        }
        "jobType" => NativeBody {
            lines: vec![format!("sent {} bytes to job {}", n("sent"), s("id"))],
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
        "jobKeyPress" => NativeBody {
            lines: vec![format!("sent {} to job {}", s("key"), s("id"))],
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
        "subagentSpawn" | "subagentStatus" | "subagentKill" | "subagentNotify" => {
            let id = s("id");
            let status = s("status");
            let mut lines = Vec::new();
            if !id.is_empty() {
                lines.push(format!("id: {id}"));
            }
            if !status.is_empty() {
                lines.push(format!("state: {status}"));
            }
            for (label, value) in [("note", s("note")), ("error", s("error"))] {
                if !value.is_empty() {
                    lines.push(format!("{label}: {value}"));
                }
            }
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        "subagentList" | "subagentMailboxList" | "subagentMailboxReceive" => {
            let mut lines = Vec::new();
            if let Some(rows) = v.get("entries").and_then(|x| x.as_array()) {
                for e in rows {
                    let id = e.get("id").and_then(|x| x.as_str()).unwrap_or("");
                    let status = e.get("status").and_then(|x| x.as_str()).unwrap_or("");
                    lines.push(format!("{id} — {status}"));
                }
            }
            if let Some(rows) = v.get("inboxes").and_then(|x| x.as_array()) {
                for e in rows {
                    let id = e.get("agent_id").and_then(|x| x.as_str()).unwrap_or("");
                    let pending = e
                        .get("pending")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    lines.push(format!("{id}: {pending} pending"));
                }
            }
            if let Some(rows) = v.get("messages").and_then(|x| x.as_array()) {
                lines.extend(rows.iter().map(message_line));
            }
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        "subagentMailboxSend" => {
            // A deduped retry answers without a `to` field.
            let line = if b("duplicate") {
                format!("duplicate of seq {}", n("seq"))
            } else {
                format!("sent to {} (seq {})", s("to"), n("seq"))
            };
            NativeBody {
                lines: vec![line],
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        "subagentRead" => {
            let lines = v
                .get("events")
                .and_then(|x| x.as_array())
                .map_or_else(Vec::new, |a| {
                    a.iter()
                        .map(|e| {
                            let seq = e
                                .get("seq")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            let kind = e.get("type").and_then(|x| x.as_str()).unwrap_or("");
                            // The payload field differs per event kind: text
                            // for text, result for tool_end, name for
                            // tool_start, status for state.
                            let body = ["text", "result", "name", "status"]
                                .iter()
                                .find_map(|k| e.get(*k).and_then(|x| x.as_str()))
                                .unwrap_or("");
                            // Round usage reports cost and tokens; neither
                            // is a string payload.
                            let body = if body.is_empty() && kind == "usage" {
                                e.get("cost")
                                    .and_then(serde_json::Value::as_f64)
                                    .map_or_else(String::new, |cost| format!("${cost:.4}"))
                            } else {
                                body.to_string()
                            };
                            let first = body.lines().next().unwrap_or("");
                            if first.is_empty() {
                                format!("#{seq} {kind}")
                            } else {
                                format!("#{seq} {kind}: {first}")
                            }
                        })
                        .collect()
                });
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("done").then(|| "(end of events)".to_string()),
            }
        }
        _ => NativeBody {
            lines: split_lines(raw),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
    }
}

/// Notice for a preview that streams an encoded field. The body is not
/// parsed, so the docs/skill name comes from the args: a scan for the field
/// would also match that text inside the encoded content.
fn encoded_notice(name: &str, native: &NativeTool) -> Option<String> {
    matches!(name, "docs" | "skill")
        .then(|| native.args.trim())
        .filter(|args| !args.is_empty())
        .map(|args| format!("({args})"))
}

pub(in crate::tui) fn cache_native_preview(native: &mut NativeTool) {
    let Some(raw) = native.result.as_deref().filter(|result| !result.is_empty()) else {
        return;
    };
    let header_suffix = native_header_suffix(&native.name, Some(raw));
    let encoded_field = match native.name.as_str() {
        "bash" => Some("output"),
        "write" | "docs" | "skill" => Some("content"),
        _ => None,
    };
    let encoded = encoded_field.and_then(|field| json_string_field(raw, field));
    let body = encoded.is_none().then(|| native_body(native));
    // An encoded preview never builds the body, so its notice has to be
    // derived separately or it is lost once the result is dropped.
    let notice = body.as_ref().map_or_else(
        || encoded_notice(&native.name, native),
        |body| body.notice.clone(),
    );
    let total_lines = encoded.map_or_else(
        || body.as_ref().map_or(0, |body| body.lines.len()),
        encoded_json_line_count,
    );
    let range = native_preview_range(&native.name, total_lines);
    let mut lines = Vec::with_capacity(range.len());
    if let Some(encoded) = encoded {
        for_each_encoded_json_line(encoded, range.clone(), |_, line| lines.push(line));
    } else if let Some(body) = &body {
        lines.extend(body.lines[range.clone()].iter().cloned());
    }
    native.preview = Some(Box::new(NativePreview {
        header_suffix,
        lines,
        total_lines,
        preview_start: range.start,
        numbered: body.as_ref().is_some_and(|body| body.numbered),
        start_line: body.as_ref().map_or(1, |body| body.start_line),
        is_diff: body.as_ref().is_some_and(|body| body.is_diff),
        notice,
    }));
}

pub(crate) fn compact_native_previews(turn: &mut Turn) {
    for block in &mut turn.blocks {
        let Block::Tool(tool) = block else { continue };
        for native in &mut tool.native {
            if native.is_error || native.result.as_deref().is_none_or(str::is_empty) {
                continue;
            }
            cache_native_preview(native);
            native.result = None;
        }
    }
}

/// Tools whose preview shows the tail: a process log, a terminal screen, or
/// a paged read all end with the newest or most useful line.
fn previews_tail(name: &str) -> bool {
    matches!(name, "bash" | "jobRead" | "jobScreen")
}

pub(super) fn native_preview_range(name: &str, total: usize) -> std::ops::Range<usize> {
    let shown = total.min(PREVIEW_LINES);
    let start = if previews_tail(name) {
        total.saturating_sub(shown)
    } else {
        0
    };
    start..start + shown
}

pub(super) fn split_lines(s: &str) -> Vec<String> {
    s.trim_end_matches('\n')
        .split('\n')
        .map(String::from)
        .collect()
}

/// Collapse JSON tool args into a short header label for known tools so
/// the `Tool <name> <args>` line scans like a sentence instead of dumping
/// the request envelope. Returns `None` for tools we have no tailored
/// view for; the caller falls back to the verbatim args.
///
/// Only shapes the surface actually emits are recognised (job ops and
/// subagent lookups take an `id`; `jobSpawn` takes a command; a mailbox
/// send takes `to`). Anything else returns `None`.
pub(super) fn summarize_tool_args(name: &str, args: &str) -> Option<String> {
    if args.is_empty() {
        return Some(String::new());
    }
    // Job and subagent ids arrive as strings from the bindings, but the
    // numeric form is accepted too.
    let id = json_string_field(args, "id")
        .map(str::to_string)
        .or_else(|| json_u64_field(args, "id").map(|n| n.to_string()));
    match name {
        "jobSpawn" => json_string_field(args, "cmd").map(|cmd| truncate_args_display(cmd, 60)),
        "jobStatus" | "jobRead" | "jobKill" | "jobNotify" | "jobScreen" | "jobType"
        | "jobKeyPress" => id.map(|s| format!("job {s}")),
        "subagentStatus" | "subagentRead" | "subagentKill" | "subagentNotify" | "subagentWait" => {
            id.map(|s| format!("subagent {s}"))
        }
        "subagentMailboxSend" => json_string_field(args, "to").map(|to| format!("to {to}")),
        _ => None,
    }
}

/// Cap an args-rendered text cell at `width` chars (multi-byte safe),
/// replacing the overflow with U+2026.
fn truncate_args_display(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut s: String = text.chars().take(width.saturating_sub(1)).collect();
    s.push('\u{2026}');
    s
}

fn json_string_field<'a>(raw: &'a str, field: &str) -> Option<&'a str> {
    let needle = format!("\"{field}\"");
    let key = raw.find(&needle)?;
    let bytes = raw.as_bytes();
    let mut i = key + needle.len();
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
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let start = i + 1;
    i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = i.saturating_add(2),
            b'"' => return Some(&raw[start..i]),
            _ => i += 1,
        }
    }
    None
}

fn json_u64_field(raw: &str, field: &str) -> Option<u64> {
    let needle = format!("\"{field}\"");
    let key = raw.find(&needle)?;
    let mut rest = raw.get(key + needle.len()..)?.trim_start();
    rest = rest.strip_prefix(':')?.trim_start();
    let end = rest.bytes().take_while(u8::is_ascii_digit).count();
    (end > 0).then(|| rest[..end].parse().ok()).flatten()
}

fn json_f64_field(raw: &str, field: &str) -> Option<f64> {
    let needle = format!("\"{field}\"");
    let key = raw.find(&needle)?;
    let mut rest = raw.get(key + needle.len()..)?.trim_start();
    rest = rest.strip_prefix(':')?.trim_start();
    let end = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'e' | b'+'))
        .count();
    rest.get(..end)?.parse().ok()
}

fn encoded_json_line_count(encoded: &str) -> usize {
    let bytes = encoded.as_bytes();
    let mut i = 0usize;
    let mut breaks = 0usize;
    let mut trailing = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'n' {
                breaks += 1;
                trailing += 1;
            } else {
                trailing = 0;
            }
            i += 2;
        } else {
            trailing = 0;
            i += 1;
        }
    }
    breaks.saturating_add(1).saturating_sub(trailing).max(1)
}

fn for_each_encoded_json_line(
    encoded: &str,
    range: std::ops::Range<usize>,
    mut f: impl FnMut(usize, String),
) {
    let wanted_end = range.end.min(encoded_json_line_count(encoded));
    if range.start >= wanted_end {
        return;
    }
    let bytes = encoded.as_bytes();
    let mut start = 0usize;
    let mut line = 0usize;
    let mut i = 0usize;
    while i <= bytes.len() && line < wanted_end {
        let at_break =
            i == bytes.len() || (bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'n');
        if at_break {
            if line >= range.start {
                let quoted = format!("\"{}\"", &encoded[start..i]);
                if let Ok(decoded) = serde_json::from_str::<String>(&quoted) {
                    f(line, decoded);
                }
            }
            line += 1;
            if i == bytes.len() {
                break;
            }
            i += 2;
            start = i;
        } else if bytes[i] == b'\\' {
            i = i.saturating_add(2);
        } else {
            i += 1;
        }
    }
}

fn native_header_suffix(name: &str, result: Option<&str>) -> Option<String> {
    let raw = result?;
    if raw.is_empty() {
        return None;
    }
    if matches!(name, "read" | "view" | "bash_read") {
        let start = json_u64_field(raw, "start_line")?.max(1);
        let total = json_u64_field(raw, "total_lines")?;
        let shown = json_string_field(raw, "content").map(encoded_json_line_count)? as u64;
        let end = start.saturating_add(shown.saturating_sub(1)).min(total);
        return Some(format!("(lines {start}-{end})"));
    }
    if name == "bash" {
        if let Some(ms) = json_u64_field(raw, "duration_ms") {
            return Some(format!(
                "(took {})",
                prim::fmt_duration(Duration::from_millis(ms))
            ));
        }
    }
    if matches!(name, "subagent" | "subagentWait") {
        let status = json_string_field(raw, "status")?;
        return Some(match json_f64_field(raw, "cost") {
            Some(cost) => format!("({status}, ${cost:.4})"),
            None => format!("({status})"),
        });
    }
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    match name {
        "read" | "view" | "bash_read" => {
            let start = v.get("start_line").and_then(serde_json::Value::as_u64)?;
            if start == 0 {
                return None;
            }
            let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
            let count = content.split('\n').count();
            if count == 0 {
                return None;
            }
            let end = start + count as u64 - 1;
            if end <= start {
                return None;
            }
            Some(format!("(lines {start}-{end})"))
        }
        "bash" => {
            let ms = v.get("duration_ms").and_then(serde_json::Value::as_u64)?;
            if ms == 0 {
                return None;
            }
            let dur = std::time::Duration::from_millis(ms);
            Some(format!("(took {})", prim::fmt_duration(dur)))
        }
        _ => None,
    }
}

/// One mailbox message as a preview line: `from: first body line`.
fn message_line(m: &serde_json::Value) -> String {
    let from = m.get("from").and_then(|x| x.as_str()).unwrap_or("");
    let body = m.get("body").and_then(|x| x.as_str()).unwrap_or("");
    let first = body.lines().next().unwrap_or("");
    if from.is_empty() {
        first.to_string()
    } else {
        format!("{from}: {first}")
    }
}

fn edit_diff(old: &str, new: &str) -> Vec<String> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => ' ',
        };
        let val = change.value();
        let line = val.strip_suffix('\n').unwrap_or(val);
        out.push(format!("{prefix}{line}"));
    }
    out
}

impl Component for ExecBlockBranch<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        self.render_window(cx, 0..usize::MAX).lines
    }

    fn height(&self, cx: &Cx) -> usize {
        self.render_window(cx, 0..0).total
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        self.render_window(cx, range).lines
    }
}

impl ExecBlockBranch<'_> {
    fn render_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> RenderWindow {
        let t = cx.theme;
        let working = !self.nt.done && cx.active_turn;
        let exec_cont = if self.is_last { "  " } else { "│ " };
        let key = DetailKey::NativeTool {
            parent: self.parent,
            id: self.nt.id,
        };
        let expanded = cx.app.expanded_details.contains_key(&key);
        let preview = self.nt.preview.as_deref();
        let body = self
            .nt
            .result
            .as_deref()
            .filter(|result| !result.is_empty())
            .filter(|_| expanded || preview.is_none())
            .map(|_| native_body(self.nt));
        let mut lines = if let Some(body) = &body {
            body.lines.clone()
        } else if expanded {
            preview.map_or_else(Vec::new, |preview| preview.lines.clone())
        } else {
            Vec::new()
        };
        if let Some(body) = &body {
            if body.numbered {
                for (index, line) in lines.iter_mut().enumerate() {
                    *line = format!("{:>4} {line}", body.start_line + index);
                }
            }
            if let Some(notice) = &body.notice {
                lines.push(notice.clone());
            }
        } else if expanded {
            if let Some(notice) = preview.and_then(|preview| preview.notice.as_ref()) {
                lines.push(notice.clone());
            }
        }
        let total = body.as_ref().map_or_else(
            || preview.map_or(lines.len(), |preview| preview.total_lines),
            |body| body.lines.len(),
        );
        let detail_deco = vec![
            Span::raw("  "),
            Span::styled(exec_cont, Style::new().fg(t.subtle)),
            Span::raw("  "),
        ];
        let visual_total = if expanded || preview.is_none() {
            detail_row_count(&lines, cx.width, &detail_deco)
        } else {
            total + usize::from(preview.is_some_and(|preview| preview.notice.is_some()))
        };
        let has_detail = if expanded || preview.is_none() {
            visual_total > 0 && lines.iter().any(|line| !line.is_empty())
        } else {
            preview.is_some_and(|preview| {
                preview.notice.is_some() || preview.lines.iter().any(|line| !line.is_empty())
            })
        };

        let mut content = vec![
            Span::styled("Tool ", Style::new().fg(t.muted)),
            Span::styled(self.nt.name.clone(), Style::new().fg(t.info)),
        ];
        let args_label = summarize_tool_args(&self.nt.name, &self.nt.args)
            .unwrap_or_else(|| self.nt.args.clone());
        if !args_label.is_empty() {
            content.push(Span::styled(
                format!(" {args_label}"),
                Style::new().fg(t.subtle),
            ));
        }
        let header_suffix = self
            .nt
            .preview
            .as_ref()
            .and_then(|preview| preview.header_suffix.as_deref())
            .map(str::to_owned)
            .or_else(|| native_header_suffix(&self.nt.name, self.nt.result.as_deref()));
        if let Some(note) = header_suffix {
            content.push(Span::styled(format!(" {note}"), Style::new().fg(t.subtle)));
        }
        let header_deco = vec![
            Span::raw("  "),
            Span::styled(
                if self.is_last { "└ " } else { "├ " },
                Style::new().fg(t.subtle),
            ),
            prim::status_icon(t, working, self.nt.is_error, cx.spinner()),
        ];
        let name_w = 5 + self.nt.name.chars().count();
        let cont_deco = vec![
            Span::raw("  "),
            Span::styled(exec_cont, Style::new().fg(t.subtle)),
            Span::raw(" ".repeat(name_w)),
        ];
        let mut rendered = prim::rline_wrapped(header_deco, &cont_deco, content, cx.width);
        if self.nt.done && has_detail {
            for line in &mut rendered {
                line.detail = Some(super::prim::DetailTarget {
                    key: key.clone(),
                    total: visual_total.max(total),
                    tail: previews_tail(&self.nt.name),
                    row: None,
                });
            }
        }
        rendered.extend(detail_box(
            &lines,
            &key,
            self.nt.name == "bash",
            cx,
            &detail_deco,
            Style::new().fg(if self.nt.is_error { t.error } else { t.muted }),
        ));
        let mut out = RenderWindow::new(range);
        out.extend(rendered);
        out
    }
}
