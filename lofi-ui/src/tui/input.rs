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
        handle_mouse(*m, app);
        return;
    }
    if let Event::Paste(s) = ev {
        // Paste only types into the prompt; ignored in Navigate/Select or
        // while a centered modal is open (the modal owns input then).
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
    // Modal overlays (`/resume` and `/tree`) intercept keys while open:
    // up/down (`↑/↓` or `j`/`k`), Enter to confirm, `Esc`/`q` to cancel.
    if app.handle_info_key(k) {
        return;
    }
    if app.handle_modal_key(k) {
        return;
    }

    // Ctrl+C cancels a run, clears the draft, or quits on double-press in
    // Input. In Navigate/Select it returns to Input and snaps the viewport
    // to the latest transcript line (a run, if active, keeps running — press
    // Ctrl+C again in Input to cancel it).
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        if app.mode != Mode::Input {
            app.enter_input();
            app.pin_to_latest();
            return;
        }
        handle_ctrl_c(app, current_run);
        return;
    }

    // Navigate / Select carry their own keymaps.
    if app.mode != Mode::Input {
        app.last_kill_was_kill = false;
        match app.mode {
            Mode::Navigate => handle_nav_key(k, app),
            Mode::Select => handle_select_key(k, app),
            Mode::Input => {}
        }
        return;
    }

    // --- Input mode ---
    // Any key press clears an active mouse selection (tmux-style).
    app.sel = None;
    // Capture before reset so consecutive `C-k` appends to the kill ring.
    let append_kill = app.last_kill_was_kill;
    app.last_kill_was_kill = false;

    if k.code == KeyCode::Enter && k.modifiers.contains(KeyModifiers::ALT) {
        app.insert_newline();
        app.refresh_slash_complete();
        return;
    }
    // Ctrl+J is a newline in readline / Emacs; treat it like Alt+Enter.
    if k.code == KeyCode::Char('j') && k.modifiers.contains(KeyModifiers::CONTROL) {
        app.insert_newline();
        app.refresh_slash_complete();
        return;
    }
    // Slash-command autocomplete popover intercepts navigation/accept/dismiss
    // keys while active. Typing and other edits fall through to the normal
    // Input handlers and re-filter the popover via `refresh_slash_complete`.
    if app.handle_popover_key(k) {
        return;
    }
    match k.code {
        KeyCode::Enter if current_run.is_none() && !app.input.is_empty() => {
            let prompt = std::mem::take(&mut app.input);
            app.input_cursor = 0;
            app.history_idx = None;
            app.slash_complete = None;
            if app.slash_command(&prompt) {
                return;
            }
            app.history_nav.push(prompt.clone());
            // Lazily create the transcript file on the first persisted prompt.
            if app.session.path.is_none() {
                if let Some(store) = &app.session.store {
                    if let Ok(p) =
                        store.create(&app.session.cwd, &app.session_model())
                    {
                        app.session.path = Some(p);
                    }
                }
            }
            // Freeze the previous turn: if it was committed to the
            // transcript (has a byte range), its blocks are now file-backed
            // (see `materialize_turn`) — drop them so memory stays bounded.
            // UI-only turns (slash commands) have no byte range and keep blocks.
            let prev = app.turns.len();
            if prev > 0
                && app
                    .turn_byte_ranges
                    .get(prev - 1)
                    .is_some_and(Option::is_some)
            {
                app.turns[prev - 1].blocks.clear();
            }
            let Some(agent) = agent else {
                // No model configured: there is no agent to emit `TurnStart`,
                // so push the turn manually and surface the hint on it.
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
            // The new turn is pushed by `AgentEvent::TurnStart` when the
            // engine begins the run — keeping turn creation in one place
            // (the event handler) for both live and resumed sessions.
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let history = Arc::clone(&app.history);
            let session_path = app.session.path.clone();
            let commit = session_path.as_ref().map(|p| SessionCommit {
                path: p.clone(),
                label: app.session_model(),
                parent_hint: app.branch_hint.take(),
            });
            let agent_clone = agent.clone();
            let err_tx = tx.clone();
            let handle = tokio::task::spawn_local(async move {
                let mut messages = history.lock().map(|m| m.clone()).unwrap_or_default();
                // The engine owns the timers and cost, and writes the turn's
                // events (messages + timings + turn-end) to the transcript.
                let result = agent_clone
                    .run_continuation(&mut messages, prompt, tx, commit.as_ref(), false)
                    .await;
                if let Ok(mut g) = history.lock() {
                    *g = messages;
                }
                if let Err(e) = result {
                    let _ = err_tx.send(AgentEvent::Error(e.to_string())).await;
                }
            });
            *current_run = Some(RunHandle { handle, rx });
            app.run = Some(0);
            app.run_start = Some(Instant::now());
            app.pinned = true;
        }
        KeyCode::Backspace if k.modifiers.contains(KeyModifiers::ALT) => app.kill_word_back(),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete_forward_char(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Up if app.slash_complete.is_some() => app.slash_complete_up(),
        KeyCode::Down if app.slash_complete.is_some() => app.slash_complete_down(),
        KeyCode::Up => app.cursor_up(),
        KeyCode::Down => app.cursor_down(),
        KeyCode::Tab => app.enter_nav(),
        KeyCode::Esc => app.clear_input(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        // Emacs / readline navigation.
        KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_line_start(),
        KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_line_end(),
        KeyCode::Char('b') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_left(),
        KeyCode::Char('f') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_right(),
        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => app.cursor_down(),
        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => app.cursor_up(),
        KeyCode::Char('h') if k.modifiers.contains(KeyModifiers::CONTROL) => app.backspace(),
        KeyCode::Char('k') if k.modifiers.contains(KeyModifiers::CONTROL) => app.kill_line_end(append_kill),
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => app.kill_line_start(),
        KeyCode::Char('w') if k.modifiers.contains(KeyModifiers::CONTROL) => app.kill_word_back(),
        KeyCode::Char('y') if k.modifiers.contains(KeyModifiers::CONTROL) => app.yank(),
        KeyCode::Char('b') if k.modifiers.contains(KeyModifiers::ALT) => app.move_word_back(),
        KeyCode::Char('f') if k.modifiers.contains(KeyModifiers::ALT) => app.move_word_fwd(),
        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::ALT) => app.kill_word_fwd(),
        KeyCode::Char('<') if k.modifiers.contains(KeyModifiers::ALT) => app.input_cursor = 0,
        KeyCode::Char('>') if k.modifiers.contains(KeyModifiers::ALT) => app.input_cursor = app.input.len(),
        // Readline `C-d`: delete the char under the cursor, or send EOF (quit)
        // when the input is empty.
        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.input.is_empty() {
                app.should_quit = true;
            } else {
                app.delete_forward_char();
            }
        }
        
        // Insert printable chars unless a control/alt combo is claimed by an
        // arm above; Shift is already folded into `c` so it must be allowed.
        KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            app.insert_char(c);
        }
        _ => {}
    }
    // Re-filter the autocomplete popover after any input edit. Commands that
    // `return` early (newline, autocomplete accept/dismiss) call
    // `refresh_slash_complete` themselves or clear the popover directly.
    app.refresh_slash_complete();
}

