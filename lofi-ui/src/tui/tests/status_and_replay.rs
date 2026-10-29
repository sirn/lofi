use super::*;

#[test]
fn turn_failed_wraps_error_below_header() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    a.transcript.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![Block::TurnFailed {
            label: "openai/gpt-4o · medium".to_string(),
            elapsed: Duration::from_secs(12),
            error: "the provider returned a 503 with a very long service-unavailable message that must wrap".to_string(),
        }],
    });
    let turn = &a.transcript.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 40,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    // Line 0 is the status header: model, level, duration only — the error
    // must not appear inline there (it used to, and got clipped).
    let header: String = rls[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        header.contains("Failed in"),
        "header missing status: {header}"
    );
    assert!(
        header.contains("openai/gpt-4o"),
        "header missing label: {header}"
    );
    assert!(
        !header.contains("503"),
        "header must not carry the error inline: {header}"
    );
    let body: String = rls[1..]
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        body.contains("503"),
        "wrapped error missing from body: {body}"
    );
    for rl in &rls[1..] {
        assert!(
            rl.line.width() <= 40,
            "body line overflows: {}",
            rl.line.width()
        );
    }
}

#[test]
fn turn_failed_dedups_after_fatal_error_block() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    let msg = "stream interrupted by upstream gateway";
    a.transcript.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![
            Block::Error(msg.to_string()),
            Block::TurnFailed {
                label: "openai/gpt-4o · medium".to_string(),
                elapsed: Duration::from_secs(3),
                error: format!("provider error: {msg}"),
            },
        ],
    });
    let turn = &a.transcript.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    // The provider emits the error twice on a stream failure — as a fatal
    // `✗` line and again in the TurnFailed marker. The dedup must collapse
    // them so the message appears exactly once across the rendered turn.
    let all: String = rls
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(
        all.matches("stream interrupted").count(),
        1,
        "dedup failed: {all}"
    );
}

#[test]
fn exec_result_wraps_long_lines_instead_of_truncating() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "lofi.bash('echo t')".to_string(),
        label: Some("echo".to_string()),
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 0,
        name: "bash".to_string(),
        args: "echo t".to_string(),
    });
    let token = "abcdefghijklmnopqrstuvwxyz1234567890abcdefghij";
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 0,
        result: serde_json::json!({ "ok": true, "output": token, "code": 0 }).to_string(),
        is_error: false,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    a.expanded_details.insert(
        DetailKey::NativeTool { parent: 0, id: 0 },
        DetailState {
            turn: 0,
            scroll: None,
        },
    );
    let turn = &a.transcript.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 28,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    for rl in &rls {
        assert!(
            rl.line.width() <= 28,
            "row overflows 28: {}",
            rl.line.width()
        );
    }
    let all = rls
        .iter()
        .map(|rl| {
            rl.line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        all.contains("hij"),
        "result tail clipped (no wrapping): {all}"
    );
}

#[test]
fn footer_shows_model_and_thinking() {
    let a = App::new(
        "openai/gpt-5.6-sol".to_string(),
        ThinkingLevel::XHigh,
        ServiceTier::Auto,
        0,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    let r: String = a
        .render_footer_right()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(r.contains("gpt-5.6-sol"));
    assert!(r.contains(":xhigh"));
}

#[test]
fn footer_hides_thinking_when_off() {
    let a = App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Off,
        ServiceTier::Auto,
        0,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    let r: String = a
        .render_footer_right()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert_eq!(r, "openai/gpt-4o");
}

#[test]
fn footer_shows_ctx_after_usage() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.0,
        usage: Usage {
            input_tokens: 40_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        stop_reason: None,
    });
    let r: String = a
        .render_footer_left(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(r.contains("context 40k/200k"), "footer: {r}");
}

#[test]
fn external_display_model_stays_separate_from_response_and_selection() {
    let mut a = app();
    a.model_label = "claude/sonnet".into();
    a.selected_model_label = Some(a.model_label.clone());
    a.thinking_label = None;
    a.run_model_label = Some(a.model_label.clone());
    for id in ["claude-sonnet-test", "claude-opus-test"] {
        a.apply_event(AgentEvent::ModelReported {
            id: id.into(),
            display: "claude-sonnet-test[1m]".into(),
        });
        let header = a
            .render_header_line(120)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(header.contains("claude/claude-sonnet-test[1m]"));
        assert_eq!(a.run_label(), format!("claude/{id}"));
    }
    assert_eq!(a.selected_model_label.as_deref(), Some("claude/sonnet"));
}

