#![allow(clippy::unwrap_used)]
use super::driver::{evict_terminal_excess, progress_tick, push_notice};
use super::pty::{idle_tail, key_press_bytes, read_log_tail};
use super::*;
use crate::tools::BuiltinTools;
use std::sync::atomic::AtomicBool;

fn handle_with(notify: NotifyOpts, log_path: String) -> Arc<JobHandle> {
    Arc::new(JobHandle {
        data: Mutex::new(Job {
            id: 1,
            cmd: "cmd".to_string(),
            state: State::Running,
            pid: None,
            started: Instant::now(),
            ended: None,
            exit_code: None,
            signal: None,
            log_path,
            timeout_ms: None,
            notify,
            tty: false,
            cols: DEFAULT_TTY_COLS,
            rows: DEFAULT_TTY_ROWS,
            idle_ms: None,
            idle: false,
            last_output_at: None,
        }),
        cancel: AtomicBool::new(false),
        on_release: Mutex::new(None),
        master: Mutex::new(None),
        screen: Mutex::new(None),
        application_cursor: AtomicBool::new(false),
    })
}

fn periodic(changed: bool) -> NotifyOpts {
    NotifyOpts {
        enabled: true,
        interval_ms: Some(1),
        changed,
    }
}

async fn wait_for_terminal(tools: &BuiltinTools, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = tools.job_status(json!({ "id": id })).await.unwrap();
        if status["state"] != json!("running") {
            return status;
        }
        assert!(Instant::now() < deadline, "job {id} did not finish");
        tokio::time::sleep(JOB_POLL_INTERVAL).await;
    }
}

async fn wait_for_log(tools: &BuiltinTools, id: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = tools.job_read(json!({ "id": id })).await.unwrap();
        if result["output"]
            .as_str()
            .is_some_and(|output| output.contains(needle))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} did not output {needle}"
        );
        tokio::time::sleep(JOB_POLL_INTERVAL).await;
    }
}

#[test]
fn tick_skips_when_log_unchanged() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let p = f.path().to_string_lossy().into_owned();
    let h = handle_with(periodic(true), p);
    let mut tick = Instant::now()
        .checked_sub(Duration::from_millis(10))
        .unwrap();
    let mut bytes = 0;
    // Log empty and unchanged from last_bytes=0: changed suppresses it.
    assert!(progress_tick(&h, &mut tick, &mut bytes).is_none());
}

#[test]
fn tick_fires_when_log_grew() {
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(f.path(), b"hello").unwrap();
    let p = f.path().to_string_lossy().into_owned();
    let h = handle_with(periodic(true), p);
    let mut tick = Instant::now()
        .checked_sub(Duration::from_millis(10))
        .unwrap();
    let mut bytes = 0;
    let note = progress_tick(&h, &mut tick, &mut bytes).unwrap();
    assert!(note.contains("running"));
    assert!(note.contains("5 bytes"));
}

#[test]
fn tick_fires_every_time_when_changed_false() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let p = f.path().to_string_lossy().into_owned();
    let h = handle_with(periodic(false), p);
    let mut tick = Instant::now()
        .checked_sub(Duration::from_millis(10))
        .unwrap();
    let mut bytes = 0;
    // No output, but changed=false means emit anyway.
    assert!(progress_tick(&h, &mut tick, &mut bytes).is_some());
}

#[test]
fn tick_not_due_before_interval() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let p = f.path().to_string_lossy().into_owned();
    let mut n = periodic(false);
    n.interval_ms = Some(60_000);
    let h = handle_with(n, p);
    let mut tick = Instant::now();
    let mut bytes = 0;
    assert!(progress_tick(&h, &mut tick, &mut bytes).is_none());
}

#[test]
fn tick_suppressed_when_disabled() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let p = f.path().to_string_lossy().into_owned();
    let mut n = periodic(false);
    n.enabled = false;
    let h = handle_with(n, p);
    let mut tick = Instant::now()
        .checked_sub(Duration::from_millis(10))
        .unwrap();
    let mut bytes = 0;
    assert!(progress_tick(&h, &mut tick, &mut bytes).is_none());
}

#[test]
fn tick_none_when_terminal_only() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let p = f.path().to_string_lossy().into_owned();
    let h = handle_with(NotifyOpts::terminal_only(), p);
    let mut tick = Instant::now()
        .checked_sub(Duration::from_millis(10))
        .unwrap();
    let mut bytes = 0;
    // No interval configured: periodic mode is off.
    assert!(progress_tick(&h, &mut tick, &mut bytes).is_none());
}

