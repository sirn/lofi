#![allow(clippy::wildcard_imports)]

use super::*;

#[allow(clippy::too_many_lines, clippy::match_same_arms)]
pub(super) fn handle_event(
    ev: &Event,
    app: &mut App,
    agent: Option<&Agent>,
    current_run: &mut Option<RunHandle>,
) {
    if let Event::Mouse(m) = ev {
        if !app.modal_open() {
            handle_mouse(*m, app);
        }
        return;
    }
    if let Event::Paste(s) = ev {
        if app.mode == Mode::Input && !app.modal_open() {
            app.sel = None;
            app.insert_str(s);
        }
        return;
    }
    let Event::Key(k) = ev else {
        return;
    };
    if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }
    if app.handle_modal_stack_key(k) {
        return;
    }

    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        handle_ctrl_c(app, current_run);
        return;
    }

    // Tab and Escape peel a focused detail before their outer action: Tab
    // does not drop to the input area and Escape does not abort the run
    // while an expansion is open.
    if matches!(k.code, KeyCode::Tab | KeyCode::Esc) && app.detail_focus.is_some() {
        app.collapse_detail_focus();
        return;
    }
    if k.code == KeyCode::Esc
        && current_run.is_some()
        && (app.mode != Mode::Input || app.slash_complete.is_none())
    {
        interrupt_run(app, current_run);
        return;
    }

    if app.mode != Mode::Input {
        app.last_kill_was_kill = false;
        match app.mode {
            Mode::Navigate => handle_nav_key(k, app),
            Mode::Select => handle_select_key(k, app),
            Mode::Input => {}
        }
        return;
    }

    app.sel = None;
    let append_kill = app.last_kill_was_kill;
    app.last_kill_was_kill = false;

    if k.code == KeyCode::Enter && k.modifiers.contains(KeyModifiers::ALT) {
        app.insert_newline();
        app.refresh_slash_complete();
        return;
    }
    if k.code == KeyCode::Char('j') && k.modifiers.contains(KeyModifiers::CONTROL) {
        app.insert_newline();
        app.refresh_slash_complete();
        return;
    }
    if app.handle_popover_key(k) {
        return;
    }
    match k.code {
        KeyCode::Enter
            if (current_run.is_some() || app.lifecycle_busy) && !app.input.is_empty() =>
        {
            let prompt = std::mem::take(&mut app.input);
            app.input_cursor = 0;
            app.history_idx = None;
            app.slash_complete = None;
            if app.slash_command(&prompt) {
                return;
            }
            app.history_nav.push(prompt.clone());
            app.prompt_queue.push(QueuedPrompt {
                text: prompt,
                kind: lofi_types::PromptKind::User,
            });
            // Signal the live run to yield after its current round. Setting the
            // flag here (not on the next event) closes the race against a
            // blocking tool call during which no events flow.
            if let Some(r) = current_run {
                r.preempt.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            return;
        }
        KeyCode::Enter if current_run.is_none() && !app.input.is_empty() => {
            let prompt = std::mem::take(&mut app.input);
            app.input_cursor = 0;
            app.history_idx = None;
            app.slash_complete = None;
            if app.slash_command(&prompt) {
                return;
            }
            app.history_nav.push(prompt.clone());
            if let Some((command, exclude_from_context)) = parse_user_shell(&prompt) {
                spawn_user_shell(app, current_run, command, exclude_from_context);
                return;
            }
            let Some(agent) = agent else {
                app.push_turn(Turn {
                    prompt,
                    kind: lofi_types::PromptKind::User,
                    blocks: Vec::new(),
                });
                if let Some(hint) = &app.no_models_hint {
                    if let Some(turn) = app.turns.last_mut() {
                        turn.blocks.push(Block::Error(hint.clone()));
                    }
                }
                return;
            };
            spawn_agent_run(
                app,
                current_run,
                agent,
                Some(prompt),
                lofi_types::PromptKind::User,
            );
        }
        KeyCode::Backspace if k.modifiers.contains(KeyModifiers::ALT) => app.kill_word_back(),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete_forward_char(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Up if k.modifiers.contains(KeyModifiers::ALT) => {
            // Pop the most recent queued prompt for editing. Skip Notices:
            // system-injected text has no business round-tripping through
            // the editor where the user could resubmit it as if typed.
            let mut user_idx = None;
            for (i, q) in app.prompt_queue.iter().enumerate().rev() {
                if q.kind == lofi_types::PromptKind::User {
                    user_idx = Some(i);
                    break;
                }
            }
            if let Some(i) = user_idx {
                let queued = app.prompt_queue.remove(i);
                app.input = queued.text;
                app.input_cursor = app.input.len();
                app.history_idx = None;
                app.refresh_slash_complete();
            }
        }
        KeyCode::Up if app.slash_complete.is_some() => app.slash_complete_up(),
        KeyCode::Down if app.slash_complete.is_some() => app.slash_complete_down(),
        KeyCode::Up => app.cursor_up(),
        KeyCode::Down => app.cursor_down(),
        KeyCode::Tab => app.enter_nav(),
        KeyCode::Esc => app.clear_input(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_line_start(),
        KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_line_end(),
        KeyCode::Char('b') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_left(),
        KeyCode::Char('f') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_right(),
        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => app.cursor_down(),
        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => app.cursor_up(),
        KeyCode::Char('h') if k.modifiers.contains(KeyModifiers::CONTROL) => app.backspace(),
        KeyCode::Char('k') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.kill_line_end(append_kill);
        }
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => app.kill_line_start(),
        KeyCode::Char('w') if k.modifiers.contains(KeyModifiers::CONTROL) => app.kill_word_back(),
        KeyCode::Char('y') if k.modifiers.contains(KeyModifiers::CONTROL) => app.yank(),
        KeyCode::Char('b') if k.modifiers.contains(KeyModifiers::ALT) => app.move_word_back(),
        KeyCode::Char('f') if k.modifiers.contains(KeyModifiers::ALT) => app.move_word_fwd(),
        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::ALT) => app.kill_word_fwd(),
        KeyCode::Char('<') if k.modifiers.contains(KeyModifiers::ALT) => app.input_cursor = 0,
        KeyCode::Char('>') if k.modifiers.contains(KeyModifiers::ALT) => {
            app.input_cursor = app.input.len();
        }
        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.input.is_empty() {
                request_quit(app, current_run);
            } else {
                app.delete_forward_char();
            }
        }

        KeyCode::Char(c)
            if !k
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.insert_char(c);
        }
        _ => {}
    }
    app.refresh_slash_complete();
}

