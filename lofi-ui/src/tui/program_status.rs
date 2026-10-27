//! Report program status with OSC 7501.
//!
//! One report tells the terminal what the program is doing, so a tab, a
//! sidebar, or a desktop notification can show it while the user looks away.
//! The sequence is `ESC ] 7501 ; key=value(:key=value)* ST` on stdout; each
//! report replaces the record it addresses. Terminals that do not know the
//! sequence ignore it, so a report needs no support probe first — and a probe
//! would have to steal stdin bytes from the `TtyEvents` reader.
//!
//! Reports go out on a phase change only, never per frame: the app draws many
//! frames per second and each one would otherwise cost a write and a record
//! update in the terminal.
//!
//! `working` and `blocked` end by themselves when the process exits, so a
//! quit needs no report. `done` and `error` survive the exit, which is the
//! point of reporting them: the result stays for the user to find, and each
//! one carries the line the screen shows, so the record says what finished
//! or what failed.
//!
//! A job or a subagent that outlives the turn gets a record of its own, under
//! an id of `job/<n>` or `sub/<id>`. The root keeps saying `working` for the
//! whole set; the child names the one command or task. A child that stops
//! running leaves by `clear`, because no later report replaces it.
//!
//! `kind=question` and `kind=auth` stay unused: lofi has no ask-the-user
//! tool and no in-process login wait, so neither names a wait that exists
//! here. `progress` stays unused too, because a turn has no honest fraction
//! to report, and an invented one misleads more than it informs.
//!
//! Inside tmux a bare report never arrives: tmux parses the sequence itself
//! and drops what it does not know, and `allow-passthrough` does not change
//! that — it opens the documented bypass, `ESC Ptmux; ... ESC\`, which the
//! program has to use. The report travels inside that wrapper, with every ESC
//! doubled, and tmux hands it to the terminal outside. One wrapper covers one
//! tmux: a tmux inside tmux would need one more, and the outer tmux sees a
//! bare report and drops it.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use base64::Engine as _;
use lofi_core::{AgentEvent, ConfirmReason, ConfirmRequest};

use super::view::fmt_duration;
use super::App;

/// Protocol cap on one report, terminator included.
const MAX_REPORT_BYTES: usize = 4096;
/// Protocol cap on the decoded `title` and `msg` text.
const MAX_TITLE_BYTES: usize = 192;
const MAX_MSG_BYTES: usize = 2048;
/// How long a plain policy prompt waits before the record says `blocked`. A
/// user who answers at once needs no tab to tell them, and a longer wait is
/// exactly the case the state exists for.
const PERMISSION_DEBOUNCE: Duration = Duration::from_secs(3);
/// Protocol cap on an `id`: 32 bytes per segment, eight levels, 128 total.
const MAX_ID_SEGMENT: usize = 32;
const MAX_ID_DEPTH: usize = 8;
const MAX_ID_BYTES: usize = 128;
/// Stable program name the report carries, at most 32 bytes.
const APP_NAME: &str = "lofi";

/// The states the protocol defines. `clear` is not one: it removes the
/// record instead of describing it, and lofi never needs it because every
/// later report replaces the previous one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Error => "error",
        }
    }
}

/// One report. Absent fields stay out of the sequence; `kind` is valid only
/// on `blocked`.
pub(crate) struct Report<'a> {
    pub(crate) phase: Phase,
    pub(crate) kind: Option<&'a str>,
    pub(crate) title: Option<&'a str>,
    pub(crate) msg: Option<Cow<'a, str>>,
}