#[test]
fn shutdown_cancels_every_running_job() {
    let registry = JobRegistry::new();
    let first_log = tempfile::NamedTempFile::new().unwrap();
    let second_log = tempfile::NamedTempFile::new().unwrap();
    let first = handle_with(
        NotifyOpts::terminal_only(),
        first_log.path().to_string_lossy().into_owned(),
    );
    let second = handle_with(
        NotifyOpts::terminal_only(),
        second_log.path().to_string_lossy().into_owned(),
    );
    second.data.lock().unwrap().id = 2;
    {
        let mut jobs = registry.inner.jobs.lock().unwrap();
        jobs.insert(1, first.clone());
        jobs.insert(2, second.clone());
    }

    registry.shutdown();

    for handle in [first, second] {
        assert_eq!(handle.data.lock().unwrap().state, State::Cancelled);
        assert!(handle.cancel.load(Ordering::Relaxed));
    }
}

#[test]
fn reset_ends_the_old_scope_and_rejects_its_late_notices() {
    let registry = JobRegistry::new();
    let old_generation = registry.inner.generation.load(Ordering::SeqCst);
    let mut old_receiver = registry.subscribe_notices();
    let log = tempfile::NamedTempFile::new().unwrap();
    let handle = handle_with(
        NotifyOpts::terminal_only(),
        log.path().to_string_lossy().into_owned(),
    );
    let id = handle.data.lock().unwrap().id;
    registry
        .inner
        .jobs
        .lock()
        .unwrap()
        .insert(id, handle.clone());
    registry.inner.pending.lock().unwrap().push(JobNotice {
        id,
        terminal: false,
        text: "buffered".into(),
    });

    registry.reset();

    assert!(registry.live_ids().is_empty());
    assert!(registry.drain_notices().is_empty());
    assert_eq!(handle.data.lock().unwrap().state, State::Cancelled);
    assert_eq!(handle.data.lock().unwrap().signal, Some(nix::libc::SIGKILL));
    assert!(handle.cancel.load(Ordering::Relaxed));
    assert!(old_receiver.try_recv().is_err());

    let weak = Arc::downgrade(&registry.inner);
    let stale = JobNotice {
        id,
        terminal: true,
        text: "stale".into(),
    };
    push_notice(&weak, old_generation, stale);
    let current_generation = registry.inner.generation.load(Ordering::SeqCst);
    let mut current_receiver = registry.subscribe_notices();
    assert!(current_receiver.try_recv().is_err());
    let current = JobNotice {
        id,
        terminal: true,
        text: "current".into(),
    };
    push_notice(&weak, current_generation, current.clone());
    assert_eq!(current_receiver.try_recv().unwrap(), current);
}

#[test]
fn terminal_records_are_evicted_beyond_the_cap_and_logs_removed() {
    let registry = JobRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let total = MAX_TERMINAL_JOBS as u64 + 3;
    let mut log_paths = Vec::new();
    {
        let mut jobs = registry.inner.jobs.lock().unwrap();
        for i in 1..=total {
            let log = dir.path().join(format!("job-{i}.log"));
            std::fs::write(&log, b"x").unwrap();
            let log = log.to_string_lossy().into_owned();
            log_paths.push(log.clone());
            let handle = handle_with(NotifyOpts::terminal_only(), log);
            {
                let mut job = handle.data.lock().unwrap();
                job.id = i;
                job.state = State::Completed;
            }
            jobs.insert(i, handle);
        }
    }
    // A running job must survive eviction regardless of age.
    let running_log = dir.path().join("running.log");
    std::fs::write(&running_log, b"x").unwrap();
    let running = handle_with(
        NotifyOpts::terminal_only(),
        running_log.to_string_lossy().into_owned(),
    );
    running.data.lock().unwrap().id = total + 1;
    registry
        .inner
        .jobs
        .lock()
        .unwrap()
        .insert(total + 1, running);

    evict_terminal_excess(&registry.inner);

    let jobs = registry.inner.jobs.lock().unwrap();
    assert_eq!(jobs.len(), MAX_TERMINAL_JOBS + 1);
    assert!(
        jobs.contains_key(&(total + 1)),
        "running job is never evicted"
    );
    for i in 1..=3_u64 {
        assert!(!jobs.contains_key(&i), "oldest terminal job {i} evicted");
        assert!(!std::path::Path::new(&log_paths[(i - 1) as usize]).exists());
    }
    assert!(jobs.contains_key(&total));
    assert!(std::path::Path::new(&log_paths[(total - 1) as usize]).exists());
}