fn parse_user_shell(prompt: &str) -> Option<(String, bool)> {
    if let Some(command) = prompt.strip_prefix("!!") {
        let command = command.trim_start();
        return (!command.is_empty()).then(|| (command.to_string(), true));
    }
    let command = prompt.strip_prefix('!')?.trim_start();
    (!command.is_empty()).then(|| (command.to_string(), false))
}

#[cfg(test)]
mod user_shell_tests {
    use super::parse_user_shell;

    #[test]
    fn parses_context_modes() {
        assert_eq!(
            parse_user_shell("!  printf ok"),
            Some(("printf ok".to_string(), false))
        );
        assert_eq!(
            parse_user_shell("!! printf ok"),
            Some(("printf ok".to_string(), true))
        );
        assert_eq!(parse_user_shell("!   "), None);
        assert_eq!(parse_user_shell("ordinary prompt"), None);
    }
}

pub(super) fn persist_unsent_prompts(app: &mut App) {
    if app.prompt_queue.is_empty() {
        return;
    }
    let run_model = app.run_model();
    let system_prompt = app.system_prompt.clone();
    for queued in std::mem::take(&mut app.prompt_queue) {
        if queued.text.trim().is_empty() {
            continue;
        }
        let Some(sink) = app.session.sink_mut() else {
            continue;
        };
        let recorded = sink
            .cursor_or_create(&run_model, &system_prompt)
            .and_then(|cursor| cursor.record_unrun_prompt(&queued.text, queued.kind));
        if let Err(error) = recorded {
            app.notify(
                NotifyKind::Error,
                format!("could not save queued prompt: {error}"),
            );
        }
    }
    app.session.refresh_cursor();
}