/// Ctrl+C: cancel an active run; otherwise clear a non-empty draft, or quit
/// on a double press within [`QUIT_DOUBLE_PRESS`] when the prompt is empty.
/// Mode-independent — works the same in Input, Navigate, and Select.
/// Kick off a silent force-continue after a hard-cap force-compact.
///
/// Mirrors the submit path in [`handle_event`] but appends no user prompt:
/// it seeds the run from the (just-compacted) history, which ends in a tool
/// result, so the model resumes the turn. The engine emits `TurnContinue`,
/// which the UI handles by appending to the current turn rather than pushing
/// a new one. No-op when no model is configured.
pub(super) fn spawn_continue(
    app: &mut App,
    agent: Option<&lofi_core::Agent>,
    current_run: &mut Option<RunHandle>,
) {
    let Some(agent) = agent else { return };
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let history = Arc::clone(&app.history);
    let session_path = app.session.path.clone();
    // Chain off the active leaf (the Compaction marker compact_now just
    // appended) — no branch_hint, so the recorder appends linearly.
    let commit = session_path.map(|p| SessionCommit {
        path: p,
        label: app.session_model(),
        parent_hint: None,
    });
    let agent_clone = agent.clone();
    let err_tx = tx.clone();
    let handle = tokio::task::spawn_local(async move {
        let mut messages = history.lock().map(|m| m.clone()).unwrap_or_default();
        let result = agent_clone.run_continue(&mut messages, tx, commit.as_ref()).await;
        if let Ok(mut g) = history.lock() {
            *g = messages;
        }
        if let Err(e) = result {
            let _ = err_tx.send(AgentEvent::Error(e.to_string())).await;
        }
    });
    *current_run = Some(RunHandle { handle, rx });
    app.run = Some(0);
    app.run_start = Some(Instant::now());
    app.pinned = true;
}

