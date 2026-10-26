#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::message::trim_reasoning_summary;
use super::native::{native_body, native_preview_range, summarize_tool_args};
use crate::tui::NativeTool;

#[test]
fn running_job_line_renders_with_dashed_bar() {
    use super::{Component, Cx, RunningJobLine};
    let a = crate::tui::App::new(
        "m".to_string(),
        lofi_types::ThinkingLevel::Off,
        lofi_types::ServiceTier::Auto,
        200_000,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let job = lofi_core::JobInfo {
        id: 42,
        cmd: "cargo test --workspace".to_string(),
        state: "running".to_string(),
        running: true,
        pid: Some(1234),
        exit_code: None,
        duration_ms: 12_000,
        log_path: String::new(),
        tty: false,
        notify: true,
    };
    let lines = (RunningJobLine { job: &job }).lines(&cx);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].line.spans[0].content, "┆ ");
    assert_eq!(lines[0].line.spans[0].style.fg, Some(a.theme.info));
    let rendered: String = lines[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(rendered, "┆ job 42 running (12s): cargo test --workspace");
}

fn running_job(id: u64, cmd: &str) -> lofi_core::JobInfo {
    lofi_core::JobInfo {
        id,
        cmd: cmd.to_string(),
        state: "running".to_string(),
        running: true,
        pid: None,
        exit_code: None,
        duration_ms: 1_000,
        log_path: String::new(),
        tty: false,
        notify: true,
    }
}

