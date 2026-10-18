#[allow(clippy::wildcard_imports)]
use super::*;

/// Cancel an in-flight run and wait for its recorder to flush. Aborting the
/// task skips that flush, so the deadline sits far beyond any healthy
/// round; the engine's cancel flag and idle timeout bound the wait on
/// their own. Returns true when aborted by deadline — quitting must not
/// silently lose the un-flushed suffix.
pub(crate) async fn settle_run_for_quit(run: RunHandle, timeout: Duration) -> bool {
    run.cancel.store(true, Ordering::Relaxed);
    let RunHandle { handle, mut rx, .. } = run;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut aborted_by_deadline = false;
    loop {
        tokio::select! {
            ev = rx.recv() => {
                if ev.is_none() {
                    break;
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                aborted_by_deadline = true;
                break;
            },
        }
    }
    if aborted_by_deadline {
        handle.abort();
    } else {
        let _ = handle.await;
    }
    aborted_by_deadline
}

pub(crate) struct RunHandle {
    pub(crate) handle: JoinHandle<()>,
    pub(crate) rx: Receiver<AgentEvent>,
    pub(crate) cancel: Arc<AtomicBool>,
    /// When set, the agent exits its round loop gracefully after the current
    /// round finishes (tool results in hand) so the TUI can pop the next
    /// queued prompt at the earliest opportunity — between rounds, not after
    /// the entire multi-round turn.
    pub(crate) preempt: Arc<AtomicBool>,
    pub(crate) session_cursor: Arc<std::sync::Mutex<Option<store::SessionCursor>>>,
    pub(crate) user_shell: Option<(String, bool)>,
}

pub(crate) fn attach_run_cursor(app: &mut App, run: &RunHandle) {
    if app.session.cursor.is_some() {
        return;
    }
    let cursor = run
        .session_cursor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(cursor) = cursor {
        app.session.attach_cursor(cursor);
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct RenderProfile {
    pub(crate) resize_events: usize,
    pub(crate) resize_batch_us: u128,
    pub(crate) resize_quiet_us: u128,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) turns: usize,
    pub(crate) frozen_turns: usize,
    pub(crate) collapsed_cache_hits: usize,
    pub(crate) collapsed_cache_misses: usize,
    pub(crate) collapsed_cache_entries: usize,
    pub(crate) collapsed_cache_bytes: usize,
    pub(crate) materialize_us: u128,
    pub(crate) width_changed: bool,
    pub(crate) ensure_frozen_us: u128,
    pub(crate) live_height_us: u128,
    pub(crate) viewport_cache_us: u128,
    pub(crate) frozen_window_us: u128,
    pub(crate) live_window_us: u128,
    pub(crate) log_total_us: u128,
}

#[derive(Debug, Default)]
pub(crate) struct ResizeState {
    pub(crate) deadline: Option<tokio::time::Instant>,
    pub(crate) started: Option<Instant>,
    pub(crate) last: Option<Instant>,
    pub(crate) events: usize,
}

pub(crate) struct TerminalGuard {
    pub(crate) terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    pub(crate) fn draw(
        &mut self,
        app: &mut App,
        resize_events: usize,
        resize_started: Option<Instant>,
        resize_last: Option<Instant>,
    ) -> Result<()> {
        app.transcript
            .collapsed_turns
            .get_mut()
            .reset_frame_profile();
        *app.render_profile = RenderProfile {
            resize_events,
            resize_batch_us: resize_started.map_or(0, |at| at.elapsed().as_micros()),
            resize_quiet_us: resize_last.map_or(0, |at| at.elapsed().as_micros()),
            ..RenderProfile::default()
        };
        let draw_started = Instant::now();
        let mut render_us = 0;
        let frame = self
            .terminal
            .draw(|f| {
                let render_started = Instant::now();
                view::render(f, app);
                render_us = render_started.elapsed().as_micros();
            })
            .map_err(Error::Io)?;
        let draw_us = draw_started.elapsed().as_micros();
        {
            let cache = app.transcript.collapsed_turns.get_mut();
            app.render_profile.collapsed_cache_hits = cache.frame_hits;
            app.render_profile.collapsed_cache_misses = cache.frame_misses;
            app.render_profile.collapsed_cache_entries = cache.map.len();
            app.render_profile.collapsed_cache_bytes = cache.retained_bytes;
            app.render_profile.materialize_us = cache.frame_materialize_us;
        }
        app.debug_render_timing(frame.area.width, frame.area.height, draw_us, render_us);
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        tty_events::set_reports_enabled(false);
        let _ = disable_raw_mode();
    }
}

/// `agent` is `None` when no model is configured: the UI still launches and
/// shows `no_models_hint` in the log; submitting a prompt re-surfaces the
/// hint instead of running.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    agent: Option<Backend>,
    model_label: String,
    thinking: ThinkingLevel,
    service_tier: ServiceTier,
    ui_theme: lofi_types::ThemeMode,
    session: SessionConfig,
    no_models_hint: Option<String>,
    ctx_limit: u64,
    compaction: lofi_types::CompactionConfig,
    switcher: Option<ModelSwitcher>,
    system_prompt: String,
) -> Result<()> {
    tty_events::ensure_terminal_input().map_err(Error::Io)?;
    enable_raw_mode().map_err(Error::Io)?;
    // `Theme::resolve(Auto)` probes via OSC 11; that requires raw mode
    // (see terminal_bg module doc for why).
    let theme = Theme::resolve(ui_theme);
    let setup = (|| -> std::io::Result<_> {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableFocusChange,
            EnableBracketedPaste
        )?;
        let backend = CrosstermBackend::new(stdout);
        Terminal::new(backend)
    })();
    let terminal = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            let _ = disable_raw_mode();
            return Err(Error::Io(e));
        }
    };

    let mut guard = TerminalGuard { terminal };
    let local = LocalSet::new();
    let result = std::panic::AssertUnwindSafe(local.run_until(async move {
        // `run_loop` exceeds clippy's large_futures stack limit.
        Box::pin(run_loop(
            &mut guard,
            agent,
            theme,
            ui_theme,
            model_label,
            thinking,
            service_tier,
            session,
            no_models_hint,
            ctx_limit,
            compaction,
            switcher,
            system_prompt,
        ))
        .await
    }))
    .catch_unwind()
    .await;
    match result {
        Ok(result) => result,
        Err(payload) => Err(Error::State(format!(
            "TUI panicked: {}",
            panic_message(payload.as_ref())
        ))),
    }
}

pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn run_loop(
    guard: &mut TerminalGuard,
    mut agent: Option<Backend>,
    theme: Theme,
    theme_mode: lofi_types::ThemeMode,
    model_label: String,
    thinking: ThinkingLevel,
    service_tier: ServiceTier,
    session: SessionConfig,
    no_models_hint: Option<String>,
    ctx_limit: u64,
    compaction: lofi_types::CompactionConfig,
    switcher: Option<ModelSwitcher>,
    system_prompt: String,
) -> Result<()> {
    let SessionConfig {
        sink,
        cursor,
        index,
        file_size,
        history_start,
        contiguous,
        cwd,
    } = session;
    let model_choices = switcher.as_ref().map_or_else(
        || agent.as_ref().map_or_else(Vec::new, Backend::model_choices),
        |s| s.choices().to_vec(),
    );
    let mut app = App::new(
        model_label,
        thinking,
        service_tier,
        ctx_limit,
        compaction,
        system_prompt,
    );
    app.external_backend = agent
        .as_ref()
        .is_some_and(|backend| backend.native().is_none());
    if app.external_backend {
        app.selected_external_model_label = agent.as_ref().map(Backend::model_label);
    }
    app.theme = theme;
    app.theme_mode = theme_mode;
    app.model_choices = model_choices;
    app.session = SessionState { sink, cursor, cwd };
    if let Some(cursor) = app.session.cursor.clone() {
        app.restore_indexed_session(&cursor, &index, file_size, history_start, contiguous)?;
        // Jobs whose started marker is on this lineage but whose terminal
        // marker is not (process died, or user /tree'd a fresh branch
        // elsewhere). Their ids are stale; surface that on the first agent
        // turn after resume so the model does not try to poll them. Reuses
        // the already-built lineage index — no second file scan.
        let outstanding =
            lofi_core::session::replay::outstanding_job_ids_at(&cursor, &index).unwrap_or_default();
        if !outstanding.is_empty() {
            let ids = outstanding
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            app.startup_notices.push(format!(
                "session resumed: jobs [{ids}] from the previous run are no longer running; their ids are stale. Use jobSpawn for new background work."
            ));
        }
    }
    // The resume index is startup scratch. All persistent UI backing uses the
    // compact turn ranges/offsets built above and opens a fresh cursor snapshot
    // only for branch operations. Explicitly drop it before the event loop so
    // an async state-machine frame cannot retain thousands of ID strings.
    drop(index);
    lofi_core::release_freed_memory();
    // Resume replay creates file-backed shells for every historical turn,
    // including the final one. Viewport materialization owns the bounded
    // display working set from this point onward.
    app.bump_render_epoch();
    app.no_models_hint = no_models_hint.clone();
    if let Some(hint) = no_models_hint {
        // Lead with the hint so the user sees why nothing will run.
        app.transcript.insert_turn(
            0,
            Turn {
                prompt: String::new(),
                kind: lofi_types::PromptKind::User,
                blocks: vec![Block::Error(hint)],
            },
        );
    }

    app.enable_debug_from_env();
    if app.debug.is_some() {
        app.debug_after_draw = Some("initial_draw");
    }

    let (confirm_tx, mut confirm_rx) =
        tokio::sync::mpsc::unbounded_channel::<lofi_core::ConfirmRequest>();
    if let Some(a) = agent.take() {
        let a = a.with_confirm_tx(confirm_tx.clone());
        // Clone the session's job registry for the `/job` modal and the
        // running-jobs badge. Cheap: shares the agent's map.
        app.jobs = Some(a.jobs());
        app.subagents = a.subagents();
        // The `/policy` dialog writes the choice here; every exec in the
        // session reads it. Cloned (not moved) so model switches keep it.
        app.policy_override = Some(a.policy_override().clone());
        app.auto_mode_configured = a.has_auto_mode();
        agent = Some(a);
    }

    // Live notice feed. The job driver pushes onto this the moment a job
    // transitions; the select arm below reacts without waiting for a tick.
    // Buffered pre-UI notices flush into this receiver on subscribe.
    let mut job_notice_rx = app
        .jobs
        .as_ref()
        .map(lofi_core::JobRegistry::subscribe_notices);
    // The startup registry instance backs this receiver; a backend switch
    // that swaps the instance re-subscribes at the switch site.
    let mut subagent_notice_rx = app
        .subagents
        .as_ref()
        .map(lofi_core::SubagentRegistry::subscribe_notices);

    let (picker_load_tx, mut picker_load_rx) = tokio::sync::mpsc::unbounded_channel();
    app.picker_load_tx = Some(picker_load_tx);
    let (lifecycle_tx, mut lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
    app.lifecycle_tx = Some(lifecycle_tx);
    let mut current_run: Option<RunHandle> = None;
    let mut events = tty_events::TtyEvents::start().map_err(Error::Io)?;
    app.sync_color_scheme_reports();
    if app.theme_mode == lofi_types::ThemeMode::Auto {
        tty_events::request_color_scheme();
    }
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            .map_err(Error::Io)?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(Error::Io)?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(Error::Io)?;
    let mut last_err: Option<String> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut resize = Box::<ResizeState>::default();
    loop {
        if dirty && resize.deadline.is_none() {
            guard.draw(&mut app, resize.events, resize.started, resize.last)?;
            if resize.deadline.is_none() {
                resize.started = None;
                resize.last = None;
                resize.events = 0;
            }
            if let Some(event) = app.debug_after_draw.take() {
                app.debug_sample(event);
            }
            dirty = if let Some(alert) = app.take_transcript_alert() {
                app.notify(NotifyKind::Error, alert);
                true
            } else {
                false
            };
        }

        tokio::select! {
            ev = async {
                match &mut current_run {
                    Some(r) => r.rx.recv().await,
                    None => std::future::pending::<Option<AgentEvent>>().await,
                }
            } => {
                match ev {
                    Some(e) => {
                        if let Some(run) = current_run.as_ref() {
                            attach_run_cursor(&mut app, run);
                        }
                        let mut event = Some(e);
                        for index in 0..64 {
                            let Some(e) = event.take() else { break };
                            if let AgentEvent::UserShell {
                                command, output, exit_code, signal, duration_ms,
                                truncated, cancelled, exclude_from_context,
                            } = e
                            {
                                let result = lofi_core::UserShellResult::from_session(
                                    command, output, exit_code, signal, duration_ms,
                                    truncated, cancelled,
                                );
                                finish_user_shell(&mut app, result, exclude_from_context);
                            } else {
                                app.apply_event(e);
                            }
                            if index < 63 {
                                event = current_run
                                    .as_mut()
                                    .and_then(|run| run.rx.try_recv().ok());
                            }
                        }
                        // If there's a queued prompt, signal the agent to
                        // exit its round loop after the current round so the
                        // queued prompt is sent at the earliest opportunity.
                        // The flag is only checked between rounds (after tool
                        // results are in hand), so setting it during streaming
                        // or tool execution is safe.
                        if !app.prompt_queue.is_empty() {
                            if let Some(r) = &current_run {
                                r.preempt.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                    None => {
                        if let Some(r) = current_run.take() {
                            attach_run_cursor(&mut app, &r);
                            let was_user_shell = r.user_shell.is_some();
                            if let Err(error) = r.handle.await {
                                app.apply_event(AgentEvent::Error(format!(
                                    "run task failed: {error}"
                                )));
                            }
                            app.run_finished();
                            app.debug_sample(if was_user_shell { "user_shell_settled" } else { "agent_settled" });
                            if !app.should_quit {
                                if was_user_shell {
                                    if let Some(queued) = app.prompt_queue.first().cloned() {
                                        app.prompt_queue.remove(0);
                                        spawn_prompt(
                                            &mut app,
                                            agent.as_ref(),
                                            &mut current_run,
                                            queued.text,
                                            queued.kind,
                                        );
                                    }
                                } else if app.context_pressure {
                                    // Core owns hard-cap eligibility and
                                    // compaction; the UI only presents its outcome.
                                    app.context_pressure = false;
                                    app.continue_after_hard_compact = true;
                                    if let Some(outcome) = app.hard_compact() {
                                        app.continue_after_hard_compact = false;
                                        match outcome {
                                            HardCompactOutcome::Compacted(_) => {
                                                spawn_continue(
                                                    &mut app,
                                                    agent.as_ref(),
                                                    &mut current_run,
                                                );
                                            }
                                            HardCompactOutcome::Cooldown => app.notify(
                                                NotifyKind::Warn,
                                                "context exceeded the hard cap too soon after a compaction; cannot continue",
                                            ),
                                            HardCompactOutcome::NotEnoughHistory => app.notify(
                                                NotifyKind::Warn,
                                                "could not compact at the hard cap; cannot continue",
                                            ),
                                        }
                                    }
                                } else {
                                    app.maybe_auto_compact();
                                    if !app.lifecycle_busy && app.run.is_none() {
                                        if let Some(queued) = app.prompt_queue.first().cloned() {
                                            app.prompt_queue.remove(0);
                                            spawn_prompt(
                                                &mut app,
                                                agent.as_ref(),
                                                &mut current_run,
                                                queued.text,
                                                queued.kind,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                dirty = true;
            }
            _ = sigterm.recv() => {
                request_quit(&mut app, &mut current_run);
            }
            _ = sighup.recv() => {
                request_quit(&mut app, &mut current_run);
            }
            _ = sigwinch.recv() => {
                if let Ok((w, h)) = crossterm::terminal::size() {
                    handle_event(
                        &Event::Resize(w, h),
                        &mut app,
                        agent.as_ref(),
                        &mut current_run,
                    );
                    let now = Instant::now();
                    if resize.deadline.is_none() {
                        resize.started = Some(now);
                        resize.events = 0;
                    }
                    resize.events = resize.events.saturating_add(1);
                    resize.last = Some(now);
                    resize.deadline = Some(
                        tokio::time::Instant::now()
                            + Duration::from_millis(RESIZE_DEBOUNCE_MS),
                    );
                }
            }
            maybe_ev = events.recv() => {
                let defer_redraw;
                match maybe_ev {
                    Some(Ok(tty_events::TuiEvent::ColorScheme(scheme))) => {
                        defer_redraw = false;
                        if app.apply_color_scheme(scheme) {
                            dirty = true;
                        }
                    }
                    Some(Ok(tty_events::TuiEvent::Input(ev))) => {
                        defer_redraw = matches!(ev, Event::Resize(_, _));
                        if !app.should_quit {
                            handle_event(&ev, &mut app, agent.as_ref(), &mut current_run);
                        }
                    if let Some(q) = app.pending_model_switch.take() {
                        let target_external = q.split_once('/').is_some_and(|(name, _)| {
                            switcher.as_ref().is_some_and(|s| s.config.backends.contains_key(name))
                        });
                        let same_external = app.external_backend && q.split_once('/').is_some_and(|(name, _)| {
                            agent.as_ref().is_some_and(|a| a.model_label().starts_with(&format!("{name}/")))
                        });
                        if same_external {
                            if let Some(backend) = agent.as_mut() {
                                match backend.select_model(&q) {
                                    Ok(()) => {
                                        app.model_label = backend.model_label();
                                        app.selected_external_model_label = Some(app.model_label.clone());
                                        app.notify(NotifyKind::Info, format!("selected {} for the next turn", app.model_label));
                                        app.bump_render_epoch();
                                    }
                                    Err(e) => app.notify(NotifyKind::Error, format!("switch model: {e}")),
                                }
                            }
                        } else if let Some(switcher) = switcher.as_ref() {
                            match switcher.rebuild_backend(agent.as_ref(), &q).await {
                                Ok((new_agent, model, level)) => {
                                    app.external_backend = target_external;
                                    app.backend_context = None;
                                    if let Some(model) = model {
                                        let tier = model.service_tier.clone();
                                        app.selected_external_model_label = None;
                                        app.apply_model_switch(&model, level, tier);
                                    } else {
                                        app.model_label = new_agent.model_label();
                                        app.selected_external_model_label = Some(app.model_label.clone());
                                        app.thinking = ThinkingLevel::Off;
                                        app.thinking_label = None;
                                        app.service_tier = ServiceTier::Auto;
                                        app.service_label = None;
                                        app.notify(NotifyKind::Info, format!("selected {} for the next turn", app.model_label));
                                    }
                                    let new_agent = new_agent.with_confirm_tx(confirm_tx.clone());
                                    app.no_models_hint = None;
                                    app.system_prompt = new_agent.system_prompt().into();
                                    app.jobs = Some(new_agent.jobs());
                                    let new_subagents = new_agent.subagents();
                                    // A switch that swaps the registry
                                    // instance orphans the old one's runs:
                                    // settle their spend and cancel them,
                                    // then listen to the new instance. A
                                    // switch that keeps it must not cancel
                                    // its live runs.
                                    let subagents_changed = match (&app.subagents, &new_subagents)
                                    {
                                        (Some(old), Some(new)) => !old.same_instance(new),
                                        (old, new) => old.is_some() != new.is_some(),
                                    };
                                    if subagents_changed {
                                        app.record_subagent_spend();
                                        app.clear_subagents();
                                    }
                                    app.subagents = new_subagents;
                                    if subagents_changed {
                                        subagent_notice_rx = app
                                            .subagents
                                            .as_ref()
                                            .map(lofi_core::SubagentRegistry::subscribe_notices);
                                    }
                                    app.policy_override = Some(new_agent.policy_override());
                                    app.auto_mode_configured = new_agent.has_auto_mode();
                                    app.jobs_receiver_stale = true;
                                    app.notified_jobs.clear();
                                    agent = Some(new_agent);
                                    app.bump_render_epoch();
                                }
                                Err(e) => app.notify(NotifyKind::Error, format!("switch model: {e}")),
                            }
                        } else {
                            app.notify(NotifyKind::Error, "switch model: no model registry");
                        }
                    }
                    if app.jobs_receiver_stale {
                        job_notice_rx = app
                            .jobs
                            .as_ref()
                            .map(lofi_core::JobRegistry::subscribe_notices);
                        app.jobs_receiver_stale = false;
                    }
                    // Slash commands (notably /tree reconcile) can push
                    // notices into prompt_queue while at rest. The queue's
                    // usual consumer is the agent-finished branch of the
                    // current_run select arm; without a live run, that arm
                    // never fires and the notice would sit forever. Drain
                    // at rest here, matching the mpsc notice path.
                    if !app.should_quit
                        && !app.busy()
                        && !app.prompt_queue.is_empty()
                        && agent.is_some()
                    {
                        if let Some(queued) = app.prompt_queue.first().cloned() {
                            app.prompt_queue.remove(0);
                            spawn_prompt(
                                &mut app,
                                agent.as_ref(),
                                &mut current_run,
                                queued.text,
                                queued.kind,
                            );
                        }
                    }
                    }
                    Some(Err(e)) => {
                        last_err = Some(format!("input read failed: {e}"));
                        break;
                    }
                    None => {
                        last_err = Some("input stream ended".to_string());
                        break;
                    }
                }
                if defer_redraw {
                    // Reflowing a large resumed transcript can take hundreds of
                    // milliseconds. Debounce the whole resize burst so one
                    // expensive leading draw cannot block event consumption and
                    // split a single drag into repeated one-event batches.
                    let now = Instant::now();
                    if resize.deadline.is_none() {
                        resize.started = Some(now);
                        resize.events = 0;
                    }
                    resize.events = resize.events.saturating_add(1);
                    resize.last = Some(now);
                    resize.deadline = Some(
                        tokio::time::Instant::now()
                            + Duration::from_millis(RESIZE_DEBOUNCE_MS),
                    );
                } else {
                    // Explicit user input should never wait behind resize UI
                    // policy. Treat it as the end of the current resize burst.
                    resize.deadline = None;
                    dirty = true;
                }
            }
            () = async {
                match resize.deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                resize.deadline = None;
                dirty = true;
            }
            _ = tick.tick() => {
                if app.refresh_confirmations() || !app.pending_confirms.is_empty() {
                    dirty = true;
                }
                if app.run.is_some() {
                    if let Some(s) = app.run.as_mut() {
                        *s = s.wrapping_add(1);
                    }
                    dirty = true;
                }
                if app.retry.is_some() || app.has_running_jobs() {
                    dirty = true;
                }
                if app
                    .jobs_modal
                    .as_ref()
                    .is_some_and(|modal| modal.viewing.is_some())
                {
                    dirty = true;
                }
                if let Some(t) = app.yank_notify {
                    if t.elapsed() >= YANK_NOTIFY {
                        app.yank_notify = None;
                    }
                    dirty = true;
                }
                if let Some(t) = app.ctrl_c_at {
                    if t.elapsed() >= QUIT_DOUBLE_PRESS {
                        app.ctrl_c_at = None;
                    }
                    dirty = true;
                }
                if let Some(n) = app.notify.as_ref() {
                    if n.at.elapsed() >= NOTIFY_TTL {
                        app.notify = None;
                    }
                    dirty = true;
                }
                // Drain pending height re-measure a little each tick so a
                // width change or estimate-seeded resume never stalls a
                // frame. Absolute line indices shift while the estimates
                // converge, so a Navigate/Select cursor must follow its
                // content through each step instead of drifting (the cursor
                // visibly jumps around while the user scrolls otherwise).
                let navigating = matches!(app.mode, Mode::Navigate | Mode::Select);
                if navigating && app.layout.height_remeasure_from.is_some() {
                    let nav_anchor = app.nav_content_anchor();
                    let sel_anchor = match app.mode {
                        Mode::Select => app.sel_content_anchor(),
                        _ => None,
                    };
                    app.remeasure_heights_step(16);
                    if let Some(anchor) = nav_anchor {
                        // The converging geometry is frozen-turn-only at
                        // rest, so an empty live last turn renders correctly.
                        app.reseat_nav_cursor(anchor, &[], app.layout.frozen_width);
                    }
                    if let Some(anchor) = sel_anchor {
                        app.reseat_sel_anchor(anchor, &[], app.layout.frozen_width);
                    }
                    app.nav_show_cursor();
                    dirty = true;
                } else if app.remeasure_heights_step(16) {
                    dirty = true;
                }
            }
            notice = async {
                match job_notice_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(notice) = notice {
                    // Settle the job line in the same arm that queues its
                    // notice so both change in one redraw.
                    if notice.terminal {
                        app.notified_jobs.insert(notice.id);
                    }
                    let queued = QueuedPrompt {
                        text: notice.text,
                        kind: lofi_types::PromptKind::Notice,
                    };
                    if app.busy() {
                        app.prompt_queue.push(queued);
                        if let Some(r) = &current_run {
                            r.preempt.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    } else if agent.is_some() {
                        spawn_prompt(
                            &mut app,
                            agent.as_ref(),
                            &mut current_run,
                            queued.text,
                            queued.kind,
                        );
                    }
                    dirty = true;
                }
            }
            notice = async {
                match subagent_notice_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                // Lagging skips ahead; Closed means the registry is
                // gone, which outlives the app only in tests.
                if let Ok(notice) = notice {
                    let queued = QueuedPrompt {
                        text: notice.text,
                        kind: lofi_types::PromptKind::Notice,
                    };
                    if app.busy() {
                        app.prompt_queue.push(queued);
                        if let Some(r) = &current_run {
                            r.preempt.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    } else if agent.is_some() {
                        spawn_prompt(
                            &mut app,
                            agent.as_ref(),
                            &mut current_run,
                            queued.text,
                            queued.kind,
                        );
                    }
                    dirty = true;
                }
            }
            picker_load = picker_load_rx.recv() => {
                if let Some(load) = picker_load {
                    app.apply_picker_load(load);
                    dirty = true;
                }
            }
            lifecycle_result = lifecycle_rx.recv() => {
                if let Some(result) = lifecycle_result {
                    let hard_compacted = app.apply_lifecycle_result(result);
                    let continue_run =
                        std::mem::take(&mut app.continue_after_hard_compact) && hard_compacted;
                    if continue_run {
                        spawn_continue(&mut app, agent.as_ref(), &mut current_run);
                    } else if current_run.is_none() {
                        if let Some(queued) = app.prompt_queue.first().cloned() {
                            app.prompt_queue.remove(0);
                            spawn_prompt(
                                &mut app,
                                agent.as_ref(),
                                &mut current_run,
                                queued.text,
                                queued.kind,
                            );
                        }
                    }
                    dirty = true;
                }
            }
            req = confirm_rx.recv() => {
                if let Some(req) = req {
                    if req.active.load(std::sync::atomic::Ordering::Relaxed) {
                        app.queue_confirmation(req);
                        dirty = true;
                    }
                }
            }
        }

        if app.should_quit {
            if let Some(r) = current_run.take() {
                if settle_run_for_quit(r, QUIT_FLUSH_TIMEOUT).await {
                    last_err =
                        Some("quit: run aborted before its final transcript write".to_string());
                }
            }
            // Record subagent spend that settled after the last turn flush;
            // the registry is dropped with the process after this.
            app.record_subagent_spend();
            // Queued prompts were never sent; quit must not silently drop
            // typed input.
            persist_unsent_prompts(&mut app);
            break;
        }
    }
    if let Some(jobs) = &app.jobs {
        jobs.shutdown();
    }
    if let Some(subagents) = &app.subagents {
        // Cancel and abort any surviving background runs; their residue
        // spend is dropped with the process.
        let _ = subagents.clear();
    }
    if let Some(msg) = last_err {
        return Err(Error::Io(std::io::Error::other(msg)));
    }
    Ok(())
}
