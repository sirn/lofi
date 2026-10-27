//! Registry semantics and the four sandbox bindings.

#![allow(clippy::unwrap_used)]

use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use super::bindings::MAX_TIMERS_PER_OWNER;
use super::*;
use crate::tools::BuiltinTools;

fn tools() -> (tempfile::TempDir, BuiltinTools) {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().join("lofi-tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let tools = BuiltinTools::with_skills_dir(
        dir.path().to_path_buf(),
        None,
        tmp,
        crate::BashEnv::default(),
        crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig::default()),
        None,
        None,
        None,
    );
    (dir, tools)
}

fn past() -> SystemTime {
    SystemTime::now() - Duration::from_secs(1)
}

#[test]
fn notify_at_fires_once_with_message() {
    let registry = TimerRegistry::new();
    registry.schedule_at(None, past(), Some("check the build".into()));
    assert_eq!(
        registry.take_notices(),
        vec!["notifyAt fired: check the build"]
    );
    assert!(registry.take_notices().is_empty(), "a one-shot fires once");
}

#[test]
fn notify_at_without_message_uses_the_default_text() {
    let registry = TimerRegistry::new();
    registry.schedule_at(None, past(), None);
    assert_eq!(registry.take_notices(), vec!["notifyAt fired"]);
}

#[test]
fn notices_leave_oldest_first() {
    let registry = TimerRegistry::new();
    let now = SystemTime::now();
    registry.schedule_at(None, now - Duration::from_secs(3), Some("one".into()));
    registry.schedule_at(None, now - Duration::from_secs(1), Some("two".into()));
    assert_eq!(
        registry.take_notices(),
        vec!["notifyAt fired: one", "notifyAt fired: two"]
    );
}

#[test]
fn notify_every_rearms_and_coalesces_missed_intervals() {
    let registry = TimerRegistry::new();
    registry.schedule_every(None, Duration::from_millis(20), Some("tick".into()));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(registry.take_notices(), vec!["notifyEvery fired: tick"]);
    // Five intervals late: one notice, not five.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(registry.take_notices(), vec!["notifyEvery fired: tick"]);
}

#[test]
fn cancel_owner_cancels_both_kinds_for_one_owner() {
    let registry = TimerRegistry::new();
    registry.schedule_at(Some("sub"), past(), None);
    registry.schedule_every(Some("sub"), Duration::from_mins(1), None);
    registry.schedule_at(Some("other"), past(), None);
    registry.schedule_at(None, past(), None);
    assert_eq!(registry.pending_for(Some("sub")), 2);
    assert_eq!(registry.cancel_owner(Some("sub")), 2);
    assert_eq!(registry.pending_for(Some("sub")), 0);
    // The other owner's timer and the session's own survive.
    assert_eq!(registry.take_notices().len(), 2);
}

#[test]
fn reset_clears_every_owner() {
    let registry = TimerRegistry::new();
    registry.schedule_at(Some("sub"), past(), None);
    registry.schedule_at(None, past(), None);
    registry.reset();
    assert!(registry.take_notices().is_empty());
    assert_eq!(registry.cancel_owner(None), 0);
}

#[tokio::test]
async fn sleep_blocks_for_the_requested_time() {
    let (_dir, tools) = tools();
    let started = std::time::Instant::now();
    let res = tools.sleep(json!(0.05)).await.unwrap();
    assert_eq!(res["ok"], json!(true));
    assert!(
        started.elapsed() >= Duration::from_millis(40),
        "slept too little"
    );
    assert!(tools.sleep(json!(0)).await.is_ok(), "a zero sleep resolves");
}

#[tokio::test]
async fn sleep_validates_seconds() {
    let (_dir, tools) = tools();
    let err = tools.sleep(json!("5s")).await.unwrap_err();
    assert!(err.to_string().contains("must be a number"), "got: {err}");
    let err = tools.sleep(json!(-1)).await.unwrap_err();
    assert!(err.to_string().contains("non-negative"), "got: {err}");
    let err = tools.sleep(json!(1e9)).await.unwrap_err();
    assert!(err.to_string().contains("at most 86400"), "got: {err}");
}