fn tail_rows(
    a: &crate::tui::App,
    jobs: &[lofi_core::JobInfo],
    subagents: &[lofi_core::SubagentRunning],
) -> Vec<String> {
    let cx = super::Cx {
        app: a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    super::render_transcript_tail(&cx, false, jobs, subagents)
        .iter()
        .map(|rl| rl.line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn transcript_tail_renders_queued_prompts_above_running_jobs() {
    let mut a = crate::tui::App::new(
        "m".to_string(),
        lofi_types::ThinkingLevel::Off,
        lofi_types::ServiceTier::Auto,
        200_000,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    a.prompt_queue.push(crate::tui::QueuedPrompt {
        text: "queued follow-up".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.prompt_queue.push(crate::tui::QueuedPrompt {
        text: "job 7 completed: sleep 1".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    let rows = tail_rows(&a, &[running_job(9, "sleep 9")], &[]);
    let user_idx = rows
        .iter()
        .position(|r| r.contains("queued follow-up"))
        .expect("queued user row");
    let notice_idx = rows
        .iter()
        .position(|r| r.contains("job 7 completed"))
        .expect("queued notice row");
    let job_idx = rows
        .iter()
        .position(|r| r.contains("job 9 running"))
        .expect("running job row");
    assert!(
        user_idx < notice_idx && notice_idx < job_idx,
        "expected queued user ({user_idx}) < queued notice ({notice_idx}) < running job ({job_idx}): {rows:#?}"
    );
}

#[test]
fn job_completion_swaps_running_line_for_its_notice_without_height_change() {
    let mut a = crate::tui::App::new(
        "m".to_string(),
        lofi_types::ThinkingLevel::Off,
        lofi_types::ServiceTier::Auto,
        200_000,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    let before = tail_rows(
        &a,
        &[running_job(1, "sleep 1"), running_job(2, "sleep 30")],
        &[],
    );
    a.prompt_queue.push(crate::tui::QueuedPrompt {
        text: "job 1 completed: sleep 1 (exit 0) — log: /tmp/lofi-job-1.log".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    let after = tail_rows(&a, &[running_job(2, "sleep 30")], &[]);
    assert_eq!(
        before.len(),
        after.len(),
        "before: {before:#?} after: {after:#?}"
    );
    assert!(before.iter().any(|r| r.contains("job 1 running")));
    assert!(!after.iter().any(|r| r.contains("job 1 running")));
    assert!(after.iter().any(|r| r.contains("job 1 completed")));
    assert!(after.iter().any(|r| r.contains("job 2 running")));
}

#[test]
fn transcript_tail_renders_running_subagents_above_running_jobs() {
    let a = crate::tui::App::new(
        "m".to_string(),
        lofi_types::ThinkingLevel::Off,
        lofi_types::ServiceTier::Auto,
        200_000,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    let runs = [lofi_core::SubagentRunning {
        id: "ab12cd34".to_string(),
        elapsed_ms: 65_000,
        task: "survey the repo".to_string(),
    }];
    let rows = tail_rows(&a, &[running_job(9, "sleep 9")], &runs);
    let subagent_idx = rows
        .iter()
        .position(|r| r.contains("subagent ab12cd34 running (65s): survey the repo"))
        .expect("running subagent row");
    let job_idx = rows
        .iter()
        .position(|r| r.contains("job 9 running"))
        .expect("running job row");
    assert!(
        subagent_idx < job_idx,
        "expected subagent ({subagent_idx}) above job ({job_idx}): {rows:#?}"
    );
}

#[test]
fn bash_preview_keeps_tail_while_other_tools_keep_head() {
    assert_eq!(native_preview_range("bash", 10), 7..10);
    assert_eq!(native_preview_range("read", 10), 0..3);
    assert_eq!(native_preview_range("bash", 2), 0..2);
    assert_eq!(native_preview_range("jobRead", 10), 7..10);
    assert_eq!(native_preview_range("jobScreen", 10), 7..10);
    assert_eq!(native_preview_range("jobStatus", 10), 0..3);
}

#[test]
fn bash_error_body_renders_only_structured_output() {
    let raw = serde_json::json!({
        "ok": false,
        "output": "command not found\n",
        "code": 127
    })
    .to_string();
    let tool = NativeTool {
        id: 1,
        name: "bash".to_string(),
        args: String::new(),
        result: Some(raw.clone()),
        preview: None,
        is_error: true,
        done: true,
    };
    assert_eq!(native_body(&tool).lines, vec!["command not found"]);
    assert_eq!(tool.result.as_deref(), Some(raw.as_str()));
}

#[test]
fn error_results_render_the_message_not_the_envelope() {
    let raw = serde_json::json!({"ok": false, "error": "no doc entry named 'nope'"}).to_string();
    let tool = NativeTool {
        id: 1,
        name: "docs".to_string(),
        args: "nope".to_string(),
        result: Some(raw),
        preview: None,
        is_error: true,
        done: true,
    };
    assert_eq!(native_body(&tool).lines, vec!["no doc entry named 'nope'"]);
}

#[test]
fn subagent_body_renders_the_outcome_result_and_spend() {
    let raw = serde_json::json!({
        "ok": true,
        "id": "s1",
        "status": "completed",
        "result": "done\nsecond line",
        "usage": {"input_tokens": 10, "output_tokens": 5, "cost": 0.02}
    })
    .to_string();
    let tool = NativeTool {
        id: 1,
        name: "subagent".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    let body = native_body(&tool);
    assert_eq!(body.lines, vec!["done", "second line"]);
    assert_eq!(body.notice.as_deref(), Some("(completed, $0.0200)"));
}

#[test]
fn subagent_body_renders_the_outcome_error_and_status() {
    let raw = serde_json::json!({
        "ok": false,
        "id": "s1",
        "status": "failed",
        "error": "provider returned 500"
    })
    .to_string();
    let tool = NativeTool {
        id: 1,
        name: "subagentWait".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    let body = native_body(&tool);
    assert_eq!(body.lines, vec!["provider returned 500"]);
    assert_eq!(body.notice.as_deref(), Some("(failed)"));
}

#[test]
fn cached_preview_keeps_the_encoded_field_notice() {
    let raw =
        serde_json::json!({"ok": true, "name": "lofi-skills", "content": "# Skills"}).to_string();
    let mut tool = NativeTool {
        id: 1,
        name: "skill".to_string(),
        args: "lofi-skills".to_string(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    crate::tui::view::blocks::native::cache_native_preview(&mut tool);
    let preview = tool.preview.expect("preview cached");
    assert_eq!(preview.lines, vec!["# Skills"]);
    assert_eq!(preview.notice.as_deref(), Some("(lofi-skills)"));
}

#[test]
fn cached_preview_header_reports_the_subagent_status_and_cost() {
    let raw = serde_json::json!({
        "ok": true,
        "id": "s1",
        "status": "completed",
        "result": "done",
        "usage": {"input_tokens": 10, "output_tokens": 5, "cost": 5}
    })
    .to_string();
    let mut tool = NativeTool {
        id: 1,
        name: "subagent".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    crate::tui::view::blocks::native::cache_native_preview(&mut tool);
    let preview = tool.preview.expect("preview cached");
    assert_eq!(
        preview.header_suffix.as_deref(),
        Some("(completed, $5.0000)")
    );
}

/// Every tool the sandbox binds must render a labelled body. A tool that
/// falls through to the raw-JSON arm starts its body with `{`, so the table
/// fails loudly instead of the TUI printing a JSON blob.
#[test]
fn every_tool_renders_a_body_without_raw_json() {
    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "read",
            serde_json::json!({"content": "one", "start_line": 1, "total_lines": 1}),
        ),
        ("ls", serde_json::json!({"entries": ["src/main.rs"]})),
        ("find", serde_json::json!({"matches": ["src/main.rs"]})),
        (
            "grep",
            serde_json::json!({"matches": [{"file": "src/main.rs", "line": 4, "content": "fn main"}]}),
        ),
        (
            "write",
            serde_json::json!({"ok": true, "path": "a.txt", "content": "one"}),
        ),
        (
            "edit",
            serde_json::json!({"ok": true, "old": "one", "new": "two"}),
        ),
        (
            "patch",
            serde_json::json!({"ok": true, "path": "a.txt", "hunks": 2}),
        ),
        (
            "bash",
            serde_json::json!({"ok": true, "output": "one", "duration_ms": 5}),
        ),
        (
            "jobList",
            serde_json::json!({"ok": true, "jobs": [{"id": "7", "state": "running", "command": "make test"}]}),
        ),
        (
            "jobSpawn",
            serde_json::json!({"ok": true, "id": "1", "state": "running"}),
        ),
        (
            "jobStatus",
            serde_json::json!({"ok": true, "id": "1", "state": "completed"}),
        ),
        (
            "jobRead",
            serde_json::json!({"ok": true, "id": "1", "output": "one", "cursor": 3, "totalBytes": 3, "done": true}),
        ),
        (
            "jobScreen",
            serde_json::json!({"ok": true, "id": "1", "cols": 80, "rows": 24, "lines": ["$ ls"]}),
        ),
        (
            "jobType",
            serde_json::json!({"ok": true, "id": "1", "sent": 3}),
        ),
        (
            "jobKeyPress",
            serde_json::json!({"ok": true, "id": "1", "key": "Enter"}),
        ),
        (
            "jobKill",
            serde_json::json!({"ok": true, "id": "1", "state": "killed"}),
        ),
        ("jobNotify", serde_json::json!({"ok": true, "id": "1"})),
        (
            "skills",
            serde_json::json!({"ok": true, "skills": [{"name": "lofi-skills", "description": "how to"}]}),
        ),
        (
            "skill",
            serde_json::json!({"ok": true, "name": "lofi-skills", "content": "# Skills"}),
        ),
        (
            "docs",
            serde_json::json!({"ok": true, "entries": [{"name": "lofi.read", "summary": "Read a file"}]}),
        ),
        (
            "docsSearch",
            serde_json::json!({"ok": true, "results": [{"name": "lofi.read", "excerpt": "Read a file"}]}),
        ),
        (
            "models",
            serde_json::json!({"models": [{"model": "plexus/glm-5.3", "name": "GLM"}]}),
        ),
        (
            "recall",
            serde_json::json!({"text": "earlier", "status": "ok"}),
        ),
        ("result", serde_json::json!("paged result")),
        (
            "subagent",
            serde_json::json!({"ok": true, "id": "s1", "status": "completed", "result": "done", "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_tokens": 0, "cache_write_tokens": 0, "cost": 0.02}}),
        ),
        (
            "subagentWait",
            serde_json::json!({"ok": false, "id": "s1", "status": "failed", "error": "provider returned 500"}),
        ),
        (
            "subagentSpawn",
            serde_json::json!({"ok": true, "id": "s1", "status": "running"}),
        ),
        (
            "subagentStatus",
            serde_json::json!({"ok": true, "id": "s1", "status": "done"}),
        ),
        (
            "subagentList",
            serde_json::json!({"ok": true, "entries": [{"id": "s1", "status": "done"}]}),
        ),
        (
            "subagentRead",
            serde_json::json!({"ok": true, "events": [{"seq": 0, "type": "state", "status": "done"}, {"seq": 1, "type": "text", "text": "hello"}], "cursor": 2, "done": true}),
        ),
        (
            "subagentKill",
            serde_json::json!({"ok": true, "id": "s1", "status": "cancelling"}),
        ),
        (
            "subagentNotify",
            serde_json::json!({"ok": true, "id": "s1"}),
        ),
        (
            "subagentMailboxSend",
            serde_json::json!({"ok": true, "seq": 1, "to": "parent"}),
        ),
        (
            "subagentMailboxSend",
            serde_json::json!({"ok": true, "seq": 3, "duplicate": true}),
        ),
        (
            "subagentMailboxReceive",
            serde_json::json!({"ok": true, "messages": [{"from": "s1", "body": "hello"}]}),
        ),
        (
            "subagentMailboxList",
            serde_json::json!({"ok": true, "messages": [], "inboxes": [{"agent_id": "s1", "pending": 2}]}),
        ),
    ];
    for (name, result) in cases {
        let tool = NativeTool {
            id: 1,
            name: name.to_string(),
            args: String::new(),
            result: Some(result.to_string()),
            preview: None,
            is_error: false,
            done: true,
        };
        let body = native_body(&tool);
        assert!(!body.lines.is_empty(), "{name} renders no body lines");
        let first = body.lines[0].trim_start();
        assert!(
            !first.starts_with('{'),
            "{name} falls back to raw JSON: {first:?}"
        );
    }
}

#[test]
fn summarize_tool_args_shortens_job_envelopes() {
    assert_eq!(
        summarize_tool_args("jobStatus", "{\"id\":\"1786294694788353138\"}").as_deref(),
        Some("job 1786294694788353138")
    );
    assert_eq!(
        summarize_tool_args("jobSpawn", "{\"cmd\":\"sleep 1\"}").as_deref(),
        Some("sleep 1")
    );
    assert_eq!(
        summarize_tool_args(
            "jobSpawn",
            "{\"cmd\":\"a very long command line that exceeds the 60-character display budget\"}"
        )
        .as_deref(),
        Some("a very long command line that exceeds the 60-character disp…")
    );
    assert_eq!(
        summarize_tool_args("read", "{\"path\":\"/tmp/x\"}"),
        None,
        "read is not a job tool — keep verbatim"
    );
    assert_eq!(
        summarize_tool_args("subagentMailboxSend", "{\"to\":\"s1\",\"body\":\"hi\"}").as_deref(),
        Some("to s1")
    );
    assert_eq!(
        summarize_tool_args("jobStatus", "{\"id\":5}").as_deref(),
        Some("job 5"),
        "parse_id also accepts a numeric id"
    );
    assert_eq!(
        summarize_tool_args("subagentMailboxReceive", "{\"limit\":5}"),
        None,
        "mailbox receive takes no id or recipient to summarize"
    );
}

#[test]
fn job_status_body_renders_compact_summary() {
    let raw = serde_json::json!({
        "id": "1786294694788353138",
        "state": "completed",
        "exitCode": 0,
        "signal": null,
        "durationMs": 30089,
        "logPath": "/tmp/lofi-job.log",
        "notify": true,
        "notifyChanged": true,
        "notifyIntervalMs": 5000,
        "ok": true,
        "command": "for i in 1 2; do echo $i; done",
        "directory": "/tmp",
        "timeoutMs": 30000
    })
    .to_string();
    let tool = NativeTool {
        id: 1,
        name: "jobStatus".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    let body = native_body(&tool);
    assert_eq!(body.lines[0], "state: completed");
    assert!(
        body.lines[1].contains("exit 0"),
        "exit code line: {:?}",
        body.lines[1]
    );
    assert!(
        body.lines[1].contains("duration"),
        "duration line: {:?}",
        body.lines[1]
    );
    let notify_line = body
        .lines
        .iter()
        .find(|l| l.starts_with("notify:"))
        .expect("notify line present");
    assert!(notify_line.contains("notify: true"), "{notify_line}");
    assert!(notify_line.contains("interval"), "{notify_line}");
    assert!(
        body.lines.last().is_some_and(|l| l.starts_with("log: ")),
        "log path last: {:?}",
        body.lines
    );
}

#[test]
fn job_notify_body_uses_the_job_notify_field_names() {
    let raw = serde_json::json!({
        "ok": true,
        "id": "1",
        "notify": true,
        "intervalMs": 5000,
        "changed": false,
        "idleMs": null
    })
    .to_string();
    let tool = NativeTool {
        id: 1,
        name: "jobNotify".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    let body = native_body(&tool);
    let notify_line = body
        .lines
        .iter()
        .find(|l| l.starts_with("notify:"))
        .expect("notify line present");
    assert!(notify_line.contains("interval"), "{notify_line}");
    assert!(notify_line.contains("changed false"), "{notify_line}");
}

#[test]
fn job_read_body_renders_output_with_tail_notice() {
    let raw = serde_json::json!({
        "id": "1",
        "state": "completed",
        "output": "beat-1\nbeat-2\n",
        "cursor": 14,
        "totalBytes": 71,
        "done": false,
        "ok": true
    })
    .to_string();
    let tool = NativeTool {
        id: 1,
        name: "jobRead".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    let body = native_body(&tool);
    assert_eq!(body.lines, vec!["beat-1", "beat-2"]);
    assert_eq!(body.notice.as_deref(), Some("(read 14 of 71 bytes)"));
}

#[test]
fn job_read_body_marks_done_at_end() {
    let raw = serde_json::json!({
        "id": "1",
        "output": "done\n",
        "cursor": 71,
        "totalBytes": 71,
        "done": true,
        "ok": true
    })
    .to_string();
    let tool = NativeTool {
        id: 1,
        name: "jobRead".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    let body = native_body(&tool);
    assert_eq!(body.notice.as_deref(), Some("(end of log; 71 bytes total)"));
}

#[test]
fn job_list_body_renders_string_ids_and_commands() {
    let raw = serde_json::json!({
        "ok": true,
        "jobs": [{"id": "7", "state": "running", "command": "make test"}]
    })
    .to_string();
    let tool = NativeTool {
        id: 1,
        name: "jobList".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    let body = native_body(&tool);
    assert_eq!(body.lines, vec!["job 7 running: make test"]);
    assert_eq!(body.notice, None);
}

#[test]
fn job_list_body_reports_an_empty_list() {
    let raw = serde_json::json!({"ok": true, "jobs": []}).to_string();
    let tool = NativeTool {
        id: 1,
        name: "jobList".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    let body = native_body(&tool);
    assert_eq!(body.notice.as_deref(), Some("(no jobs)"));
}

#[test]
fn subagent_read_body_renders_event_payload_text() {
    let raw = serde_json::json!({
        "ok": true,
        "events": [
            {"seq": 0, "type": "state", "status": "done"},
            {"seq": 1, "type": "text", "text": "hello\nworld"},
            {"seq": 2, "type": "tool_start", "name": "read"}
        ],
        "cursor": 3,
        "done": true
    })
    .to_string();
    let tool = NativeTool {
        id: 1,
        name: "subagentRead".to_string(),
        args: String::new(),
        result: Some(raw),
        preview: None,
        is_error: false,
        done: true,
    };
    let body = native_body(&tool);
    assert_eq!(
        body.lines,
        vec!["#0 state: done", "#1 text: hello", "#2 tool_start: read"]
    );
}

#[test]
fn reasoning_summary_trims_empty_placeholder_parts() {
    let text = "**Checking**\n<!-- -->\n\nActual <!-- --> content.\n\n**Done**\nResult";
    assert_eq!(
        trim_reasoning_summary(text),
        "Actual <!-- --> content.\n\n**Done**\nResult"
    );
}

#[test]
fn reasoning_summary_trims_plain_empty_placeholder() {
    assert_eq!(trim_reasoning_summary(" <!-- --> "), "");
}