#[test]
fn model_report_keeps_the_run_effort_and_service_tier() {
    let mut a = app();
    a.thinking_label = Some(":high".into());
    a.service_label = Some("@flex".into());
    a.run_model_label = Some(a.session_model());
    a.apply_event(AgentEvent::ModelReported {
        id: "gpt-4o-2024".into(),
        display: "gpt-4o-2024".into(),
    });
    assert_eq!(a.run_label(), "openai/gpt-4o-2024:high@flex");
}

#[test]
fn external_reports_do_not_replace_a_mid_run_model_selection() {
    let mut a = app();
    a.model_label = "claude/sonnet".into();
    a.selected_model_label = Some(a.model_label.clone());
    a.run_selected_model_label = Some(a.model_label.clone());
    a.run_model_label = Some(a.model_label.clone());
    for selected in ["claude/opus", "openai/gpt-4o"] {
        a.model_label = selected.into();
        a.selected_model_label = selected.starts_with("claude/").then(|| selected.into());
        a.apply_event(AgentEvent::ModelReported {
            id: "claude-sonnet-test".into(),
            display: "claude-sonnet-test[1m]".into(),
        });
        assert_eq!(a.model_label, selected);
        assert_eq!(a.run_label(), "claude/claude-sonnet-test:medium");
    }
}

#[test]
fn external_context_is_separate_from_turn_token_totals() {
    let mut a = app();
    let footer = |a: &App| {
        a.render_footer_left(120)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    // One gauge for every transport: the configured window is the fallback.
    assert!(footer(&a).contains("context 0/200k"));
    push_turn(&mut a);
    a.apply_event(AgentEvent::ContextReported {
        context: Some(lofi_types::ContextUsage {
            used: 53_000,
            size: Some(128_000),
        }),
    });
    assert!(footer(&a).contains("context 53k/128k"));
    assert_eq!(a.total_in, 0);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.04,
        usage: Usage {
            input_tokens: 100_000,
            output_tokens: 2000,
            ..Usage::default()
        },
        stop_reason: None,
    });
    assert!(footer(&a).contains("context 53k/128k"));
    assert_eq!(a.total_in, 100_000);
    // A context report carries no spend; spend stays on the turn markers.
    assert_eq!(a.cost, 0.04);
    a.apply_event(AgentEvent::ContextReported {
        context: Some(lofi_types::ContextUsage {
            used: 1000,
            size: Some(64_000),
        }),
    });
    assert!(footer(&a).contains("context 1k/64k"));
    assert_eq!(a.cost, 0.04);
    a.apply_event(AgentEvent::ContextReported {
        context: Some(lofi_types::ContextUsage {
            used: 1000,
            size: None,
        }),
    });
    // A size-less report keeps the transport's fill and falls back to the
    // configured window.
    assert!(footer(&a).contains("context 1k/200k"));
    a.apply_event(AgentEvent::ContextReported { context: None });
    assert!(footer(&a).contains("context 102k/200k"));
}

#[test]
fn compact_count_formats() {
    assert_eq!(compact_count(0), "0");
    assert_eq!(compact_count(500), "500");
    assert_eq!(compact_count(119_000), "119k");
    assert_eq!(compact_count(200_000), "200k");
    assert_eq!(compact_count(2_000_000), "2M");
    assert_eq!(compact_count(9_700_000), "9.7M");
}

#[test]
fn fmt_cost_two_decimals() {
    assert_eq!(fmt_cost(0.0), "$0.00");
    assert_eq!(fmt_cost(0.01), "$0.01");
    assert_eq!(fmt_cost(81.0), "$81.00");
    assert_eq!(fmt_cost(81.40), "$81.40");
    assert_eq!(fmt_cost(81.45), "$81.45");
}