#[test]
fn pending_notices_are_bounded_and_summarize_overflow() {
    let mut pending = PendingNotices::new();
    let total = MAX_PENDING_NOTICES as u64 + 10;
    for i in 0..total {
        pending.push(JobNotice {
            id: i,
            terminal: false,
            text: format!("notice-{i}"),
        });
    }

    assert!(pending.queue.len() <= MAX_PENDING_NOTICES);
    let drained = pending.drain();
    let summaries: Vec<_> = drained
        .iter()
        .filter(|n| n.text.contains("dropped (queue overflow)"))
        .collect();
    assert!(!summaries.is_empty(), "overflow produced a summary notice");
    // The newest notice survives the overflow.
    assert!(drained
        .iter()
        .any(|n| n.text == format!("notice-{}", total - 1)));
}

#[test]
fn reset_removes_drained_job_logs() {
    let registry = JobRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("reset-job.log");
    std::fs::write(&log, b"x").unwrap();
    let handle = handle_with(
        NotifyOpts::terminal_only(),
        log.to_string_lossy().into_owned(),
    );
    handle.data.lock().unwrap().state = State::Completed;
    let id = handle.data.lock().unwrap().id;
    registry.inner.jobs.lock().unwrap().insert(id, handle);

    registry.reset();

    assert!(!log.exists());
}

#[test]
fn kill_not_in_disables_notify_before_killing() {
    // Regression: /tree rollback reconcile used to kill the off-lineage
    // job with notify still enabled, so the driver poll saw the
    // Cancelled state and pushed a "cancelled: <cmd>" notice that the UI
    // then delivered to the agent as a user-role message. The user
    // already opted the job out of the conversation by rolling back;
    // nothing should reach the agent.
    let registry = JobRegistry::new();
    let f = tempfile::NamedTempFile::new().unwrap();
    let handle = handle_with(
        NotifyOpts::terminal_only(),
        f.path().to_string_lossy().into_owned(),
    );
    let id = handle.data.lock().unwrap().id;
    registry
        .inner
        .jobs
        .lock()
        .unwrap()
        .insert(id, handle.clone());
    assert_eq!(registry.live_ids(), std::collections::HashSet::from([id]));

    let killed = registry.kill_not_in(&[]);
    assert_eq!(killed, vec![id]);

    // Job is gone from the registry...
    assert!(registry.live_ids().is_empty());
    // ...and notify was disabled before terminal, so the driver poll
    // (if it were running) would never push the "cancelled" notice.
    let job = handle.data.lock().unwrap();
    assert!(job.state.is_terminal());
    assert!(!job.notify.enabled);
}

fn tools_with_cancel(cancel: Arc<AtomicBool>) -> (tempfile::TempDir, BuiltinTools) {
    tools_with_env(cancel, crate::BashEnv::default())
}

fn tools_with_env(
    cancel: Arc<AtomicBool>,
    bash_env: crate::BashEnv,
) -> (tempfile::TempDir, BuiltinTools) {
    let dir = tempfile::tempdir().unwrap();
    let auto: crate::AutoModeFn =
        Arc::new(|_| Box::pin(async { crate::AutoModeOutcome::Allow { reason: "t".into() } }));
    let tmp = dir.path().join("lofi-tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let tools = BuiltinTools::with_skills_dir(
        dir.path().to_path_buf(),
        None,
        tmp,
        bash_env,
        crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig::default()),
        None,
        Some(auto),
        None,
    )
    .with_cancel(Some(cancel));
    (dir, tools)
}

