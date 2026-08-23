use super::*;

use ratatui::backend::TestBackend;

struct PanickingProvider;

#[async_trait]
impl Provider for PanickingProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> lofi_error::Result<BoxStream<'static, lofi_error::Result<StreamingEvent>>> {
        panic!("provider settlement panic");
    }
}

fn panicking_agent(root: &Path) -> Agent {
    Agent::new(
        Box::new(PanickingProvider),
        Model {
            id: "panic".into(),
            name: "Panic".into(),
            provider: "test".into(),
            api: lofi_types::Api::OpenAiCompletions,
            reasoning: false,
            thinking: ThinkingLevel::Off,
            service_tier: ServiceTier::Auto,
            supports_image: false,
            context_window: Some(100_000),
            max_tokens: None,
            base_url: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            per_request_price: None,
        },
        root.to_path_buf(),
        root.join("tmp"),
        String::new(),
        None,
        0,
        &lofi_types::BashConfig::default(),
        &[],
        lofi_types::TruncateConfig::default(),
        lofi_types::ImageConfig::default(),
        &lofi_types::ShellPolicyConfig::default(),
    )
}

async fn run_agent_task_panic_tui_e2e_child() {
    let root = tempfile::tempdir().unwrap();
    let agent = panicking_agent(root.path());
    run(
        Some(agent),
        "test/panic".into(),
        ThinkingLevel::Off,
        ServiceTier::Auto,
        lofi_types::ThemeMode::Dark,
        SessionConfig::ephemeral(root.path().to_path_buf()),
        None,
        100_000,
        lofi_types::CompactionConfig::default(),
        None,
        String::new(),
    )
    .await
    .unwrap();
}

fn wait_for_tui_output(
    child: &mut std::process::Child,
    master: &mut std::fs::File,
    output: &mut String,
    needle: &str,
) {
    use std::io::Read as _;

    let start = Instant::now();
    loop {
        let mut chunk = [0_u8; 8192];
        loop {
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => output.push_str(&String::from_utf8_lossy(&chunk[..read])),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("PTY read failed: {error}"),
            }
        }
        if output.contains(needle) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for {needle:?}; terminal output:\n{output}"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited: {output}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn agent_task_panic_is_visible_in_tui_e2e() {
    use nix::pty::{openpty, Winsize};
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::thread;

    if std::env::var_os("LOFI_E2E_PANIC_CHILD").is_some() {
        run_agent_task_panic_tui_e2e_child().await;
        return;
    }

    let pty = openpty(
        Some(&Winsize {
            ws_row: 30,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        None,
    )
    .unwrap();
    let mut master = File::from(pty.master);
    let slave = File::from(pty.slave);
    let stdin = slave.try_clone().unwrap();
    let stdout = slave.try_clone().unwrap();
    let controlling = slave.try_clone().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "tui::tests::reliability::agent_task_panic_is_visible_in_tui_e2e",
            "--nocapture",
        ])
        .env("LOFI_E2E_PANIC_CHILD", "1")
        .env("TERM", "xterm-256color")
        .env("COLUMNS", "120")
        .env("LINES", "30")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(slave));
    #[allow(unsafe_code)]
    unsafe {
        let fd = controlling.as_raw_fd();
        command.pre_exec(move || {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            if nix::libc::ioctl(fd, nix::libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(controlling);

    let flags = nix::fcntl::fcntl(master.as_raw_fd(), nix::fcntl::FcntlArg::F_GETFL).unwrap();
    nix::fcntl::fcntl(
        master.as_raw_fd(),
        nix::fcntl::FcntlArg::F_SETFL(
            nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK,
        ),
    )
    .unwrap();
    let mut output = String::new();

    wait_for_tui_output(&mut child, &mut master, &mut output, "test/panic");
    master.write_all(b"trigger panic\r").unwrap();
    master.flush().unwrap();
    wait_for_tui_output(
        &mut child,
        &mut master,
        &mut output,
        "agent task panicked: provider settlement panic",
    );
    master.write_all(b"/quit\r").unwrap();
    master.flush().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(10) {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("child did not exit; terminal output:\n{output}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "terminal output:\n{output}");
}

#[tokio::test(flavor = "current_thread")]
async fn agent_task_panic_is_visible_and_task_settles() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = tempfile::tempdir().unwrap();
            let agent = panicking_agent(root.path());
            let mut a = app();
            let mut run = None;

            spawn_prompt(
                &mut a,
                Some(&agent),
                &mut run,
                "trigger panic".into(),
                lofi_types::PromptKind::User,
            );

            let mut run = run.expect("agent run");
            while let Some(event) = run.rx.recv().await {
                a.apply_event(event);
            }
            run.handle.await.expect("agent task settled");
            a.run_finished();

            assert!(a.turns[0].blocks.iter().any(|block| matches!(
                block,
                Block::Error(message)
                    if message == "agent task panicked: provider settlement panic"
            )));
            assert!(a.run.is_none());
        })
        .await;
}