#[test]
fn abbreviate_path_progressive() {
    let home = dirs::home_dir().unwrap();
    let p = home.join("Dev/src/proj");
    assert_eq!(abbreviate_path(&p, 100), "~/Dev/src/proj");
    assert_eq!(abbreviate_path(&p, 10), "~/D/s/proj");
    assert_eq!(abbreviate_path(&p, 4), "proj");
    let p2 = home.join("Dev/~user/lofi");
    assert_eq!(abbreviate_path(&p2, 12), "~/D/~u/lofi");
}

#[test]
fn abbreviate_path_falls_back_to_ellipsis_between_marker_and_tail() {
    let home = dirs::home_dir().unwrap();
    let p = home.join(".local/state/l/tmp/lofi-00000000-0000-4000-8000-000000000000/0123456789abcdef0123456789abcdef");
    let tail = "0123456789abcdef0123456789abcdef";
    // The whole tail fits: marker, ellipsis, tail.
    assert_eq!(abbreviate_path(&p, 36), format!("~/…/{tail}"));
    // The tail clips to the remaining width, marked with an ellipsis.
    assert_eq!(abbreviate_path(&p, 10), "~/…/01234…");
}

#[test]
fn abbreviate_path_shortens_the_middle_first_one_component_at_a_time() {
    let home = dirs::home_dir().unwrap();
    let p = home.join("Dev/src/gitlab.com/acme/lofi");
    // Full width first.
    assert_eq!(abbreviate_path(&p, 100), "~/Dev/src/gitlab.com/acme/lofi");
    // The center middle component goes first, then left before right, then
    // the leading component, then only the tail remains.
    assert_eq!(abbreviate_path(&p, 21), "~/Dev/src/g/acme/lofi");
    assert_eq!(abbreviate_path(&p, 19), "~/Dev/s/g/acme/lofi");
    assert_eq!(abbreviate_path(&p, 16), "~/Dev/s/g/a/lofi");
    assert_eq!(abbreviate_path(&p, 14), "~/D/s/g/a/lofi");
    // No whole form fits: marker, ellipsis, tail.
    assert_eq!(abbreviate_path(&p, 13), "~/…/lofi");
}

#[test]
fn footer_and_header_show_cost_and_usage() {
    let mut a = App::new(
        "proxy/deepseek-v4-flash".to_string(),
        ThinkingLevel::Off,
        ServiceTier::Auto,
        200_000,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 18.0,
        usage: Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        stop_reason: None,
    });
    let footer: String = a
        .render_footer_left(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(footer.contains("↑1M ↓1M"), "footer: {footer}");
    assert!(footer.contains("context 2M/200k"), "footer: {footer}");
    let cost: String = a
        .render_footer_cost()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(cost.contains("$18.00"), "cost: {cost}");
    let header: String = a
        .render_header_line(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(header.contains("lofi"), "header: {header}");
    assert!(
        !header.contains('$'),
        "header should not show cost: {header}"
    );
}

#[test]
fn footer_shows_cache_percentage_without_cumulative_cache_counts() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 1.0,
        usage: Usage {
            input_tokens: 100_000,
            output_tokens: 50_000,
            cache_read_tokens: 800_000,
            cache_write_tokens: 200_000,
        },
        stop_reason: None,
    });
    let footer: String = a
        .render_footer_left(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(footer.contains("73% cached"), "footer: {footer}");
    assert!(!footer.contains("cache ↑"), "footer: {footer}");
}

#[test]
fn footer_left_hides_labels_then_elements_when_narrow() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 1.0,
        usage: Usage {
            input_tokens: 100_000,
            output_tokens: 100_000,
            cache_read_tokens: 800_000,
            cache_write_tokens: 200_000,
        },
        stop_reason: None,
    });
    let footer = |a: &App, width: usize| {
        a.render_footer_left(width)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    };
    let full = footer(&a, 120);
    assert!(full.contains("↑100k ↓100k"), "full: {full}");
    assert!(
        full.contains("context 1.2M/200k · 73% cached"),
        "full: {full}"
    );
    assert_eq!(footer(&a, 30), "↑100k ↓100k · 1.2M/200k");
    assert_eq!(footer(&a, 10), "1.2M/200k");
    assert_eq!(footer(&a, 5), "");
}

