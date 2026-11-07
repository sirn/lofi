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
    app.transcript.push_turn(Turn {
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
        origin: None,
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
    }
}

pub(super) fn assistant(text: &str) -> Message {
    Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
    }
}

pub(super) fn blank_events<I: IntoIterator<Item = SessionEventKind>>(
    kinds: I,
) -> Vec<SessionEvent> {
    kinds
        .into_iter()
        .map(|kind| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind,
        })
        .collect()
}

pub(super) fn sev_chain<I: IntoIterator<Item = SessionEventKind>>(kinds: I) -> Vec<SessionEvent> {
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

pub(super) fn grant_request(
    dir: &str,
    requested: lofi_types::Access,
    current: Option<lofi_types::GrantedDir>,
) -> (
    lofi_core::ConfirmRequest,
    tokio::sync::oneshot::Receiver<lofi_core::ConfirmResponse>,
) {
    grant_request_for(dir, None, requested, current)
}

/// A grant request for a project root that offers the requested directory as
/// the narrower unit.
/// A read-only grant a prompt can name as the access it starts from.
pub(super) fn read_grant(dir: &str) -> lofi_types::GrantedDir {
    lofi_types::GrantedDir {
        dir: std::path::PathBuf::from(dir),
        access: lofi_types::Access::Read,
    }
}

pub(super) fn grant_request_with_narrow(
    dir: &str,
    narrow: &str,
    requested: lofi_types::Access,
) -> (
    lofi_core::ConfirmRequest,
    tokio::sync::oneshot::Receiver<lofi_core::ConfirmResponse>,
) {
    grant_request_for(dir, Some(narrow), requested, None)
}

pub(super) fn grant_request_for(
    dir: &str,
    narrow: Option<&str>,
    requested: lofi_types::Access,
    current: Option<lofi_types::GrantedDir>,
) -> (
    lofi_core::ConfirmRequest,
    tokio::sync::oneshot::Receiver<lofi_core::ConfirmResponse>,
) {
    let (respond, response) = tokio::sync::oneshot::channel();
    (
        lofi_core::ConfirmRequest {
            id: 1,
            command: dir.to_string(),
            reason: std::sync::Arc::new(std::sync::Mutex::new(
                lofi_core::ConfirmReason::OutsideRoot {
                    dir: dir.to_string(),
                    narrow: narrow.map(str::to_string),
                    marker: narrow.map(|_| ".jj"),
                    requested,
                    current,
                },
            )),
            active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            deny_at: std::sync::Arc::new(std::sync::Mutex::new(None)),
            respond,
        },
        response,
    )
}

pub(super) fn confirm_request(
    command: &str,
) -> (
    lofi_core::ConfirmRequest,
    tokio::sync::oneshot::Receiver<lofi_core::ConfirmResponse>,
) {
    let (respond, response) = tokio::sync::oneshot::channel();
    (
        lofi_core::ConfirmRequest {
            id: 1,
            command: command.to_string(),
            reason: std::sync::Arc::new(std::sync::Mutex::new(lofi_core::ConfirmReason::Policy)),
            active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            deny_at: std::sync::Arc::new(std::sync::Mutex::new(None)),
            respond,
        },
        response,
    )
}

pub(super) fn user_text(m: &lofi_types::Message) -> &str {
    match &m.blocks[..] {
        [lofi_types::ContentBlock::Text { text }] => text,
        _ => "",
    }
}
pub(super) fn ctrl_key(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code,
        KeyModifiers::CONTROL,
        KeyEventKind::Press,
    ))
}

pub(super) fn plain_key(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code,
        KeyModifiers::empty(),
        KeyEventKind::Press,
    ))
}

/// The background dialog's subagents tab, for tests that drive it directly.
pub(super) fn subagents_tab(a: &App) -> &SubagentsModalState {
    &a.background_modal
        .as_ref()
        .expect("background modal")
        .subagents
}

/// The subagent drill-in, when the dialog shows one.
pub(super) fn subagent_view(a: &App) -> Option<&SubagentTranscriptView> {
    match a.background_modal.as_ref()?.view.as_ref()? {
        BackgroundView::Subagent(view) => Some(view),
        BackgroundView::Job(_) => None,
    }
}

pub(super) fn subagent_view_mut(a: &mut App) -> Option<&mut SubagentTranscriptView> {
    match a.background_modal.as_mut()?.view.as_mut()? {
        BackgroundView::Subagent(view) => Some(view),
        BackgroundView::Job(_) => None,
    }
}

/// Put a subagent drill-in on the dialog, the way `Enter` does.
pub(super) fn open_subagent_view(a: &mut App, view: SubagentTranscriptView) {
    let modal = a
        .background_modal
        .get_or_insert_with(|| background_modal(BackgroundTab::Subagents));
    modal.view = Some(BackgroundView::Subagent(view));
}

/// A fresh dialog on one tab, with no rows yet.
pub(super) fn background_modal(tab: BackgroundTab) -> BackgroundModalState {
    BackgroundModalState {
        tab,
        jobs: JobsModalState { selected: 0 },
        subagents: SubagentsModalState {
            selected: 0,
            rows: Vec::new(),
        },
        timers: TimersModalState {
            selected: 0,
            rows: Vec::new(),
        },
        view: None,
        confirm: None,
    }
}
