//! The auto-mode evaluation log.
//!
//! Every decision-model evaluation is appended to a JSONL file under the
//! session whose command it judged, so calibration works on live traffic: no
//! `RUST_LOG` capture is needed, which only exists when a session starts
//! with debug logging on. The records are the same JSONL the calibrate
//! command reads, so a probe archive and a live log share one format, and a
//! replay merges every session's file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lofi_code::AutoModeOutcome;
use lofi_types::DecisionAnswer;

use lofi_error::{Error, Result};

/// Every number `decide_with` reads off one answer set, in the one shape
/// the JSONL archive uses.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Scores {
    pub(crate) scope: String,
    pub(crate) system: f64,
    pub(crate) remote: f64,
    pub(crate) destructive: f64,
    pub(crate) exposure: f64,
    pub(crate) authorized: f64,
    pub(crate) known: f64,
}

/// One judged command with its outcome label.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Record {
    pub(crate) label: String,
    pub(crate) command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) request: Option<String>,
    pub(crate) scores: Scores,
}

/// Read the scores off a live answer set. Returns `None` when an answer
/// is missing, because a record without every number cannot be rejudged.
pub(crate) fn scores_from_answers(answers: &BTreeMap<String, DecisionAnswer>) -> Option<Scores> {
    let DecisionAnswer::Choice {
        choice,
        probabilities,
        ..
    } = answers.get("scope")?
    else {
        return None;
    };
    let noul = |name: &str| -> Option<f64> { answers.get(name)?.noul() };
    Some(Scores {
        scope: choice.clone(),
        system: probabilities.get("system").copied().unwrap_or(0.0),
        remote: probabilities.get("remote").copied().unwrap_or(0.0),
        destructive: noul("destructive")?,
        exposure: noul("exposure")?,
        authorized: noul("authorized")?,
        known: noul("recognized")?,
    })
}

/// The label one record carries: the gate's own outcome name.
pub(crate) fn outcome_label(outcome: &AutoModeOutcome) -> &'static str {
    match outcome {
        AutoModeOutcome::Allow { .. } => "Allow",
        AutoModeOutcome::Ask { .. } => "Ask",
        AutoModeOutcome::HighRisk { .. } => "HighRisk",
        AutoModeOutcome::Failed { .. } => "Failed",
    }
}

/// The directory one gate files its records under: the live session's
/// transcript id, or the workspace key when no session exists yet, which is
/// the print-mode case. A session id names a conversation a `--resume` can
/// pick again; the workspace key keeps a sessionless run's records grouped
/// by project instead of dropping them.
pub(crate) fn session_key(session: Option<&str>, cwd: &Path) -> String {
    session.map_or_else(|| crate::state::workspace_key(cwd), ToString::to_string)
}

/// One session's log file: `logs/<session>/auto-mode.jsonl` under the state
/// directory.
/// # Errors
/// Returns state and io errors when the state directory cannot be
/// resolved or the log directory cannot be created.
pub(crate) fn log_path(session: &str) -> Result<PathBuf> {
    let mut path = crate::state::ensure_state_dir()?;
    path.push("logs");
    path.push(session);
    path.push("auto-mode.jsonl");
    Ok(path)
}

/// Every session log, oldest first, so a replay covers the live traffic of
/// every session. A missing `logs/` directory reads as no logs.
/// # Errors
/// Returns state and io errors when the state directory cannot be resolved
/// or its log directory cannot be read.
pub(crate) fn session_logs() -> Result<Vec<PathBuf>> {
    let mut root = crate::state::ensure_state_dir()?;
    root.push("logs");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::Io(error)),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path().join("auto-mode.jsonl"))
        .filter(|path| path.is_file())
        .collect();
    paths.sort();
    Ok(paths)
}

/// Append one evaluation to a session's log. Best effort: the gate must not
/// fail a command because its log could not be written, so an error is
/// warned and swallowed.
pub(crate) fn append(session: &str, record: &Record) {
    if let Err(error) = try_append(session, record) {
        tracing::warn!(%error, "auto-mode: evaluation log append failed");
    }
}

