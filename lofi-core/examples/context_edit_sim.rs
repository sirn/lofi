#!/usr/bin/env -S -e
#![allow(clippy::expect_used, clippy::print_stdout, clippy::unwrap_used)]
//! Context-editing compression simulator.
//!
//! Reads a pi/lofi session JSONL and measures how much of the *message
//! content* survives under three retention policies, applied to a trailing
//! window of messages (the "kept tail" a compaction would carry verbatim):
//!
//!   verbatim   — keep everything (VCC/lofi today: the tail is carried as-is).
//!   edited     — continuous context-editing pass:
//!                  * evict thinking blocks except the last `keep_thinking`.
//!                  * elide tool-result text except the last `keep_results`,
//!                    replacing each with a one-line placeholder stub.
//!                  * trim tool-call arguments to their `display` label except
//!                    the last `keep_calls` (the model's intent survives; the
//!                    verbatim code does not).
//!   compacted  — the edited policy, plus everything older than the window is
//!                assumed folded into a summary (not measured here; this is
//!                just the tail cost).
//!
//! The point: `edited` is the no-LLM, per-turn, recall-recoverable floor.
//! Because lofi.recall can re-expand elided tool results on demand, elision is
//! non-destructive — strictly better than Anthropic's `clear_tool_uses` /
//! opencode's prune / Claude Code's microcompact, which discard outright.

use std::env;
use std::io::{BufRead, BufReader};

use serde_json::Value;

fn content_bytes(msg: &Value) -> usize {
    // Approximate "what the provider charges": the serialized text of every
    // content block. Good enough for ratio comparison.
    let content = &msg["message"]["content"];
    match content {
        Value::Array(a) => a.iter().map(block_bytes).sum(),
        other => other.to_string().len(),
    }
}

fn block_bytes(b: &Value) -> usize {
    match b["type"].as_str() {
        Some("thinking") => b["thinking"].as_str().map(str::len).unwrap_or(0),
        Some("toolCall") => b["arguments"].to_string().len(),
        Some("text") => b["text"].as_str().map(str::len).unwrap_or(0),
        _ => b.to_string().len(),
    }
}

/// Bytes a block retains under the `edited` policy. `keep_recent` is whether
/// this block is within the protected recent window for its category.
fn edited_block_bytes(b: &Value, keep_recent: bool) -> usize {
    match b["type"].as_str() {
        Some("thinking") if keep_recent => block_bytes(b),
        Some("thinking") => 0, // evicted
        Some("text") if keep_recent => block_bytes(b),
        Some("text") => stub_for(b), // elided tool result (or non-recent text)
        Some("toolCall") if keep_recent => block_bytes(b),
        Some("toolCall") => display_label_bytes(b), // intent only
        _ => block_bytes(b),
    }
}

/// Placeholder for a cleared tool result. Mirrors Anthropic's
/// `clear_tool_uses` placeholder, plus a recall hint.
fn stub_for(_b: &Value) -> usize {
    "[tool result cleared — /recall or lofi.recall to re-expand]".len()
}

/// Bytes of just the `display` label on a tool call (name + description),
/// i.e. the model's intent without the verbatim code.
fn display_label_bytes(b: &Value) -> usize {
    let d = &b["arguments"]["display"];
    if d.is_null() {
        return "[tool call cleared — /recall to re-expand]".len();
    }
    let name = d.get("name").and_then(Value::as_str).unwrap_or("");
    let desc = d.get("description").and_then(Value::as_str).unwrap_or("");
    name.len() + desc.len() + 4
}