pub(super) fn handle_ctrl_c(app: &mut App, current_run: &mut Option<RunHandle>) {
    if let Some(r) = current_run.take() {
        r.handle.abort();
        if let Some(turn) = app.turns.last_mut() {
            turn.blocks.push(Block::Error("cancelled".to_string()));
        }
        app.run_finished();
        app.ctrl_c_at = None;
        return;
    }
    if app.input.is_empty() {
        let now = Instant::now();
        if app.ctrl_c_at.is_some_and(|t| now.duration_since(t) < QUIT_DOUBLE_PRESS) {
            app.should_quit = true;
        } else {
            app.ctrl_c_at = Some(now);
        }
    } else {
        app.clear_input();
        app.ctrl_c_at = None;
    }
}

/// Vim-style word/WORD motion. `big` distinguishes `w`/`b`/`e` (words are
/// runs of word-chars or punctuation) from `W`/`B`/`E` (runs of non-space).
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

/// Navigate keymap: j/k and arrows move the cursor, g/G jump to top/bottom, v
/// enters Select, i/Tab return to Input. Control combos (other than the
/// globally-handled C-c) are ignored.
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

/// Select keymap: movement extends the selection, y/Enter yanks it,
/// Tab/Esc drops the selection and returns to Navigate.
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
    // Mouse drag-selection is an Input-mode convenience; Navigate/Select use
    // the keyboard cursor. The wheel scrolls in every mode.
    let can_select = app.mode == Mode::Input;
    match m.kind {
        MouseEventKind::ScrollUp if in_log => app.scroll_nav(-3),
        MouseEventKind::ScrollDown if in_log => app.scroll_nav(3),
        MouseEventKind::Down(MouseButton::Left) if can_select => {
            app.sel = None;
            if in_log {
                let cell = log_cell(app, m.row, m.column);
                app.sel = Some(Selection { start: cell, end: cell });
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if in_log && can_select => {
            let cell = log_cell(app, m.row, m.column);
            if let Some(sel) = app.sel.as_mut() {
                sel.end = cell;
            }
        }
        MouseEventKind::Up(MouseButton::Left) if can_select => app.yank_selection(),
        _ => {}
    }
}

/// Map a screen cell inside the log viewport to (select-line index, char
/// index) in `log_lines`, using display width so wide chars land correctly.
/// The char index is clamped to the line's content range so a press/drag in
/// the gutter or padding snaps to the content edge — the selection never
/// starts or ends in the decorative whitespace.
pub(super) fn log_cell(app: &App, row: u16, column: u16) -> (usize, usize) {
    let rel_y = row.saturating_sub(app.log_rect.y) as usize;
    let rel_x = column.saturating_sub(app.log_rect.x) as usize;
    let line_idx = app.log_off.saturating_add(rel_y);
    // `log_lines` is the visible window only (window-relative); index by `rel_y`.
    let line = app.log_lines.get(rel_y);
    let col = line.map_or(rel_x, |s| {
        let (cstart, cend) = app.log_content.get(rel_y).copied().unwrap_or((0, s.chars().count()));
        col_to_char_idx(s, rel_x).clamp(cstart, cend)
    });
    (line_idx, col)
}
