//! `QuickJS` tool bindings for the sandbox clock: `lofi.sleep` waits
//! inside the exec, `lofi.notifyAt`/`lofi.notifyEvery` schedule wake-up
//! notices on the session registry, and `lofi.notifyCancel` drops the
//! caller's pending ones.

use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use lofi_error::{Error, Result};

use crate::tools::{wait_for_cancel, BuiltinTools};

/// Longest `lofi.sleep`. A longer pause belongs to `notifyAt`, which wakes
/// the agent later without holding the exec open.
const MAX_SLEEP_SECS: f64 = 86_400.0;

/// Shortest `notifyEvery` interval. A shorter one would queue a notice on
/// every host poll and starve the turn it keeps waking.
const MIN_NOTIFY_EVERY: Duration = Duration::from_secs(1);

/// Longest wake-up message carried in a notice.
const MAX_MESSAGE_CHARS: usize = 500;

/// Maximum pending timers one subagent may hold. Bounds a runaway child
/// without capping the session's own timers, mirroring the job cap.
pub(super) const MAX_TIMERS_PER_OWNER: usize = 16;

impl BuiltinTools {
    /// `lofi.sleep(seconds)`: resolve after `seconds` seconds, observed
    /// against the run's cancellation flag so a cancelled turn leaves the
    /// wait at once.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] when `seconds` is not a non-negative number
    /// within [`MAX_SLEEP_SECS`], or when the run is cancelled mid-sleep.
    pub async fn sleep(&self, seconds: Value) -> Result<Value> {
        let secs = seconds
            .as_f64()
            .ok_or_else(|| Error::Tool("sleep: 'seconds' must be a number".into()))?;
        if !secs.is_finite() || secs < 0.0 {
            return Err(Error::Tool(
                "sleep: 'seconds' must be a non-negative number".into(),
            ));
        }
        if secs > MAX_SLEEP_SECS {
            return Err(Error::Tool(format!(
                "sleep: 'seconds' must be at most {} (one day); use notifyAt for longer waits",
                MAX_SLEEP_SECS as u64
            )));
        }
        let total = Duration::from_secs_f64(secs);
        if !total.is_zero() {
            let deadline = tokio::time::Instant::now() + total;
            match self.cancel.clone() {
                Some(cancel) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(deadline) => {}
                        () = wait_for_cancel(&cancel) => {
                            return Err(Error::Tool("sleep: run cancelled".into()));
                        }
                    }
                }
                None => tokio::time::sleep_until(deadline).await,
            }
        }
        Ok(json!({ "ok": true }))
    }

    /// `lofi.notifyAt(at, message?)`: queue a one-shot wake-up notice for
    /// the RFC 3339 timestamp `at`. A timestamp in the past fires on the
    /// next host poll.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] when `at` is not a parseable RFC 3339
    /// timestamp or `message` is not a string within [`MAX_MESSAGE_CHARS`].
    #[allow(clippy::unused_async)]
    pub async fn notify_at(&self, at: Value, message: Option<Value>) -> Result<Value> {
        let at = at
            .as_str()
            .ok_or_else(|| Error::Tool("notifyAt: 'at' must be a timestamp string".into()))?;
        let message = parse_message("notifyAt", message)?;
        let at_time = parse_timestamp(at).ok_or_else(|| {
            Error::Tool(
                "notifyAt: 'at' must be an RFC 3339 timestamp like 2026-01-01T09:00:00+00:00"
                    .into(),
            )
        })?;
        if !self.timer_slot_available() {
            return Ok(timer_cap_reached());
        }
        self.timers
            .schedule_at(self.job_owner.as_deref(), at_time, message.clone());
        Ok(json!({ "ok": true, "at": at, "message": message }))
    }

    /// `lofi.notifyEvery(every, message?)`: queue a repeating wake-up
    /// notice, first firing one `every` duration from now and re-armed past
    /// each fire. `every` is a duration like `5m` or `1h30m`; every segment
    /// carries a unit (`ms`, `s`, `m`, `h`, `d`).
    ///
    /// # Errors
    /// Returns [`Error::Tool`] when `every` is not a parseable duration at
    /// least [`MIN_NOTIFY_EVERY`] long, or `message` is not a string within
    /// [`MAX_MESSAGE_CHARS`].
    #[allow(clippy::unused_async)]
    pub async fn notify_every(&self, every: Value, message: Option<Value>) -> Result<Value> {
        let every = every
            .as_str()
            .ok_or_else(|| Error::Tool("notifyEvery: 'every' must be a duration string".into()))?;
        let message = parse_message("notifyEvery", message)?;
        let interval = parse_duration(every).ok_or_else(|| {
            Error::Tool("notifyEvery: 'every' must be a duration like 5m or 1h30m".into())
        })?;
        if interval < MIN_NOTIFY_EVERY {
            return Err(Error::Tool(
                "notifyEvery: 'every' must be at least 1s".into(),
            ));
        }
        if !self.timer_slot_available() {
            return Ok(timer_cap_reached());
        }
        self.timers
            .schedule_every(self.job_owner.as_deref(), interval, message.clone());
        Ok(json!({ "ok": true, "every": every, "message": message }))
    }

    /// `lofi.notifyCancel()`: drop every pending `notifyAt` and
    /// `notifyEvery` of this caller, returning how many were cancelled.
    ///
    /// # Errors
    /// Cannot fail; the signature stays uniform with the other bindings.
    #[allow(clippy::unused_async)]
    pub async fn notify_cancel(&self, _args: Value) -> Result<Value> {
        let cancelled = self.timers.cancel_owner(self.job_owner.as_deref());
        Ok(json!({ "ok": true, "cancelled": cancelled }))
    }

    /// Subagent timers stay bounded; the session's own are uncapped here,
    /// matching the job cap.
    fn timer_slot_available(&self) -> bool {
        let Some(owner) = self.job_owner.as_deref() else {
            return true;
        };
        self.timers.pending_for(Some(owner)) < MAX_TIMERS_PER_OWNER
    }
}