pub(super) fn finish_user_shell(
    app: &mut App,
    result: lofi_core::UserShellResult,
    exclude_from_context: bool,
) {
    if !exclude_from_context {
        if let Err(error) = app.lifecycle.push_message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: result.context_text(),
            }],
            kind: PromptKind::default(),
        }) {
            app.notify(NotifyKind::Error, format!("update agent history: {error}"));
        }
    }
    // Recording the command is a core-owned session write — the UI hands the
    // finished result to the sink rather than assembling/appending an event.
    let run_model = app.run_model();
    let mut byte_range = None;
    if let Some(sink) = app.session.sink_mut() {
        match sink.record_user_shell(
            &result,
            exclude_from_context,
            &run_model,
            &app.system_prompt,
        ) {
            Ok(range) => byte_range = Some(range),
            Err(error) => app.notify(
                NotifyKind::Error,
                format!("transcript write failed: {error}"),
            ),
        }
    }
    app.session.refresh_cursor();
    app.apply_event(AgentEvent::UserShell {
        command: result.command,
        output: result.output,
        exit_code: result.exit_code,
        signal: result.signal,
        duration_ms: result.duration_ms,
        truncated: result.truncated,
        cancelled: result.cancelled,
        exclude_from_context,
    });
    if let (Some(range), Some(slot)) = (byte_range, app.turn_byte_ranges.last_mut()) {
        *slot = Some(range);
    }
}

fn install_run(app: &mut App, current_run: &mut Option<RunHandle>, run: RunHandle) {
    *current_run = Some(run);
    app.run = Some(0);
    app.run_start = Some(Instant::now());
    app.run_model_label = Some(app.session_model());
    app.pinned = true;
}

fn spawn_agent_run(
    app: &mut App,
    current_run: &mut Option<RunHandle>,
    agent: &lofi_core::Agent,
    prompt: Option<String>,
    prompt_kind: lofi_types::PromptKind,
) {
    let run_model = app.run_model();
    let system_prompt = app.system_prompt.clone();
    let mut session_sink = app.session.sink.clone();
    let session_cursor = Arc::new(std::sync::Mutex::new(app.session.cursor.clone()));
    let worker_cursor = Arc::clone(&session_cursor);
    // Mirror the lineage-birth pin into the live history so the first request
    // carries the system prompt; resume has already rebuilt history from the
    // log, so seed_system is a no-op there.
    let _ = app.lifecycle.seed_system(&app.system_prompt);
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let history = app.lifecycle.shared_history();
    let agent = agent.clone();
    let err_tx = tx.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = cancel.clone();
    let preempt = Arc::new(AtomicBool::new(false));
    let preempt_clone = preempt.clone();
    let continuation = prompt.is_none();
    let prompt = prompt.unwrap_or_default();
    // Notices computed at session startup (e.g. stale job ids) ride the
    // first agent run so the model sees them inline before the user's text.
    // Continuations skip them: mid-run context is already shaped.
    let startup_notices = if continuation {
        Vec::new()
    } else {
        std::mem::take(&mut app.startup_notices)
    };
    // Pre-render only when the submitted prompt is the first durable row.
    // Otherwise the core event stream must establish notice-before-user
    // ordering without deduplicating the wrong last row.
    if !continuation && startup_notices.is_empty() {
        app.begin_prompt_turn(prompt.clone(), prompt_kind);
    }
    // The agent run loop performs durable transcript syncs. Its own runtime
    // keeps those blocking writes off the current-thread TUI runtime.
    let handle = tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let result = runtime.map_err(|error| format!("start agent runtime: {error}"));
        let result = result.and_then(|runtime| {
            runtime.block_on(async move {
                let cursor = session_sink
                    .as_mut()
                    .map(|sink| sink.cursor_or_create(&run_model, &system_prompt))
                    .transpose()
                    .map_err(|error| format!("create session: {error}"))?;
                if let Some(cursor) = cursor.as_ref() {
                    *worker_cursor
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cursor.clone());
                }
                let mut messages = history.lock().map(|m| m.clone()).unwrap_or_default();
                let result = std::panic::AssertUnwindSafe(agent.run_continuation_with_notices(
                    &mut messages,
                    prompt,
                    prompt_kind,
                    Vec::new(),
                    startup_notices,
                    tx,
                    cursor.as_ref(),
                    continuation,
                    Some(cancel_clone),
                    Some(preempt_clone),
                ))
                .catch_unwind()
                .await;
                if let Ok(mut stored) = history.lock() {
                    *stored = messages;
                }
                match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(payload) => Err(format!(
                        "agent task panicked: {}",
                        panic_message(payload.as_ref())
                    )),
                }
            })
        });
        if let Err(error) = result {
            let _ = err_tx.blocking_send(AgentEvent::Error(error));
        }
    });
    install_run(
        app,
        current_run,
        RunHandle {
            handle,
            rx,
            cancel,
            preempt,
            session_cursor,
            user_shell: None,
        },
    );
}