#[test]
fn retry_notice_is_transient_status_and_resets_on_success() {
    let mut a = app();
    push_turn(&mut a);
    let before = a.transcript.turns[0].blocks.len();

    a.apply_event(AgentEvent::RetryStart {
        attempt: 2,
        max_attempts: 10,
        delay_ms: 42_000,
        error: "provider error: HTTP 500 Internal Server Error".to_string(),
    });

    assert_eq!(a.retry_badge().as_deref(), Some("Retry: 2 of 10"));
    assert_eq!(
        a.transcript.turns[0].blocks.len(),
        before,
        "retry status must not enter the transcript"
    );

    a.apply_event(AgentEvent::RetryEnd {
        success: true,
        attempt: 2,
        final_error: None,
    });
    assert!(a.retry_badge().is_none());
    assert_eq!(a.transcript.turns[0].blocks.len(), before);
}

#[test]
fn queued_prompts_and_notices_render_below_working_indicator() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "active prompt".to_string(),
    });
    a.run = Some(0);
    a.run_start = Some(Instant::now());
    a.prompt_queue.push(QueuedPrompt {
        text: "queued user message".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.prompt_queue.push(QueuedPrompt {
        text: "job 7 completed".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    let rendered = view::blocks::render_turns(&a, 80);
    let rows: Vec<String> = rendered
        .lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let working_idx = rows
        .iter()
        .position(|r| r.contains("Working for"))
        .expect("working row");
    let user_idx = rows
        .iter()
        .position(|r| r.contains("queued user message"))
        .expect("queued user row");
    let notice_idx = rows
        .iter()
        .position(|r| r.contains("job 7 completed"))
        .expect("queued notice row");
    assert!(
        working_idx < user_idx && user_idx < notice_idx,
        "expected Working ({working_idx}) < queued user ({user_idx}) < queued notice ({notice_idx}): {rows:#?}"
    );
}

#[test]
fn working_line_reports_an_unanswered_cancel() {
    let mut a = app();
    a.run = Some(0);
    a.run_start = Some(Instant::now());
    let stalled = Instant::now().checked_sub(CANCEL_STALL * 2).unwrap();
    a.cancel_at = Some(stalled);
    a.run_event_at = Some(stalled);
    let rendered = view::blocks::render_turns(&a, 80);
    let rows: Vec<String> = rendered
        .lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(
        rows.iter()
            .any(|r| r.contains("no response, Ctrl-C to quit")),
        "{rows:#?}"
    );
}

#[test]
fn working_line_marks_a_pending_cancel_without_the_quit_hint() {
    let mut a = app();
    a.run = Some(0);
    a.run_start = Some(Instant::now());
    a.cancel_at = Some(Instant::now());
    a.run_event_at = Some(Instant::now());
    let rendered = view::blocks::render_turns(&a, 80);
    let rows: Vec<String> = rendered
        .lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(rows.iter().any(|r| r.contains("· cancelling")), "{rows:#?}");
    assert!(
        !rows.iter().any(|r| r.contains("Ctrl-C to quit")),
        "{rows:#?}"
    );
}

#[test]
fn queued_prompts_render_in_transcript_with_muted_bars() {
    let mut a = app();
    a.prompt_queue.push(QueuedPrompt {
        text: "fix the bug".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.prompt_queue.push(QueuedPrompt {
        text: "also add tests".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    let rendered = view::blocks::render_turns(&a, 80);
    let lines: Vec<String> = rendered
        .lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(lines.iter().any(|l| l == "▌ fix the bug"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "▌ also add tests"), "{lines:?}");
    assert_eq!(rendered.lines[0].spans[0].style.fg, Some(a.theme.muted));
    assert_eq!(rendered.lines[0].spans[1].style.fg, Some(a.theme.muted));
}

#[test]
fn interrupt_run_restores_user_prompts_and_keeps_notices_queued() {
    // A Notice that arrives while a run is live must not enter the editor,
    // but it must survive cancellation for the next agent round.
    let mut a = app();
    a.prompt_queue.push(QueuedPrompt {
        text: "user follow-up".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.prompt_queue.push(QueuedPrompt {
        text: "job 1786 completed: sleep 1".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    restore_queued_prompts(&mut a);
    assert_eq!(a.input, "user follow-up");
    assert!(!a.input.contains("job 1786"));
    assert_eq!(a.prompt_queue.len(), 1);
    assert_eq!(a.prompt_queue[0].kind, lofi_types::PromptKind::Notice);
    assert!(a.prompt_queue[0].text.contains("job 1786"));
}

#[test]
fn queued_notice_renders_with_quarter_block_and_italic_body() {
    let mut a = app();
    a.prompt_queue.push(QueuedPrompt {
        text: "job 1786 completed: sleep 1".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    let rendered = view::blocks::render_turns(&a, 80);
    assert_eq!(rendered.lines[0].spans[0].content, "▎ ");
    assert_eq!(rendered.lines[0].spans[0].style.fg, Some(a.theme.muted));
    assert!(rendered.lines[0].spans[1]
        .style
        .add_modifier
        .contains(Modifier::ITALIC));
}

#[test]
fn toast_renders_with_rounded_border_on_bottom_right_above_input() {
    use ratatui::Terminal;
    let mut a = app();
    a.notify(NotifyKind::Warn, "something happened".to_string());
    let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    let row = |y: u16| -> String { (0..60).map(|x| buf[(x, y)].symbol()).collect() };
    // Footer panel occupies rows 8..12; the 3-row rounded toast sits on rows 5..8 at the right edge.
    assert!(row(5).trim_end().ends_with('╮'), "top border: {}", row(5));
    assert!(row(6).contains("something happened"), "body: {}", row(6));
    assert!(
        row(7).trim_end().ends_with('╯'),
        "bottom border: {}",
        row(7)
    );
}

#[test]
fn select_mode_colors_cursor_cell_distinctly_from_navigate_mode() {
    use ratatui::Terminal;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text("select cursor test".into()));
    let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();

    a.enter_nav();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    let nav_y = a.log_rect.y + (a.nav_cursor - a.log_off) as u16;
    assert!((0..60).any(|x| buf[(x, nav_y)].bg == a.theme.cursor_line));
    assert!((0..60).any(|x| buf[(x, nav_y)].bg == a.theme.select_cursor));
    assert!(!(0..60).any(|x| buf[(x, nav_y)].bg == a.theme.warn));

    a.enter_select();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    assert!(!(0..60).any(|x| buf[(x, nav_y)].bg == a.theme.cursor_line));
    assert!((0..60).any(|x| buf[(x, nav_y)].bg == a.theme.warn));
}

#[test]
fn policy_override_renders_left_of_cost_in_footer() {
    let mut a = app();
    let policy = lofi_core::PolicyOverride::default();
    policy.set(Some(lofi_core::BashApprovalMode::DenyAll));
    a.policy_override = Some(policy);
    let cost_line: String = a
        .render_footer_cost()
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(cost_line, "deny all · $0.00");
}

#[test]
fn alt_up_restores_queued_prompt_lifo() {
    let mut a = app();
    let mut run = None;
    a.prompt_queue.push(QueuedPrompt {
        text: "first prompt".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.prompt_queue.push(QueuedPrompt {
        text: "second prompt".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Up,
        KeyModifiers::ALT,
        KeyEventKind::Press,
    ));
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "second prompt");
    assert_eq!(a.prompt_queue.len(), 1);
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "first prompt");
    assert!(a.prompt_queue.is_empty());
}

#[test]
fn alt_up_skips_notices_when_restoring_queue() {
    // Notices are not user input — Alt+Up should never land one in the
    // editor, even if it's newer than the latest User prompt.
    let mut a = app();
    let mut run = None;
    a.prompt_queue.push(QueuedPrompt {
        text: "user text".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.prompt_queue.push(QueuedPrompt {
        text: "job 1786 completed".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Up,
        KeyModifiers::ALT,
        KeyEventKind::Press,
    ));
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "user text");
    // The Notice remains queued — only the User entry was popped.
    assert_eq!(a.prompt_queue.len(), 1);
    assert_eq!(a.prompt_queue[0].kind, lofi_types::PromptKind::Notice);
}

#[test]
fn a_retry_names_the_attempt_in_the_working_report() {
    let mut a = app();
    a.run = Some(0);
    a.retry = Some(RetryState {
        attempt: 2,
        max_attempts: 5,
    });
    let settled = program_status::Settled::default();
    let report = program_status::report(&a, &settled);
    assert_eq!(report.phase, program_status::Phase::Working);
    assert_eq!(report.msg.as_deref(), Some("retrying, attempt 2 of 5"));
}

#[test]
fn a_waiting_confirmation_outranks_the_running_turn() {
    let mut a = app();
    a.run = Some(0);
    let (request, _response) = confirm_request("touch status-ran");
    a.pending_confirms.push(request);
    a.confirm_waiting = Some((
        1,
        Instant::now().checked_sub(Duration::from_secs(5)).unwrap(),
    ));
    let settled = program_status::Settled::default();
    let report = program_status::report(&a, &settled);
    assert_eq!(report.phase, program_status::Phase::Blocked);
    assert_eq!(report.kind, Some("permission"));
    assert_eq!(report.msg.as_deref(), Some("touch status-ran"));
}

#[test]
fn a_fresh_policy_prompt_keeps_the_report_working() {
    // An approval the user answers at once needs no blocked record in a tab.
    let mut a = app();
    a.run = Some(0);
    let (request, _response) = confirm_request("touch status-ran");
    a.pending_confirms.push(request);
    a.confirm_waiting = Some((1, Instant::now()));
    let settled = program_status::Settled::default();
    let report = program_status::report(&a, &settled);
    assert_eq!(report.phase, program_status::Phase::Working);
    assert_eq!(report.msg.as_deref(), None);
}

#[test]
fn an_auto_evaluation_keeps_the_report_working() {
    // The evaluator decides on its own, so nothing waits on the user yet.
    let mut a = app();
    a.run = Some(0);
    let (mut request, _response) = confirm_request("touch status-ran");
    request.reason = Arc::new(std::sync::Mutex::new(
        lofi_core::ConfirmReason::AutoEvaluating {
            started_at: Instant::now(),
        },
    ));
    a.pending_confirms.push(request);
    a.confirm_waiting = Some((
        1,
        Instant::now().checked_sub(Duration::from_secs(30)).unwrap(),
    ));
    let settled = program_status::Settled::default();
    let report = program_status::report(&a, &settled);
    assert_eq!(report.phase, program_status::Phase::Working);
}

#[test]
fn an_auto_decision_is_the_users_prompt_at_once() {
    let mut a = app();
    a.run = Some(0);
    let (mut request, _response) = confirm_request("touch status-ran");
    request.reason = Arc::new(std::sync::Mutex::new(lofi_core::ConfirmReason::AutoAsk {
        reason: "needs a look".to_string(),
    }));
    a.pending_confirms.push(request);
    a.confirm_waiting = Some((1, Instant::now()));
    let settled = program_status::Settled::default();
    let report = program_status::report(&a, &settled);
    assert_eq!(report.phase, program_status::Phase::Blocked);
}

#[test]
fn a_launch_without_a_model_reports_the_reason() {
    let mut a = app();
    a.no_models_hint = Some("set OPENAI_API_KEY".to_string());
    let settled = program_status::Settled::default();
    let report = program_status::report(&a, &settled);
    assert_eq!(report.phase, program_status::Phase::Error);
    assert_eq!(report.msg.as_deref(), Some("set OPENAI_API_KEY"));
}

#[test]
fn no_model_submit_surfaces_hint_without_running() {
    let mut a = app();
    a.no_models_hint = Some("set OPENAI_API_KEY".to_string());
    a.input = "hello".to_string();
    let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::empty(),
        KeyEventKind::Press,
    ));
    let mut run = None;
    handle_event(&ev, &mut a, None, &mut run);
    assert!(run.is_none(), "no run should be started without a model");
    assert!(a.run.is_none());
    let last = a.transcript.turns.last().unwrap();
    assert_eq!(last.prompt, "hello");
    assert!(
        last.blocks
            .iter()
            .any(|b| matches!(b, Block::Error(m) if m == "set OPENAI_API_KEY")),
        "the hint should be attached to the turn"
    );
}