#[tokio::test]
async fn sleep_stops_on_run_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().join("lofi-tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let tools = BuiltinTools::with_skills_dir(
        dir.path().to_path_buf(),
        None,
        tmp,
        crate::BashEnv::default(),
        crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig::default()),
        None,
        None,
        None,
    )
    .with_cancel(Some(std::sync::Arc::new(AtomicBool::new(true))));
    let started = std::time::Instant::now();
    let err = tools.sleep(json!(30)).await.unwrap_err();
    assert!(err.to_string().contains("run cancelled"), "got: {err}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancel did not stop the wait"
    );
}

#[tokio::test]
async fn notify_at_schedules_and_fires() {
    let (_dir, tools) = tools();
    let res = tools
        .notify_at(json!("2020-01-01T00:00:00Z"), Some(json!("hi")))
        .await
        .unwrap();
    assert_eq!(res["ok"], json!(true));
    assert_eq!(res["message"], json!("hi"));
    assert_eq!(tools.timers.take_notices(), vec!["notifyAt fired: hi"]);
}

#[tokio::test]
async fn notify_at_accepts_an_offset_without_a_colon() {
    let (_dir, tools) = tools();
    let res = tools
        .notify_at(json!("2020-01-01T05:00:00+0900"), None)
        .await
        .unwrap();
    assert_eq!(res["ok"], json!(true));
}

#[tokio::test]
async fn notify_at_rejects_bad_input() {
    let (_dir, tools) = tools();
    let err = tools.notify_at(json!("tomorrow"), None).await.unwrap_err();
    assert!(err.to_string().contains("RFC 3339"), "got: {err}");
    let err = tools
        .notify_at(json!("2020-01-01T00:00:00Z"), Some(json!(7)))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("'message' must be a string"),
        "got: {err}"
    );
}

#[tokio::test]
async fn notify_every_validates_the_interval() {
    let (_dir, tools) = tools();
    let err = tools.notify_every(json!("5"), None).await.unwrap_err();
    assert!(err.to_string().contains("duration like"), "got: {err}");
    let err = tools.notify_every(json!("500ms"), None).await.unwrap_err();
    assert!(err.to_string().contains("at least 1s"), "got: {err}");
    let err = tools.notify_every(json!("5x"), None).await.unwrap_err();
    assert!(err.to_string().contains("duration like"), "got: {err}");
}

#[tokio::test]
async fn notify_cancel_cancels_both_kinds() {
    let (_dir, tools) = tools();
    tools
        .notify_at(json!("2030-01-01T00:00:00Z"), None)
        .await
        .unwrap();
    tools
        .notify_every(json!("5m"), Some(json!("tick")))
        .await
        .unwrap();
    let res = tools.notify_cancel(Value::Null).await.unwrap();
    assert_eq!(res["ok"], json!(true));
    assert_eq!(res["cancelled"], json!(2));
    assert!(tools.timers.take_notices().is_empty());
}

#[tokio::test]
async fn owner_cap_bounds_subagent_timers_only() {
    let (_dir, root) = tools();
    let (_sub_dir, sub) = tools();
    let sub = sub.with_job_owner(Some("sub".to_string()));
    for _ in 0..MAX_TIMERS_PER_OWNER {
        let res = sub
            .notify_at(json!("2030-01-01T00:00:00Z"), None)
            .await
            .unwrap();
        assert_eq!(res["ok"], json!(true));
    }
    let res = sub
        .notify_at(json!("2030-01-01T00:00:00Z"), None)
        .await
        .unwrap();
    assert_eq!(res["ok"], json!(false));
    assert!(res["error"].as_str().unwrap().contains("cap reached"));
    // The session's own timers are not capped.
    for _ in 0..=MAX_TIMERS_PER_OWNER {
        let res = root
            .notify_at(json!("2030-01-01T00:00:00Z"), None)
            .await
            .unwrap();
        assert_eq!(res["ok"], json!(true));
    }
}