/// The report for the app as it stands. A waiting confirmation outranks the
/// turn that produced it, because that is what the user has to answer.
/// `settled` is the outcome of the last run, which stays on screen at rest
/// until the next run reports `working`.
pub(crate) fn report<'a>(app: &'a App, settled: &'a Settled) -> Report<'a> {
    // Only a permission ask can hold a turn, and naming the command in the
    // terminal is what tells the user what they are approving.
    if let Some(request) = waiting(app) {
        return Report {
            phase: Phase::Blocked,
            kind: Some("permission"),
            title: None,
            msg: Some(Cow::Borrowed(request.command.as_str())),
        };
    }
    if app.busy() {
        // A retry keeps the turn running, so the state stays `working`.
        // Naming the attempt explains a stall that the screen shows and a tab
        // does not.
        return Report {
            phase: Phase::Working,
            kind: None,
            title: None,
            msg: app.retry.as_ref().map(|retry| {
                Cow::Owned(format!(
                    "retrying, attempt {} of {}",
                    retry.attempt, retry.max_attempts
                ))
            }),
        };
    }
    // Background work outranks the outcome of the last run: a job or a
    // subagent still runs, so `done` would call the user back before the
    // program is finished. Subagents stay a yes/no question because counting
    // them copies every running task string.
    let jobs = app.running_jobs().iter().filter(|job| job.running).count();
    if let Some(msg) = background_work(jobs, app.has_running_subagents()) {
        return Report {
            phase: Phase::Working,
            kind: None,
            title: None,
            msg: Some(Cow::Owned(msg)),
        };
    }
    // A launch that found no usable model keeps its reason in the transcript
    // and waits on the picker; the terminal shows that reason instead of
    // claiming the session is simply idle.
    if settled.phase == Phase::Idle {
        if let Some(hint) = &app.no_models_hint {
            return Report {
                phase: Phase::Error,
                kind: None,
                title: None,
                msg: Some(Cow::Borrowed(hint.as_str())),
            };
        }
    }
    Report {
        phase: settled.phase,
        kind: None,
        title: None,
        msg: settled.msg.as_deref().map(Cow::Borrowed),
    }
}

/// The pending confirmation the user has to answer right now. An auto-mode
/// evaluation that has not returned is still deciding, so the record stays
/// `working`; a prompt auto mode chose to ask, or blocked on, is the user's at
/// once — the wait is already real; and a plain policy prompt waits out a
/// short debounce, because an approval that takes a moment needs no record.
fn waiting(app: &App) -> Option<&ConfirmRequest> {
    let request = app.pending_confirms.first()?;
    let ready = match *request
        .reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        ConfirmReason::AutoEvaluating { .. } => false,
        ConfirmReason::AutoAsk { .. }
        | ConfirmReason::AutoRisk { .. }
        | ConfirmReason::AutoFailed { .. } => true,
        ConfirmReason::Policy | ConfirmReason::OutsideRoot { .. } => app
            .confirm_waiting
            .is_some_and(|(_, since)| since.elapsed() >= PERMISSION_DEBOUNCE),
    };
    ready.then_some(request)
}

/// What still runs after the turn ended: a background job or subagent keeps
/// the record `working`, and the message counts it. `None` means the
/// program is at rest.
fn background_work(jobs: usize, subagents: bool) -> Option<String> {
    let mut parts = Vec::new();
    match jobs {
        0 => {}
        1 => parts.push("1 job".to_string()),
        jobs => parts.push(format!("{jobs} jobs")),
    }
    if subagents {
        parts.push("subagents".to_string());
    }
    (!parts.is_empty()).then(|| format!("{} running", parts.join(" and ")))
}

/// What a finished run leaves behind: the phase at rest and the line that
/// explains it. The line mirrors what the transcript shows, so the record for
/// a finished turn names the work without sending the user back to the screen.
pub(crate) struct Settled {
    pub(crate) phase: Phase,
    pub(crate) msg: Option<String>,
}

impl Settled {
    pub(crate) fn failed(msg: String) -> Self {
        Self {
            phase: Phase::Error,
            msg: Some(msg),
        }
    }
}

impl Default for Settled {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            msg: None,
        }
    }
}

/// What a terminal event leaves behind, if the event ends a run. Only live
/// events pass through here: replaying a resumed transcript must not report a
/// turn that ended before this process started.
pub(crate) fn settled(event: &AgentEvent) -> Option<Settled> {
    match event {
        AgentEvent::TurnEnd {
            model, elapsed_ms, ..
        } => Some(Settled {
            phase: Phase::Done,
            msg: Some(format!(
                "Done in {} with {}",
                fmt_duration(Duration::from_millis(*elapsed_ms)),
                model.label()
            )),
        }),
        AgentEvent::TurnFailed { error, .. } | AgentEvent::Error(error) => {
            Some(Settled::failed(error.clone()))
        }
        AgentEvent::TurnCancelled { .. } => Some(Settled {
            phase: Phase::Idle,
            msg: None,
        }),
        // A cancelled shell reports idle like any other interrupt; a finished
        // one names the command the user typed.
        AgentEvent::UserShell {
            command, cancelled, ..
        } => Some(Settled {
            phase: if *cancelled { Phase::Idle } else { Phase::Done },
            msg: (!*cancelled).then(|| command.clone()),
        }),
        _ => None,
    }
}