pub(super) fn spawn_user_shell(
    app: &mut App,
    current_run: &mut Option<RunHandle>,
    command: String,
    exclude_from_context: bool,
) {
    // Ensure a session exists (core-owned) so the finished command can be
    // recorded; mirror the cursor for reads.
    let run_model = app.run_model();
    if let Some(sink) = app.session.sink_mut() {
        let _ = sink.cursor_or_create(&run_model, &app.system_prompt);
    }
    app.session.refresh_cursor();
    // Deltas can arrive per pipe drain; 1 would turn every chunk into a
    // round trip against the event loop.
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let cwd = app.session.cwd.clone();
    let command_for_run = command.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_run = Arc::clone(&cancel);
    let handle = tokio::task::spawn_local(async move {
        let _ = tx
            .send(AgentEvent::UserShellStart {
                command: command_for_run.clone(),
                exclude_from_context,
            })
            .await;
        let (delta_tx, mut delta_rx) = tokio::sync::mpsc::channel::<String>(256);
        let delta_forward = tx.clone();
        let forwarder = tokio::task::spawn_local(async move {
            while let Some(delta) = delta_rx.recv().await {
                if delta_forward
                    .send(AgentEvent::UserShellDelta(delta))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let event = match Box::pin(lofi_core::run_user_shell_command(
            &cwd,
            command_for_run.clone(),
            cancel_for_run,
            Some(delta_tx),
        ))
        .await
        {
            Ok(result) => AgentEvent::UserShell {
                command: result.command,
                output: result.output,
                exit_code: result.exit_code,
                signal: result.signal,
                duration_ms: result.duration_ms,
                truncated: result.truncated,
                cancelled: result.cancelled,
                exclude_from_context,
            },
            Err(error) => AgentEvent::UserShell {
                command: command_for_run,
                output: error.to_string(),
                exit_code: Some(1),
                signal: None,
                duration_ms: 0,
                truncated: false,
                cancelled: false,
                exclude_from_context,
            },
        };
        // Drain every queued delta before the finishing event so the block
        // finalizes the accumulated stream, not the other way around.
        let _ = forwarder.await;
        let _ = tx.send(event).await;
    });
    install_run(
        app,
        current_run,
        RunHandle {
            handle,
            rx,
            cancel,
            preempt: Arc::new(AtomicBool::new(false)),
            session_cursor: Arc::new(std::sync::Mutex::new(app.session.cursor.clone())),
            user_shell: Some((command, exclude_from_context)),
        },
    );
}

pub(super) fn spawn_prompt(
    app: &mut App,
    agent: Option<&lofi_core::Agent>,
    current_run: &mut Option<RunHandle>,
    prompt: String,
    kind: lofi_types::PromptKind,
) {
    if let Some((command, exclude_from_context)) = parse_user_shell(&prompt) {
        spawn_user_shell(app, current_run, command, exclude_from_context);
        return;
    }
    let Some(agent) = agent else {
        app.push_turn(Turn {
            prompt,
            kind,
            blocks: app
                .no_models_hint
                .clone()
                .map_or_else(Vec::new, |hint| vec![Block::Error(hint)]),
        });
        return;
    };
    spawn_agent_run(app, current_run, agent, Some(prompt), kind);
}

pub(super) fn spawn_continue(
    app: &mut App,
    agent: Option<&lofi_core::Agent>,
    current_run: &mut Option<RunHandle>,
) {
    if let Some(agent) = agent {
        spawn_agent_run(app, current_run, agent, None, lofi_types::PromptKind::User);
    }
}

pub(super) fn restore_queued_prompts(app: &mut App) {
    if app.prompt_queue.is_empty() {
        return;
    }
    // User-typed follow-ups belong back in the editor on interrupt. System-
    // injected Notices stay queued for the next round. They must not enter
    // `app.input`, but dropping them here loses a job completion that raced
    // with cancellation of the foreground run.
    let mut user_prompts = Vec::new();
    app.prompt_queue.retain(|queued| {
        if queued.kind == lofi_types::PromptKind::User {
            user_prompts.push(queued.text.clone());
            false
        } else {
            true
        }
    });
    let queued = user_prompts.join("\n\n");
    let draft = std::mem::take(&mut app.input);
    app.input = [queued, draft]
        .into_iter()
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    app.input_cursor = app.input.len();
    app.history_idx = None;
    app.refresh_slash_complete();
}

fn interrupt_run(app: &mut App, current_run: &mut Option<RunHandle>) {
    if let Some(r) = current_run.as_mut() {
        if r.user_shell.is_none() {
            // Restore steering/follow-up messages to the editor when a
            // stream is aborted instead of submitting them automatically.
            restore_queued_prompts(app);
        }
        // All runs must settle cooperatively: cancel only raises the flag.
        // A user shell then finishes by returning what its pipes captured so
        // far — aborting here would discard the streamed output — and an
        // agent run checkpoints each completed round, flushes the partial
        // current response, and writes a TurnCancelled marker before its
        // channel closes. Both paths finish through the event loop.
        r.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        app.ctrl_c_at = None;
    }
}

pub(super) fn request_quit(app: &mut App, current_run: &mut Option<RunHandle>) {
    app.should_quit = true;
    if let Some(r) = current_run.as_mut() {
        // Signal cancel now. The event loop waits for flush and does not abort.
        r.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(super) fn handle_ctrl_c(app: &mut App, current_run: &mut Option<RunHandle>) {
    // Navigate/Select: Ctrl-C is a "give me the prompt" key, not a cancel. It
    // drops the user onto the newest transcript line and focuses the editor
    // without ever interrupting the current turn.
    if app.mode != Mode::Input {
        app.enter_input();
        app.pin_to_latest();
        app.ctrl_c_at = None;
        return;
    }
    // A non-empty prompt is the more immediate thing to dismiss; clearing it
    // also takes priority over cancelling the turn so a single Ctrl-C never
    // both wipes text and kills a run.
    if !app.input.is_empty() {
        app.clear_input();
        app.ctrl_c_at = None;
        return;
    }
    if current_run.is_some() {
        interrupt_run(app, current_run);
        app.ctrl_c_at = None;
        return;
    }
    let now = Instant::now();
    if app
        .ctrl_c_at
        .is_some_and(|t| now.duration_since(t) < QUIT_DOUBLE_PRESS)
    {
        app.should_quit = true;
    } else {
        app.ctrl_c_at = Some(now);
    }
}

#[derive(Clone, Copy)]
pub(super) enum WordMotion {
    NextStart { big: bool },
    PrevStart { big: bool },
    NextEnd { big: bool },
}

/// Dispatches the vim line-movement keys (`$`, `^`, `0`, `w`/`b`/`e`/`W`/`B`/`E`).
/// Returns `true` when the key was a motion so the caller can skip its own
/// match. Shared by Navigate and Select.
pub(super) fn apply_motion(k: &KeyEvent, app: &mut App) -> bool {
    match k.code {
        KeyCode::Char('$') => {
            let (_, cend) = app.cursor_content_range();
            app.nav_set_col(cend.saturating_sub(1));
            true
        }
        KeyCode::Char('0') => {
            let (cstart, _) = app.cursor_content_range();
            app.nav_set_col(cstart);
            true
        }
        KeyCode::Char('^') => {
            app.nav_set_col(app.first_nonblank_col());
            true
        }
        KeyCode::Char('w') => {
            app.nav_word_motion(WordMotion::NextStart { big: false });
            true
        }
        KeyCode::Char('W') => {
            app.nav_word_motion(WordMotion::NextStart { big: true });
            true
        }
        KeyCode::Char('b') => {
            app.nav_word_motion(WordMotion::PrevStart { big: false });
            true
        }
        KeyCode::Char('B') => {
            app.nav_word_motion(WordMotion::PrevStart { big: true });
            true
        }
        KeyCode::Char('e') => {
            app.nav_word_motion(WordMotion::NextEnd { big: false });
            true
        }
        KeyCode::Char('E') => {
            app.nav_word_motion(WordMotion::NextEnd { big: true });
            true
        }
        _ => false,
    }
}

pub(super) fn handle_nav_key(k: &KeyEvent, app: &mut App) {
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return;
    }
    if k.modifiers.contains(KeyModifiers::SHIFT) && matches!(k.code, KeyCode::Up | KeyCode::Down) {
        return;
    }
    if apply_motion(k, app) {
        return;
    }
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.sel = None;
            app.nav_move(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.sel = None;
            app.nav_move(-1);
        }
        KeyCode::Char('h') | KeyCode::Left => {
            app.sel = None;
            app.nav_col_delta(-1);
        }
        KeyCode::Char('l') | KeyCode::Right => {
            app.sel = None;
            app.nav_col_delta(1);
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            app.sel = None;
            if !app.toggle_cursor_detail() && k.code == KeyCode::Enter {
                app.yank_line();
                app.enter_input();
            }
        }
        KeyCode::Char('g') => {
            app.sel = None;
            app.nav_top();
        }
        KeyCode::Char('G') => {
            app.sel = None;
            app.nav_bottom();
        }
        KeyCode::Char('v') => app.enter_select(),
        KeyCode::Char('y') => {
            app.yank_line();
            app.enter_input();
        }
        KeyCode::Char('i') | KeyCode::Tab => app.enter_input(),
        KeyCode::Char('[') if app.detail_focus.is_none() => app.nav_jump_turn(-1),
        KeyCode::Char(']') if app.detail_focus.is_none() => app.nav_jump_turn(1),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        _ => {}
    }
}

pub(super) fn handle_select_key(k: &KeyEvent, app: &mut App) {
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return;
    }
    if k.modifiers.contains(KeyModifiers::SHIFT) && matches!(k.code, KeyCode::Up | KeyCode::Down) {
        return;
    }
    if apply_motion(k, app) {
        return;
    }
    match k.code {
        KeyCode::Enter | KeyCode::Char(' ') if app.detail_focus.is_some() => {
            app.toggle_cursor_detail();
        }
        KeyCode::Char('j') | KeyCode::Down => app.nav_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.nav_move(-1),
        KeyCode::Char('h') | KeyCode::Left => app.nav_col_delta(-1),
        KeyCode::Char('l') | KeyCode::Right => app.nav_col_delta(1),
        KeyCode::Char('g') => app.nav_top(),
        KeyCode::Char('G') => app.nav_bottom(),
        KeyCode::Char('y') | KeyCode::Enter => {
            app.yank_selection();
            app.enter_input();
        }
        KeyCode::Tab | KeyCode::Esc => {
            app.mode = Mode::Navigate;
            app.sel = None;
        }
        KeyCode::Char('[') if app.detail_focus.is_none() => app.nav_jump_turn(-1),
        KeyCode::Char(']') if app.detail_focus.is_none() => app.nav_jump_turn(1),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        _ => {}
    }
}

pub(super) fn handle_mouse(m: MouseEvent, app: &mut App) {
    let in_log = m.row >= app.log_rect.y
        && m.row < app.log_rect.y + app.log_rect.height
        && m.column >= app.log_rect.x
        && m.column < app.log_rect.x + app.log_rect.width;
    let can_select = app.mode == Mode::Input || app.mode == Mode::Navigate;
    let detail_at = |app: &App, row: u16| {
        let rel = row.saturating_sub(app.log_rect.y) as usize;
        app.log_details.get(rel)?.clone()
    };
    match m.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if in_log => {
            let delta = if matches!(m.kind, MouseEventKind::ScrollUp) {
                -1
            } else {
                1
            };
            let cell = log_cell(app, m.row, m.column);
            app.nav_cursor = cell.0;
            if let Some(detail) = detail_at(app, m.row) {
                if app.mode == Mode::Navigate {
                    app.detail_focus = Some(DetailFocus::on(&detail, cell.1));
                }
            } else {
                app.collapse_detail_focus();
            }
            if !app.scroll_cursor_detail(delta) {
                app.scroll_nav(if delta < 0 { -3 } else { 3 });
            }
        }
        MouseEventKind::Down(MouseButton::Left) if can_select => {
            app.sel = None;
            if in_log {
                let cell = log_cell(app, m.row, m.column);
                if app.mode == Mode::Navigate {
                    if let Some(detail) = detail_at(app, m.row) {
                        if detail.row.is_some() {
                            app.detail_focus = Some(DetailFocus::on(&detail, cell.1));
                            app.sel = Some(Selection {
                                start: cell,
                                end: cell,
                            });
                            return;
                        }
                    }
                }
                app.sel = Some(Selection {
                    start: cell,
                    end: cell,
                });
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if in_log && app.detail_focus.is_some() => {
            let cell = log_cell(app, m.row, m.column);
            if let Some(detail) = detail_at(app, m.row) {
                if let Some(focus) = app.detail_focus.as_mut() {
                    if detail.key == focus.key {
                        if let Some(row) = detail.row {
                            focus.cursor = row;
                            focus.col = cell.1;
                            app.nav_cursor = cell.0;
                            app.nav_col = cell.1;
                            if let Some(sel) = app.sel.as_mut() {
                                sel.end = cell;
                            }
                        }
                    }
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if in_log && can_select => {
            let cell = log_cell(app, m.row, m.column);
            if let Some(sel) = app.sel.as_mut() {
                sel.end = cell;
            }
        }
        MouseEventKind::Up(MouseButton::Left) if can_select => {
            app.yank_selection();
            // Return to Input mode after a drag-yank so the user can
            // immediately type (mouse drag is a quick-peek action).
            app.enter_input();
        }
        _ => {}
    }
}

/// Map a screen cell inside the log viewport to (select-line index, char
/// index) in `log_vis`, using display width so wide chars land correctly.
/// The char index is clamped to the line's content range so a press/drag in
/// the gutter or padding snaps to the content edge — the selection never
/// starts or ends in the decorative whitespace.
pub(super) fn log_cell(app: &App, row: u16, column: u16) -> (usize, usize) {
    let rel_y = row.saturating_sub(app.log_rect.y) as usize;
    let rel_x = column.saturating_sub(app.log_rect.x) as usize;
    let line_idx = app.log_off.saturating_add(rel_y);
    let line = app.log_vis.get(rel_y);
    let col = line.map_or(rel_x, |vl| {
        let s = &vl.rendered;
        let (cstart, cend) = vl.content;
        col_to_char_idx(s, rel_x).clamp(cstart, cend)
    });
    (line_idx, col)
}