fn main() {
    let path = env::args().nth(1).expect("usage: context_edit_sim <session.jsonl>");
    let f = std::fs::File::open(&path).expect("open");
    let msgs: Vec<Value> = BufReader::new(f)
        .lines()
        .filter_map(|l| l.ok())
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .filter(|v| v["type"].as_str() == Some("message"))
        .collect();

    // Category totals across the whole session, for context.
    let mut tot = [0usize; 4]; // [assistantText, toolOutput, thinking, toolCall]
    for m in &msgs {
        let role = m["message"]["role"].as_str().unwrap_or("");
        if let Some(a) = m["message"]["content"].as_array() {
            for b in a {
                match b["type"].as_str() {
                    Some("thinking") => tot[2] += block_bytes(b),
                    Some("toolCall") => tot[3] += block_bytes(b),
                    Some("text") if role == "toolResult" => tot[1] += block_bytes(b),
                    Some("text") => tot[0] += block_bytes(b),
                    _ => {}
                }
            }
        }
    }
    let all: usize = tot.iter().sum();
    println!("session messages: {}", msgs.len());
    println!("whole-session content bytes: {}", all);
    println!(
        "  assistantText {:>8} ({:.1}%)  toolOutput {:>8} ({:.1}%)  thinking {:>8} ({:.1}%)  toolCall {:>8} ({:.1}%)",
        tot[0], 100.0 * tot[0] as f64 / all as f64,
        tot[1], 100.0 * tot[1] as f64 / all as f64,
        tot[2], 100.0 * tot[2] as f64 / all as f64,
        tot[3], 100.0 * tot[3] as f64 / all as f64,
    );

    // Simulate the kept tail at several window sizes. For each window we walk
    // the last `window` messages and compute verbatim vs. edited bytes.
    for &window in &[50usize, 100, 200, 400] {
        let start = msgs.len().saturating_sub(window);
        let tail = &msgs[start..];
        let mut verbatim = 0usize;
        let mut edited = 0usize;
        // Count recent blocks per category from the end of the tail.
        let keep_results = 6;
        let keep_thinking = 2;
        let keep_calls = 6;
        let mut seen_results = 0u32;
        let mut seen_thinking = 0u32;
        let mut seen_calls = 0u32;
        // Walk tail in reverse to mark the protected recent window per block.
        let mut keep_recent_results = vec![false; tail.len()];
        let mut keep_recent_thinking = vec![false; tail.len()];
        let mut keep_recent_calls = vec![false; tail.len()];
        for (i, m) in tail.iter().enumerate().rev() {
            let role = m["message"]["role"].as_str().unwrap_or("");
            if let Some(a) = m["message"]["content"].as_array() {
                for b in a {
                    match b["type"].as_str() {
                        Some("text") if role == "toolResult" => {
                            if seen_results < keep_results {
                                keep_recent_results[i] = true;
                            }
                            seen_results += 1;
                        }
                        Some("thinking") => {
                            if seen_thinking < keep_thinking {
                                keep_recent_thinking[i] = true;
                            }
                            seen_thinking += 1;
                        }
                        Some("toolCall") => {
                            if seen_calls < keep_calls {
                                keep_recent_calls[i] = true;
                            }
                            seen_calls += 1;
                        }
                        _ => {}
                    }
                }
            }
        }
        for (i, m) in tail.iter().enumerate() {
            let role = m["message"]["role"].as_str().unwrap_or("");
            verbatim += content_bytes(m);
            if let Some(a) = m["message"]["content"].as_array() {
                for b in a {
                    let kr = match b["type"].as_str() {
                        Some("text") if role == "toolResult" => keep_recent_results[i],
                        Some("thinking") => keep_recent_thinking[i],
                        Some("toolCall") => keep_recent_calls[i],
                        _ => true,
                    };
                    edited += edited_block_bytes(b, kr);
                }
            } else {
                edited += content_bytes(m);
            }
        }
        let ratio = 100.0 * edited as f64 / verbatim.max(1) as f64;
        println!(
            "\ntail window = {} msgs: verbatim {} bytes -> edited {} bytes ({:.1}% retained, {:.1}% saved)",
            window, verbatim, edited, ratio, 100.0 - ratio
        );
    }
}