#[tokio::test]
async fn job_list_returns_spawned_jobs_newest_first() {
    let (_dir, tools) = tools_with_cancel(Arc::new(AtomicBool::new(false)));
    let first = tools
        .job_spawn(json!({ "cmd": "true", "timeoutMs": 60_000 }))
        .await
        .unwrap();
    let second = tools
        .job_spawn(json!({ "cmd": "true", "timeoutMs": 60_000 }))
        .await
        .unwrap();
    let listed = tools.job_list(json!({})).await.unwrap();
    assert_eq!(listed["ok"], json!(true));
    let jobs = listed["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2, "got: {listed}");
    let first_id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();
    assert_eq!(jobs[0]["id"].as_str().unwrap(), second_id);
    assert_eq!(jobs[1]["id"].as_str().unwrap(), first_id);
    // Entry shape mirrors jobStatus.
    for entry in jobs {
        for key in [
            "state",
            "command",
            "directory",
            "durationMs",
            "logPath",
            "tty",
        ] {
            assert!(entry.get(key).is_some(), "missing {key} in {entry}");
        }
    }
}

#[test]
fn key_press_bytes_maps_named_keys() {
    assert_eq!(key_press_bytes("Enter", false), Some(b"\r".as_slice()));
    assert_eq!(key_press_bytes("Tab", false), Some(b"\t".as_slice()));
    assert_eq!(key_press_bytes("Up", false), Some(b"\x1b[A".as_slice()));
    assert_eq!(key_press_bytes("Up", true), Some(b"\x1bOA".as_slice()));
    assert_eq!(key_press_bytes("Down", true), Some(b"\x1bOB".as_slice()));
    assert_eq!(key_press_bytes("Home", true), Some(b"\x1bOH".as_slice()));
    assert_eq!(key_press_bytes("Ctrl+C", true), Some(b"\x03".as_slice()));
    assert_eq!(key_press_bytes("F12", true), Some(b"\x1b[24~".as_slice()));
    assert_eq!(key_press_bytes("NotAKey", false), None);
}

#[tokio::test]
async fn job_key_press_uses_the_active_terminal_cursor_mode() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (_dir, tools) = tools_with_cancel(cancel);
    let spawned = tools
        .job_spawn(json!({
            "cmd": "stty raw -echo; printf '\\033[?1h'; dd bs=3 count=1 2>/dev/null | od -An -t x1",
            "tty": true,
            "notify": false
        }))
        .await
        .unwrap();
    let id = spawned["id"].as_str().unwrap().to_owned();
    wait_for_log(&tools, &id, "\x1b[?1h").await;
    tools
        .job_key_press(json!({ "id": id, "key": "Down" }))
        .await
        .unwrap();
    let done = wait_for_terminal(&tools, &id).await;
    assert_eq!(done["state"], json!("completed"), "{done}");
    let log = tools.job_read(json!({ "id": id })).await.unwrap();
    assert!(
        log["output"].as_str().unwrap().contains("1b 4f 42"),
        "{log}"
    );
}

#[test]
fn read_log_tail_reads_only_the_requested_suffix() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"prefix-suffix").unwrap();

    assert_eq!(read_log_tail(file.path().to_str().unwrap(), 6), "suffix");
}

#[test]
fn idle_tail_redacts_approved_environment_values() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"token=secret-value").unwrap();
    let bash_env = crate::BashEnv {
        redact: vec!["secret-value".to_string()],
        ..crate::BashEnv::default()
    };

    assert_eq!(
        idle_tail(None, file.path().to_str().unwrap(), &bash_env),
        "token=[redacted]"
    );
}

#[tokio::test]
async fn tty_job_accepts_typed_input_and_key_presses() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (_dir, tools) = tools_with_cancel(cancel);
    let spawned = tools
        .job_spawn(json!({ "cmd": "read x; echo \"got:$x\"", "tty": true }))
        .await
        .unwrap();
    assert_eq!(spawned["tty"], json!(true));
    let id = spawned["id"].as_str().unwrap().to_owned();

    let typed = tools
        .job_type(json!({ "id": id, "text": "hello" }))
        .await
        .unwrap();
    assert_eq!(typed["sent"], json!(5));
    let pressed = tools
        .job_key_press(json!({ "id": id, "key": "Enter" }))
        .await
        .unwrap();
    assert_eq!(pressed["ok"], json!(true));

    let done = wait_for_terminal(&tools, &id).await;
    assert_eq!(done["state"], json!("completed"), "got: {done}");
    let log = tools.job_read(json!({ "id": id })).await.unwrap();
    assert!(
        log["output"].as_str().unwrap().contains("got:hello"),
        "log: {log}"
    );
}

#[tokio::test]
async fn tty_job_exposes_its_parsed_terminal_screen() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (_dir, tools) = tools_with_cancel(cancel);
    let spawned = tools
        .job_spawn(json!({
            "cmd": "printf 'raw-marker\\033[2K\\r\\033[1;3;4;38;2;1;2;3;48;5;25mparsed-marker'; sleep 30",
            "tty": true,
            "cols": 24,
            "rows": 3
        }))
        .await
        .unwrap();
    let id = spawned["id"].as_str().unwrap().to_owned();
    wait_for_log(&tools, &id, "parsed-marker").await;

    let screen = tools.jobs.screen(id.parse().unwrap()).unwrap();
    assert_eq!((screen.cols, screen.rows), (24, 3));
    assert_eq!(screen.lines.len(), 3);
    let line: String = screen.lines[0]
        .spans
        .iter()
        .map(|span| span.text.as_str())
        .collect();
    assert!(line.contains("parsed-marker"), "{screen:?}");
    assert!(!line.contains("raw-marker"), "{screen:?}");
    let styled = screen.lines[0]
        .spans
        .iter()
        .find(|span| span.text.contains("parsed-marker"))
        .unwrap();
    assert_eq!(styled.style.foreground, JobColor::Rgb(1, 2, 3));
    assert_eq!(styled.style.background, JobColor::Indexed(25));
    assert!(styled.style.attributes.contains(JobAttributes::BOLD));
    assert!(styled.style.attributes.contains(JobAttributes::ITALIC));
    assert!(styled.style.attributes.contains(JobAttributes::UNDERLINE));
    let visible = tools.job_screen(json!({ "id": id })).await.unwrap();
    assert_eq!(visible["cols"], json!(24));
    assert_eq!(visible["rows"], json!(3));
    assert_eq!(visible["lines"][0], json!("parsed-marker"));
    assert!(tools.jobs.snapshot()[0].tty);
    tools.job_kill(json!({ "id": id })).await.unwrap();
}