fn try_append(session: &str, record: &Record) -> Result<()> {
    use std::io::Write as _;
    let path = log_path(session)?;
    if let Some(dir) = path.parent() {
        crate::state::ensure_private_dir(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    crate::state::ensure_private_file(&path)?;
    serde_json::to_writer(&mut file, record)
        .map_err(|e| Error::Config(format!("serialize record: {e}")))?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn temp_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lofi-eval-log-{name}"));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    fn record(command: &str) -> Record {
        Record {
            label: "Allow".to_string(),
            command: command.to_string(),
            request: None,
            scores: Scores {
                scope: "workspace".to_string(),
                system: 0.0,
                remote: 0.0,
                destructive: 0.0,
                exposure: 0.0,
                authorized: 0.0,
                known: 0.9,
            },
        }
    }

    #[test]
    fn the_log_lands_under_the_session() {
        let _lock = crate::state::STATE_ENV_LOCK.lock().unwrap();
        let dir = temp_state_dir("path");
        std::env::set_var("LOFI_STATE_HOME", &dir);
        let path = log_path("20260101_abcd").unwrap();
        std::env::remove_var("LOFI_STATE_HOME");
        assert_eq!(
            path.strip_prefix(dir.join("lofi")).unwrap(),
            std::path::Path::new("logs/20260101_abcd/auto-mode.jsonl")
        );
    }

    #[test]
    fn a_missing_session_files_under_the_workspace_key() {
        let cwd = std::path::Path::new("/repo/lofi");
        assert_eq!(
            session_key(None, cwd),
            crate::state::workspace_key(cwd),
            "a sessionless run keeps the project's key"
        );
        assert_eq!(session_key(Some("20260101_abcd"), cwd), "20260101_abcd");
    }

    #[test]
    fn append_then_read_round_trips_one_record() {
        let _lock = crate::state::STATE_ENV_LOCK.lock().unwrap();
        let dir = temp_state_dir("append");
        std::env::set_var("LOFI_STATE_HOME", &dir);
        let record = record("cargo test");
        append("20260101_abcd", &record);
        let text = std::fs::read_to_string(log_path("20260101_abcd").unwrap()).unwrap();
        std::env::remove_var("LOFI_STATE_HOME");
        let read: Record = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(read, record);
    }

    #[test]
    fn every_session_log_is_listed_oldest_first() {
        let _lock = crate::state::STATE_ENV_LOCK.lock().unwrap();
        let dir = temp_state_dir("listing");
        std::env::set_var("LOFI_STATE_HOME", &dir);
        append("20260102_bbbb", &record("ls"));
        append("20260101_aaaa", &record("ls"));
        let listed = session_logs().unwrap();
        std::env::remove_var("LOFI_STATE_HOME");
        let names: Vec<String> = listed
            .iter()
            .map(|path| {
                path.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["20260101_aaaa", "20260102_bbbb"]);
    }

    #[test]
    fn no_log_directory_lists_nothing() {
        let _lock = crate::state::STATE_ENV_LOCK.lock().unwrap();
        let dir = temp_state_dir("empty");
        std::env::set_var("LOFI_STATE_HOME", &dir);
        let listed = session_logs().unwrap();
        std::env::remove_var("LOFI_STATE_HOME");
        assert!(listed.is_empty(), "{listed:?}");
    }

    #[test]
    fn the_log_dir_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let _lock = crate::state::STATE_ENV_LOCK.lock().unwrap();
        let dir = temp_state_dir("private");
        std::env::set_var("LOFI_STATE_HOME", &dir);
        append("20260101_abcd", &record("ls"));
        let mode = std::fs::metadata(log_path("20260101_abcd").unwrap().parent().unwrap())
            .unwrap()
            .permissions();
        std::env::remove_var("LOFI_STATE_HOME");
        assert_eq!(mode.mode() & 0o777, 0o700);
    }
}