/// One record below the root: a job or a subagent that still runs. The root
/// stays `working` for the whole set; the child names the single thing, so a
/// tab can say which command or task waits instead of only how many.
pub(crate) struct Child {
    id: String,
    title: String,
}

/// The records that hang below the root right now. An id stays the same while
/// a child runs, so an unchanged child keeps the record it already has.
pub(crate) fn children(app: &App) -> Vec<Child> {
    let mut children = Vec::new();
    for job in app.running_jobs().iter().filter(|job| job.running) {
        children.push(Child {
            id: format!("job/{}", job.id),
            title: job.cmd.clone(),
        });
    }
    for run in app.running_subagents() {
        let Some(segment) = id_segment(&run.id) else {
            continue;
        };
        children.push(Child {
            id: format!("sub/{segment}"),
            title: run.task.clone(),
        });
    }
    children
}

/// Fit `text` into one `id` segment. A report whose id is outside the
/// protocol grammar is dropped whole, so a run that leaves nothing keeps its
/// name off the wire instead of breaking the batch.
fn id_segment(text: &str) -> Option<String> {
    let mut out = String::new();
    for c in text.chars() {
        if id_char(c) {
            out.push(c);
            if out.len() == MAX_ID_SEGMENT {
                break;
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Whether `id` matches the protocol path grammar: at most eight segments of
/// at most 32 bytes, 128 bytes in total.
fn valid_id(id: &str) -> bool {
    id.len() <= MAX_ID_BYTES
        && id.split('/').count() <= MAX_ID_DEPTH
        && id.split('/').all(|segment| {
            (1..=MAX_ID_SEGMENT).contains(&segment.len()) && segment.chars().all(id_char)
        })
}

fn id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-')
}

/// Whether the reports travel through tmux. `$TMUX` is the reliable
/// signal; a stripped environment still leaves `TERM` naming the terminal.
fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
        || std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
}

/// Wrap `sequence` for tmux passthrough. tmux forwards the payload to the
/// terminal outside and un-doubles each ESC, so the report arrives unchanged.
fn wrapped(sequence: &str) -> String {
    let payload = sequence.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{payload}\x1b\\")
}

/// Reports each record change to the terminal: the root record, then the
/// children that hang below it.
pub(crate) struct ProgramStatus {
    /// The last sequence written for the root. Two reports can share a state
    /// and still change the record — a second command that waits for approval,
    /// a retried attempt — so the comparison covers the whole sequence.
    last: Option<String>,
    /// The child records the terminal holds, by id. Nothing replaces a child
    /// that stops running, so the set is compared and cleared by name.
    held: BTreeMap<String, String>,
    /// A redirected stdout gets no report: print mode and piped output stay
    /// clean, and an escape sequence written to a file is only noise.
    enabled: bool,
    /// Whether every sequence leaves inside the tmux passthrough wrapper.
    tmux: bool,
}

impl ProgramStatus {
    pub(crate) fn new() -> Self {
        Self {
            last: None,
            held: BTreeMap::new(),
            enabled: io::stdout().is_terminal(),
            tmux: in_tmux(),
        }
    }

    /// Report `report` and `children` unless the terminal already shows them.
    pub(crate) fn publish(&mut self, report: &Report<'_>, children: &[Child]) {
        let batch = self.sequences(report, children);
        if self.enabled && !batch.is_empty() {
            // One write per publish: a frame must not split a sequence.
            let mut stdout = io::stdout().lock();
            let _ = stdout.write_all(batch.as_bytes());
            let _ = stdout.flush();
        }
    }

    /// The bytes for `report` and `children`: the records the terminal does
    /// not hold yet, and a clear for each child that stopped running. The
    /// tracking state moves here, not in `publish`, so a redirected stdout
    /// still leaves the next comparison honest.
    fn sequences(&mut self, report: &Report<'_>, children: &[Child]) -> String {
        let mut batch = String::new();
        if let Some(sequence) = encode(report) {
            if self.last.as_deref() != Some(sequence.as_str()) {
                self.push(&mut batch, &sequence);
                self.last = Some(sequence);
            }
        }
        let live: Vec<&str> = children.iter().map(|child| child.id.as_str()).collect();
        let stale: Vec<String> = self
            .held
            .keys()
            .filter(|id| !live.contains(&id.as_str()))
            .cloned()
            .collect();
        for id in stale {
            self.push(&mut batch, &clear(&id));
            self.held.remove(&id);
        }
        for child in children {
            let Some(sequence) = encode_child(child) else {
                continue;
            };
            if self.held.get(&child.id).map(String::as_str) != Some(sequence.as_str()) {
                self.push(&mut batch, &sequence);
                self.held.insert(child.id.clone(), sequence);
            }
        }
        batch
    }

    /// Append one sequence to the batch, wrapped where tmux sits in between.
    fn push(&self, batch: &mut String, sequence: &str) {
        if self.tmux {
            batch.push_str(&wrapped(sequence));
        } else {
            batch.push_str(sequence);
        }
    }
}

/// Build the sequence for one report. `None` means the protocol would drop
/// the report whole, so nothing is sent: a partial record is worse than the
/// previous one staying up.
fn encode(report: &Report<'_>) -> Option<String> {
    let mut params = format!("state={}", report.phase.as_str());
    if report.phase == Phase::Blocked {
        if let Some(kind) = report.kind {
            params.push_str(":kind=");
            params.push_str(kind);
        }
    }
    params.push_str(":app=");
    params.push_str(APP_NAME);
    if let Some(title) = report
        .title
        .and_then(|text| one_line(text, MAX_TITLE_BYTES))
    {
        params.push_str(":title=");
        params.push_str(&base64::engine::general_purpose::STANDARD.encode(title));
    }
    if let Some(msg) = report
        .msg
        .as_deref()
        .and_then(|text| one_line(text, MAX_MSG_BYTES))
    {
        params.push_str(":msg=");
        params.push_str(&base64::engine::general_purpose::STANDARD.encode(msg));
    }
    let sequence = format!("\x1b]7501;{params}\x1b\\");
    (sequence.len() <= MAX_REPORT_BYTES).then_some(sequence)
}

/// Build the sequence for one child record. `app` stays off it: the root
/// carries `app=lofi` and every record under it inherits the name.
fn encode_child(child: &Child) -> Option<String> {
    if !valid_id(&child.id) {
        return None;
    }
    let mut params = format!("state=working:id={}", child.id);
    if let Some(title) = one_line(&child.title, MAX_TITLE_BYTES) {
        params.push_str(":title=");
        params.push_str(&base64::engine::general_purpose::STANDARD.encode(title));
    }
    let sequence = format!("\x1b]7501;{params}\x1b\\");
    (sequence.len() <= MAX_REPORT_BYTES).then_some(sequence)
}

/// The sequence that removes one child record and everything under it.
fn clear(id: &str) -> String {
    format!("\x1b]7501;state=clear:id={id}\x1b\\")
}

/// Fit `text` into one line of at most `max` bytes. The protocol drops a
/// report whose decoded text holds a control character, so each one becomes a
/// separator, runs of whitespace collapse, and the tail is cut on a character
/// boundary. Text with nothing left does not travel at all.
fn one_line(text: &str, max: usize) -> Option<String> {
    let cleaned = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        return None;
    }
    let mut end = cleaned.len().min(max);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    Some(cleaned[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(phase: Phase, msg: Option<&str>) -> Option<String> {
        encode(&Report {
            phase,
            kind: (phase == Phase::Blocked).then_some("permission"),
            title: None,
            msg: msg.map(Cow::Borrowed),
        })
    }

    fn base64(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    #[test]
    fn working_report_names_the_app_and_ends_with_st() {
        assert_eq!(
            encoded(Phase::Working, None).as_deref(),
            Some("\x1b]7501;state=working:app=lofi\x1b\\")
        );
    }

    #[test]
    fn blocked_report_carries_the_permission_kind() {
        assert_eq!(
            encoded(Phase::Blocked, None).as_deref(),
            Some("\x1b]7501;state=blocked:kind=permission:app=lofi\x1b\\")
        );
    }

    #[test]
    fn terminal_states_need_no_kind() {
        for phase in [Phase::Idle, Phase::Done, Phase::Error] {
            let sequence = encoded(phase, None).unwrap_or_default();
            assert!(!sequence.contains("kind="), "{sequence}");
            assert!(sequence.contains(&format!("state={}", phase.as_str())));
        }
    }

    #[test]
    fn message_is_base64_without_newlines() {
        assert_eq!(
            encoded(Phase::Blocked, Some("touch status-ran")).as_deref(),
            Some(
                format!(
                    "\x1b]7501;state=blocked:kind=permission:app=lofi:msg={}\x1b\\",
                    base64("touch status-ran")
                )
                .as_str()
            )
        );
    }

    #[test]
    fn control_characters_never_reach_the_report() {
        assert_eq!(
            one_line("one\ntwo\x07three", MAX_TITLE_BYTES).as_deref(),
            Some("one two three")
        );
        assert_eq!(
            one_line("rm\t-rf\u{1b}[31m.", MAX_MSG_BYTES).as_deref(),
            Some("rm -rf [31m.")
        );
    }

    #[test]
    fn text_is_cut_at_a_character_boundary_within_the_cap() {
        // 100 'é' are 200 bytes: the 192-byte cap falls inside the 96th one.
        let long = "é".repeat(100);
        assert_eq!(one_line(&long, MAX_TITLE_BYTES).map(|t| t.len()), Some(192));
        assert_eq!(
            one_line(&"x".repeat(3000), MAX_MSG_BYTES).map(|t| t.len()),
            Some(MAX_MSG_BYTES)
        );
    }

    #[test]
    fn blank_text_does_not_travel() {
        assert_eq!(one_line(" \n\t", MAX_MSG_BYTES), None);
        let sequence = encoded(Phase::Working, Some("   ")).unwrap_or_default();
        assert!(!sequence.contains(":msg="));
    }

    #[test]
    fn longest_report_stays_under_the_protocol_cap() {
        let title = "t".repeat(MAX_TITLE_BYTES);
        let msg = "m".repeat(MAX_MSG_BYTES);
        let report = Report {
            phase: Phase::Blocked,
            kind: Some("permission"),
            title: Some(&title),
            msg: Some(Cow::Borrowed(&msg)),
        };
        assert_eq!(
            encode(&report).map(|sequence| sequence.len() <= MAX_REPORT_BYTES),
            Some(true)
        );
    }

    #[test]
    fn the_passthrough_wrapper_doubles_every_esc() {
        let sequence = "\x1b]7501;state=done:app=lofi\x1b\\";
        assert_eq!(
            wrapped(sequence),
            "\x1bPtmux;\x1b\x1b]7501;state=done:app=lofi\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn background_work_names_what_still_runs() {
        assert_eq!(background_work(0, false), None);
        assert_eq!(background_work(1, false).as_deref(), Some("1 job running"));
        assert_eq!(background_work(3, false).as_deref(), Some("3 jobs running"));
        assert_eq!(
            background_work(0, true).as_deref(),
            Some("subagents running")
        );
        assert_eq!(
            background_work(2, true).as_deref(),
            Some("2 jobs and subagents running")
        );
    }

    #[test]
    fn a_changed_message_reaches_the_terminal_again() {
        // A second command that waits for approval, or a retried attempt,
        // keeps the state and changes the record. Comparing states alone
        // would leave the first command on screen.
        let mut status = ProgramStatus {
            last: None,
            held: BTreeMap::new(),
            enabled: false,
            tmux: false,
        };
        let first = Report {
            phase: Phase::Blocked,
            kind: Some("permission"),
            title: None,
            msg: Some(Cow::Borrowed("touch one")),
        };
        let second = Report {
            msg: Some(Cow::Borrowed("touch two")),
            ..first
        };
        status.publish(&first, &[]);
        let sent = status.last.clone();
        assert_eq!(
            sent.as_deref(),
            encoded(Phase::Blocked, Some("touch one")).as_deref()
        );
        status.publish(&first, &[]);
        assert_eq!(status.last, sent, "an unchanged record must not repeat");
        status.publish(&second, &[]);
        assert_eq!(
            status.last.as_deref(),
            encoded(Phase::Blocked, Some("touch two")).as_deref(),
            "the second command must replace the first"
        );
    }

    #[test]
    fn settled_maps_terminal_events_only() {
        assert!(settled(&AgentEvent::Text("hi".into())).is_none());
        assert_eq!(
            settled(&AgentEvent::UserShell {
                command: "true".into(),
                output: String::new(),
                exit_code: Some(0),
                signal: None,
                duration_ms: 1,
                truncated: false,
                cancelled: true,
                exclude_from_context: false,
            })
            .map(|s| s.phase),
            Some(Phase::Idle)
        );
        let Some(failed) = settled(&AgentEvent::Error("boom".into())) else {
            panic!("a failed turn settles");
        };
        assert_eq!(failed.phase, Phase::Error);
        assert_eq!(failed.msg.as_deref(), Some("boom"));
    }

    #[test]
    fn done_names_the_duration_and_the_model() {
        let event = AgentEvent::TurnEnd {
            model: lofi_types::RunModel {
                provider: "openai".into(),
                id: "gpt-4o".into(),
                selected: String::new(),
                display: None,
                thinking: lofi_types::ThinkingLevel::Off,
                service_tier: lofi_types::ServiceTier::Auto,
            },
            elapsed_ms: 1500,
            cost: 0.0,
            usage: lofi_types::Usage::default(),
            stop_reason: None,
        };
        let Some(done) = settled(&event) else {
            panic!("a finished turn settles");
        };
        assert_eq!(done.phase, Phase::Done);
        assert_eq!(done.msg.as_deref(), Some("Done in 1.5s with openai/gpt-4o"));
    }

    #[test]
    fn a_child_record_names_its_job_under_the_root() {
        let child = Child {
            id: "job/7".into(),
            title: "make test".into(),
        };
        assert_eq!(
            encode_child(&child).as_deref(),
            Some("\x1b]7501;state=working:id=job/7:title=bWFrZSB0ZXN0\x1b\\")
        );
    }

    #[test]
    fn a_child_without_a_title_still_names_its_record() {
        let child = Child {
            id: "sub/abc".into(),
            title: String::new(),
        };
        assert_eq!(
            encode_child(&child).as_deref(),
            Some("\x1b]7501;state=working:id=sub/abc\x1b\\")
        );
    }

    #[test]
    fn an_id_outside_the_grammar_keeps_the_report_off_the_wire() {
        let long = "x".repeat(MAX_ID_SEGMENT + 1);
        let deep = "a/".repeat(MAX_ID_DEPTH);
        for id in ["", "job/", "job//7", "job/seven eight", &long, &deep] {
            assert_eq!(
                encode_child(&Child {
                    id: id.to_string(),
                    title: "make test".into(),
                }),
                None,
                "{id:?}"
            );
        }
    }

    #[test]
    fn a_run_id_becomes_one_id_segment() {
        assert_eq!(id_segment("ab:cd ef").as_deref(), Some("abcdef"));
        assert_eq!(
            id_segment(&"9".repeat(40)).map(|segment| segment.len()),
            Some(MAX_ID_SEGMENT)
        );
        assert_eq!(id_segment("::"), None);
    }

    #[test]
    fn a_child_record_leaves_when_its_job_stops() {
        // Nothing replaces a child that stops running, so the only way the
        // terminal learns it is gone is a clear by id.
        let mut status = ProgramStatus {
            last: None,
            held: BTreeMap::new(),
            enabled: false,
            tmux: false,
        };
        let report = Report {
            phase: Phase::Working,
            kind: None,
            title: None,
            msg: None,
        };
        let child = Child {
            id: "job/7".into(),
            title: "make test".into(),
        };
        let first = status.sequences(&report, std::slice::from_ref(&child));
        assert!(first.contains("id=job/7"), "{first:?}");
        assert_eq!(
            status.sequences(&report, &[]),
            "\x1b]7501;state=clear:id=job/7\x1b\\"
        );
        assert!(
            status.sequences(&report, &[]).is_empty(),
            "an unchanged pair must not repeat"
        );
    }
}