fn timer_cap_reached() -> Value {
    json!({
        "ok": false,
        "error": format!("subagent timer cap reached ({MAX_TIMERS_PER_OWNER} pending)"),
    })
}

fn parse_message(tool: &str, message: Option<Value>) -> Result<Option<String>> {
    let Some(message) = message else {
        return Ok(None);
    };
    if message.is_null() {
        return Ok(None);
    }
    let message = message
        .as_str()
        .ok_or_else(|| Error::Tool(format!("{tool}: 'message' must be a string")))?;
    if message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(Error::Tool(format!(
            "{tool}: 'message' must be at most {MAX_MESSAGE_CHARS} characters"
        )));
    }
    Ok(Some(message.to_owned()))
}

/// Parse an RFC 3339 timestamp, accepting the offset with or without a
/// colon (`+0000` and `+00:00` are both accepted).
fn parse_timestamp(at: &str) -> Option<SystemTime> {
    let normalized = normalize_offset(at);
    let dt = chrono::DateTime::parse_from_rfc3339(&normalized).ok()?;
    Some(SystemTime::from(dt.with_timezone(&chrono::Utc)))
}

fn normalize_offset(at: &str) -> String {
    if at.len() < 5 {
        return at.to_owned();
    }
    let tail_start = at.len() - 5;
    let tail = &at.as_bytes()[tail_start..];
    if matches!(tail[0], b'+' | b'-') && tail[1..].iter().all(u8::is_ascii_digit) {
        let (head, mins) = at.split_at(tail_start + 3);
        return format!("{head}:{mins}");
    }
    at.to_owned()
}

/// Parse a duration like `5m`, `1h30m`, or `500ms`. Every segment carries
/// a unit: `ms`, `s`, `m`, `h`, or `d`.
fn parse_duration(text: &str) -> Option<Duration> {
    let mut rest = text;
    let mut total = Duration::ZERO;
    let mut seen = false;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let value: f64 = rest[..digits].parse().ok()?;
        // The bound keeps `mul_f64` below any Duration overflow.
        if !value.is_finite() || !(0.0..=1_000_000.0).contains(&value) {
            return None;
        }
        rest = &rest[digits..];
        let (unit, width) = if rest.starts_with("ms") {
            (Duration::from_millis(1), 2)
        } else {
            match rest.as_bytes().first()? {
                b's' => (Duration::from_secs(1), 1),
                b'm' => (Duration::from_mins(1), 1),
                b'h' => (Duration::from_hours(1), 1),
                b'd' => (Duration::from_hours(24), 1),
                _ => return None,
            }
        };
        rest = &rest[width..];
        total = total.checked_add(unit.mul_f64(value))?;
        seen = true;
    }
    seen.then_some(total)
}
