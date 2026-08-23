use super::*;

pub(super) fn app() -> App {
    App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Medium,
        ServiceTier::Auto,
        0,
        lofi_types::CompactionConfig::default(),
        String::new(),
    )
}

pub(super) fn push_turn(app: &mut App) {
    app.push_turn(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: "p".to_string(),
        blocks: Vec::new(),
    });
}

pub(super) fn test_append_events(
    path: &Path,
    events: &mut [SessionEvent],
    parent: Option<&str>,
) -> lofi_core::Result<(u64, u64)> {
    let cursor = match parent {
        Some(parent) => store::SessionCursor::new(path.to_path_buf(), Some(parent.to_string())),
        None => store::SessionCursor::open(path.to_path_buf())?,
    };
    cursor.append_events(events)
}

pub(super) fn test_append_compaction(
    path: &Path,
    kept_messages: &[Message],
    parent: Option<&str>,
    summary: &str,
    summarized_range: &[String; 2],
    counts: store::CompactionCounts,
) -> lofi_core::Result<(u64, u64, String)> {
    let cursor = match parent {
        Some(parent) => store::SessionCursor::new(path.to_path_buf(), Some(parent.to_string())),
        None => store::SessionCursor::open(path.to_path_buf())?,
    };
    let (start, end) =
        cursor.append_compaction(kept_messages, summary, summarized_range, counts)?;
    Ok((start, end, cursor.leaf_id().unwrap_or_default()))
}

/// Install a core-owned session sink (plus its read mirror) on a test `App`,
/// pointing at the test's temporary store. Mirror of what production code
/// does via `SessionConfig`/`run`. Branch switching goes through the sink in
/// production, so tests that exercise `/tree` must seed one here.
pub(super) fn attach_session_sink(
    a: &mut App,
    store: store::SessionStore,
    cwd: &Path,
    cursor: store::SessionCursor,
) {
    a.session.sink = Some(lofi_core::session::sink::SessionSink::with_store(
        store,
        cwd,
        Some(cursor.clone()),
    ));
    a.session.cursor = Some(cursor);
    a.session.cwd = cwd.to_path_buf();
}
pub(super) fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
    }
}

pub(super) fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
    }
}

fn sev_chain<I: IntoIterator<Item = SessionEventKind>>(kinds: I) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    let mut parent: Option<String> = None;
    for (i, kind) in kinds.into_iter().enumerate() {
        let id = format!("e{i}");
        out.push(SessionEvent {
            id: id.clone(),
            parent_id: parent.clone(),
            kind,
        });
        parent = Some(id);
    }
    out
}

pub(super) fn msg(m: Message) -> SessionEventKind {
    SessionEventKind::Message(m)
}

#[test]
pub(super) fn confirm_request(
    command: &str,
) -> (
    lofi_core::ConfirmRequest,
    tokio::sync::oneshot::Receiver<bool>,
) {
    let (respond, response) = tokio::sync::oneshot::channel();
    (
        lofi_core::ConfirmRequest {
            id: 1,
            command: command.to_string(),
            reason: std::sync::Arc::new(std::sync::Mutex::new(lofi_core::ConfirmReason::Policy)),
            active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            respond,
        },
        response,
    )
}

#[test]
pub(super) fn join_rendered(text: &ratatui::text::Text<'static>) -> String {
    let mut s = String::new();
    for line in &text.lines {
        for span in &line.spans {
            s.push_str(span.content.as_ref());
        }
        s.push('\n');
    }
    s
}

#[test]
pub(super) fn user_text(m: &lofi_types::Message) -> &str {
    match &m.blocks[..] {
        [lofi_types::ContentBlock::Text { text }] => text,
        _ => "",
    }
}
#[test]
pub(super) fn ctrl_key(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code,
        KeyModifiers::CONTROL,
        KeyEventKind::Press,
    ))
}

#[test]
pub(super) fn plain_key(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code,
        KeyModifiers::empty(),
        KeyEventKind::Press,
    ))
}

#[test]
