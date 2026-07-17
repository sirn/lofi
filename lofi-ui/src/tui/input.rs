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
    if app.handle_info_key(k) {
        return;
    }
    if app.handle_modal_key(k) {
        return;
    }

    if app.handle_confirm_key(k) {
        return;
    }

    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        handle_ctrl_c(app, agent, current_run);
        return;
    }

    // Escape is the interrupt key: it dismisses completion first, then aborts
    // an active stream. Ctrl-C is deliberately not a bare interrupt alias; it
    // first peels away the editor/nav state (see handle_ctrl_c) so it only
    // cancels a turn from a clean, empty prompt.
    if k.code == KeyCode::Esc
        && current_run.is_some()
        && (app.mode != Mode::Input || app.slash_complete.is_none())
    {
        interrupt_run(app, agent, current_run);
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
        KeyCode::Enter if current_run.is_some() && !app.input.is_empty() => {
            let prompt = std::mem::take(&mut app.input);
            app.input_cursor = 0;
            app.history_idx = None;
            app.slash_complete = None;
            if app.slash_command(&prompt) {
                return;
            }
            app.history_nav.push(prompt.clone());
            app.prompt_queue.push(prompt);
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
            if let Some((command, exclude_from_context)) = parse_user_bash(&prompt) {
                spawn_user_bash(app, current_run, command, exclude_from_context);
                return;
            }
            // Keep the completed turn intact until the engine's TurnStart
            // arrives. TurnStart freezes it and pushes the new prompt in one
            // event-handler call, so an intervening redraw cannot expose an
            // old prompt with its assistant response temporarily removed.
            let Some(agent) = agent else {
                app.push_turn(Turn {
                    prompt: prompt.clone(),
                    blocks: Vec::new(),
                });
                if let Some(hint) = &app.no_models_hint {
                    if let Some(turn) = app.turns.last_mut() {
                        turn.blocks.push(Block::Error(hint.clone()));
                    }
                }
                return;
            };
            spawn_agent_run(app, current_run, agent, Some(prompt));
        }
        KeyCode::Backspace if k.modifiers.contains(KeyModifiers::ALT) => app.kill_word_back(),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete_forward_char(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Up if k.modifiers.contains(KeyModifiers::ALT) => {
            if let Some(prompt) = app.prompt_queue.pop() {
                app.input = prompt;
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
                app.should_quit = true;
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

fn parse_user_bash(prompt: &str) -> Option<(String, bool)> {
    if let Some(command) = prompt.strip_prefix("!!") {
        let command = command.trim_start();
        return (!command.is_empty()).then(|| (command.to_string(), true));
    }
    let command = prompt.strip_prefix('!')?.trim_start();
    (!command.is_empty()).then(|| (command.to_string(), false))
}

#[cfg(test)]
mod user_bash_tests {
    use super::parse_user_bash;

    #[test]
    fn parses_context_modes() {
        assert_eq!(
            parse_user_bash("!  printf ok"),
            Some(("printf ok".to_string(), false))
        );
        assert_eq!(
            parse_user_bash("!! printf ok"),
            Some(("printf ok".to_string(), true))
        );
        assert_eq!(parse_user_bash("!   "), None);
        assert_eq!(parse_user_bash("ordinary prompt"), None);
    }
}

/// Ctrl+C: cancel an active run; otherwise clear a non-empty draft, or quit
/// on a double press within [`QUIT_DOUBLE_PRESS`] when the prompt is empty.
/// Mode-independent — works the same in Input, Navigate, and Select.
/// Kick off a silent force-continue after a hard-cap force-compact.
/// Mirrors the submit path in [`handle_event`] but appends no user prompt:
/// it seeds the run from the (just-compacted) history, which ends in a tool
/// result, so the model resumes the turn. The engine emits `TurnContinue`,
/// which the UI handles by appending to the current turn rather than pushing
/// a new one. No-op when no model is configured.
/// Start a new run with a queued prompt (FIFO pop at turn end). Shares
/// the session-file creation and turn-freezing logic with the Enter
/// handler but skips UI-only concerns (history nav, slash completion).
pub(super) fn finish_user_bash(
    app: &mut App,
    result: lofi_core::UserBashResult,
    exclude_from_context: bool,
) {
    if !exclude_from_context {
        if let Err(error) = app.lifecycle.push_message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: result.context_text(),
            }],
        }) {
            app.notify(NotifyKind::Error, format!("update agent history: {error}"));
        }
    }
    // Recording the command is a core-owned session write — the UI hands the
    // finished result to the sink rather than assembling/appending an event.
    let run_model = app.run_model();
    let byte_range = app.session.sink_mut().and_then(|sink| {
        sink.record_user_bash(
            &result,
            exclude_from_context,
            &run_model,
            &app.system_prompt,
        )
        .ok()
    });
    app.session.refresh_cursor();
    app.apply_event(AgentEvent::UserBash {
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

fn install_run(
    app: &mut App,
    current_run: &mut Option<RunHandle>,
    handle: JoinHandle<()>,
    rx: Receiver<AgentEvent>,
    cancel: Arc<AtomicBool>,
    preempt: Arc<AtomicBool>,
    user_bash: Option<(String, bool)>,
) {
    *current_run = Some(RunHandle {
        handle,
        rx,
        cancel,
        preempt,
        user_bash,
    });
    app.run = Some(0);
    app.run_start = Some(Instant::now());
    app.pinned = true;
}

fn spawn_agent_run(
    app: &mut App,
    current_run: &mut Option<RunHandle>,
    agent: &lofi_core::Agent,
    prompt: Option<String>,
) {
    // Session creation/writes are core-owned: ask the sink for the cursor,
    // creating the session file on first use, then mirror it for reads.
    let run_model = app.run_model();
    // Ask the sink for the cursor; on brand-new lineage birth it pins the
    // system prompt as the first event (compact-style boundary write, never
    // detected by scanning). A reused/resumed lineage skips the pin — restore
    // re-reads the system event already on the log.
    let cursor = app
        .session
        .sink_mut()
        .and_then(|sink| sink.cursor_or_create(&run_model, &app.system_prompt).ok());
    // Mirror the lineage-birth pin into the live history so the first request
    // carries the system prompt; resume has already rebuilt history from the
    // log, so seed_system is a no-op there.
    let _ = app.lifecycle.seed_system(&app.system_prompt);
    app.session.refresh_cursor();
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
    let handle = tokio::task::spawn_local(async move {
        let mut messages = history.lock().map(|m| m.clone()).unwrap_or_default();
        let result = agent
            .run_continuation(
                &mut messages,
                prompt,
                tx,
                cursor.as_ref(),
                continuation,
                Some(cancel_clone),
                Some(preempt_clone),
            )
            .await;
        if let Ok(mut stored) = history.lock() {
            *stored = messages;
        }
        if let Err(error) = result {
            let _ = err_tx.send(AgentEvent::Error(error.to_string())).await;
        }
    });
    install_run(app, current_run, handle, rx, cancel, preempt, None);
}

pub(super) fn spawn_user_bash(
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
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let cwd = app.session.cwd.clone();
    let command_for_run = command.clone();
    let handle = tokio::task::spawn_local(async move {
        let event = match Box::pin(lofi_core::run_user_bash(&cwd, command_for_run.clone())).await {
            Ok(result) => AgentEvent::UserBash {
                command: result.command,
                output: result.output,
                exit_code: result.exit_code,
                signal: result.signal,
                duration_ms: result.duration_ms,
                truncated: result.truncated,
                cancelled: result.cancelled,
                exclude_from_context,
            },
            Err(error) => AgentEvent::UserBash {
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
        let _ = tx.send(event).await;
    });
    install_run(
        app,
        current_run,
        handle,
        rx,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Some((command, exclude_from_context)),
    );
}

pub(super) fn spawn_prompt(
    app: &mut App,
    agent: Option<&lofi_core::Agent>,
    current_run: &mut Option<RunHandle>,
    prompt: String,
) {
    if let Some((command, exclude_from_context)) = parse_user_bash(&prompt) {
        spawn_user_bash(app, current_run, command, exclude_from_context);
        return;
    }
    let Some(agent) = agent else {
        app.push_turn(Turn {
            prompt,
            blocks: app
                .no_models_hint
                .clone()
                .map_or_else(Vec::new, |hint| vec![Block::Error(hint)]),
        });
        return;
    };
    spawn_agent_run(app, current_run, agent, Some(prompt));
}

pub(super) fn spawn_continue(
    app: &mut App,
    agent: Option<&lofi_core::Agent>,
    current_run: &mut Option<RunHandle>,
) {
    if let Some(agent) = agent {
        spawn_agent_run(app, current_run, agent, None);
    }
}

fn restore_queued_prompts(app: &mut App) {
    if app.prompt_queue.is_empty() {
        return;
    }
    let queued = std::mem::take(&mut app.prompt_queue).join("\n\n");
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

fn interrupt_run(
    app: &mut App,
    agent: Option<&lofi_core::Agent>,
    current_run: &mut Option<RunHandle>,
) {
    if let Some(r) = current_run.as_mut() {
        if r.user_bash.is_none() {
            // Restore steering/follow-up messages to the editor when a
            // stream is aborted instead of submitting them automatically.
            restore_queued_prompts(app);
        }
        let user_bash = r.user_bash.clone();
        // Agent runs must settle cooperatively: the engine checkpoints each
        // completed round, flushes the partial current response, and writes a
        // TurnCancelled marker before its channel closes. Aborting the task
        // here skips that cleanup and makes cancelled output disappear.
        r.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some((command, exclude_from_context)) = user_bash {
            let Some(r) = current_run.take() else {
                return;
            };
            r.handle.abort();
            let result =
                lofi_core::cancelled_user_bash(command, app.run_elapsed().as_millis() as u64);
            finish_user_bash(app, result, exclude_from_context);
            app.run_finished();
            if let Some(prompt) = app.prompt_queue.first().cloned() {
                app.prompt_queue.remove(0);
                spawn_prompt(app, agent, current_run, prompt);
            }
        }
        app.ctrl_c_at = None;
    }
}

pub(super) fn handle_ctrl_c(
    app: &mut App,
    agent: Option<&lofi_core::Agent>,
    current_run: &mut Option<RunHandle>,
) {
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
        interrupt_run(app, agent, current_run);
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
        KeyCode::Char('[') => app.nav_jump_turn(-1),
        KeyCode::Char(']') => app.nav_jump_turn(1),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        _ => {}
    }
}

pub(super) fn handle_select_key(k: &KeyEvent, app: &mut App) {
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return;
    }
    if apply_motion(k, app) {
        return;
    }
    match k.code {
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
        KeyCode::Char('[') => app.nav_jump_turn(-1),
        KeyCode::Char(']') => app.nav_jump_turn(1),
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
    match m.kind {
        MouseEventKind::ScrollUp if in_log => app.scroll_nav(-3),
        MouseEventKind::ScrollDown if in_log => app.scroll_nav(3),
        MouseEventKind::Down(MouseButton::Left) if can_select => {
            app.sel = None;
            if in_log {
                let cell = log_cell(app, m.row, m.column);
                app.sel = Some(Selection {
                    start: cell,
                    end: cell,
                });
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
            app.mode = Mode::Input;
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
