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

fn tail_rows(a: &crate::tui::App, jobs: &[lofi_core::JobInfo]) -> Vec<String> {
    let cx = super::Cx {
        app: a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    super::render_transcript_tail(&cx, false, jobs)
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
    let rows = tail_rows(&a, &[running_job(9, "sleep 9")]);
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
    let before = tail_rows(&a, &[running_job(1, "sleep 1"), running_job(2, "sleep 30")]);
    a.prompt_queue.push(crate::tui::QueuedPrompt {
        text: "job 1 completed: sleep 1 (exit 0) — log: /tmp/lofi-job-1.log".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    let after = tail_rows(&a, &[running_job(2, "sleep 30")]);
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
fn bash_preview_keeps_tail_while_other_tools_keep_head() {
    assert_eq!(native_preview_range("bash", 10), 7..10);
    assert_eq!(native_preview_range("read", 10), 0..3);
    assert_eq!(native_preview_range("bash", 2), 0..2);
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