#[tokio::test]
async fn job_screen_redacts_approved_environment_values() {
    let cancel = Arc::new(AtomicBool::new(false));
    let bash_env = crate::BashEnv {
        redact: vec!["secret-value".to_string()],
        ..crate::BashEnv::default()
    };
    let (_dir, tools) = tools_with_env(cancel, bash_env);
    let spawned = tools
        .job_spawn(json!({
            "cmd": "printf 'token=secret-value'",
            "tty": true,
            "cols": 24,
            "rows": 2,
            "notify": false
        }))
        .await
        .unwrap();
    let id = spawned["id"].as_str().unwrap().to_owned();
    wait_for_terminal(&tools, &id).await;

    let screen = tools.job_screen(json!({ "id": id })).await.unwrap();
    assert_eq!(screen["lines"][0], json!("token=[redacted]"));
}

#[tokio::test]
async fn tty_job_sets_term_after_applying_the_stripped_environment() {
    let cancel = Arc::new(AtomicBool::new(false));
    let bash_env = crate::BashEnv {
        strip_env: true,
        baseline: vec![
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            ("TERM".to_string(), "dumb".to_string()),
        ],
        ..crate::BashEnv::default()
    };
    let (_dir, tools) = tools_with_env(cancel, bash_env);
    let spawned = tools
        .job_spawn(json!({ "cmd": "printf %s \"$TERM\"", "tty": true }))
        .await
        .unwrap();
    let id = spawned["id"].as_str().unwrap().to_owned();
    let done = wait_for_terminal(&tools, &id).await;
    assert_eq!(done["state"], json!("completed"), "got: {done}");
    let log = tools.job_read(json!({ "id": id })).await.unwrap();
    assert!(
        log["output"].as_str().unwrap().contains("xterm-256color"),
        "log: {log}"
    );
}

#[tokio::test]
async fn tty_input_rejects_a_completed_job() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (_dir, tools) = tools_with_cancel(cancel);
    let spawned = tools
        .job_spawn(json!({ "cmd": "true", "tty": true }))
        .await
        .unwrap();
    let id = spawned["id"].as_str().unwrap().to_owned();
    wait_for_terminal(&tools, &id).await;

    let err = tools
        .job_type(json!({ "id": id, "text": "x" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not running"), "got: {err}");
}

#[tokio::test]
async fn job_type_rejects_a_non_tty_job() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (_dir, tools) = tools_with_cancel(cancel);
    let spawned = tools
        .job_spawn(json!({ "cmd": "sleep 30", "notify": false }))
        .await
        .unwrap();
    let id = spawned["id"].as_str().unwrap().to_owned();
    let err = tools
        .job_type(json!({ "id": id, "text": "x" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a tty job"), "got: {err}");
    tools.job_kill(json!({ "id": id })).await.unwrap();
}

#[tokio::test]
async fn tty_job_queues_one_idle_notice() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (_dir, tools) = tools_with_cancel(cancel);
    let spawned = tools
        .job_spawn(json!({ "cmd": "printf 'Name? '; read x; echo \"got:$x\"", "tty": true, "idleMs": 500 }))
        .await
        .unwrap();
    let id = spawned["id"].as_str().unwrap().to_owned();
    // One idle window plus a little scheduling slack.
    tokio::time::sleep(Duration::from_millis(900)).await;
    let notices = tools.jobs.drain_notices();
    let idle = notices.iter().find(|n| n.text.contains("idle"));
    assert!(idle.is_some(), "notices: {notices:?}");
    assert!(idle.unwrap().text.contains("Name?"), "notices: {notices:?}");
    tools.job_kill(json!({ "id": id })).await.unwrap();
}
