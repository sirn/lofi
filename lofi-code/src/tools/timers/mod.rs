//! Session-scoped wake-up timers behind `lofi.sleep`, `lofi.notifyAt`,
//! `lofi.notifyEvery`, and `lofi.notifyCancel`. Scheduling registers a
//! timer here; when it comes due, its notice text leaves through
//! [`TimerRegistry::take_notices`] and the host hands it to the agent the
//! same way job notices are handed over. Nothing is persisted: timers live
//! and die with the session registry.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

mod bindings;
#[cfg(test)]
mod tests;

#[derive(Debug)]
struct Timer {
    /// Identity for the host's timer list; scheduling order, never reused.
    id: u64,
    /// Next fire time. A `notifyAt` in the past fires on the next poll.
    due: SystemTime,
    /// `Some` for `notifyEvery`, which re-arms past every fire.
    every: Option<Duration>,
    owner: Option<String>,
    message: Option<String>,
}

impl Timer {
    fn notice_text(&self) -> String {
        let kind = if self.every.is_some() {
            "notifyEvery"
        } else {
            "notifyAt"
        };
        match &self.message {
            Some(message) => format!("{kind} fired: {message}"),
            None => format!("{kind} fired"),
        }
    }
}

/// One pending timer as the host lists it. `due` is absolute so the host
/// can count down without a clock of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimerInfo {
    pub id: u64,
    pub due: SystemTime,
    /// `Some` for `notifyEvery`: the interval between fires.
    pub every: Option<Duration>,
    pub message: Option<String>,
    pub owner: Option<String>,
}

/// Per-session registry of wake-up timers. Cheap to clone; every clone
/// shares the same list, so the exec-time tool bundle, the agent that owns
/// the session, and the host that polls notices see the same timers.
#[derive(Clone, Debug, Default)]
pub struct TimerRegistry {
    timers: Arc<Mutex<Vec<Timer>>>,
    next_id: Arc<AtomicU64>,
}

impl TimerRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a one-shot `notifyAt` wake-up. An `at` in the past fires on
    /// the next [`take_notices`](Self::take_notices).
    pub fn schedule_at(&self, owner: Option<&str>, at: SystemTime, message: Option<String>) {
        self.push(Timer {
            id: 0,
            due: at,
            every: None,
            owner: owner.map(str::to_owned),
            message,
        });
    }

    /// Register a repeating `notifyEvery` wake-up whose first fire is one
    /// interval from now. Callers must pass a non-zero `every`.
    pub fn schedule_every(&self, owner: Option<&str>, every: Duration, message: Option<String>) {
        self.push(Timer {
            id: 0,
            due: SystemTime::now() + every,
            every: Some(every),
            owner: owner.map(str::to_owned),
            message,
        });
    }

    /// Pending timers of `owner`, both kinds. `None` counts the timers no
    /// owner claimed (the session's own).
    #[must_use]
    pub fn pending_for(&self, owner: Option<&str>) -> usize {
        self.locked(|timers| {
            timers
                .iter()
                .filter(|t| t.owner.as_deref() == owner)
                .count()
        })
    }

    /// Every pending timer, soonest first. The host renders one row each.
    #[must_use]
    pub fn listing(&self) -> Vec<TimerInfo> {
        self.locked(|timers| {
            let mut out: Vec<TimerInfo> = timers
                .iter()
                .map(|timer| TimerInfo {
                    id: timer.id,
                    due: timer.due,
                    every: timer.every,
                    message: timer.message.clone(),
                    owner: timer.owner.clone(),
                })
                .collect();
            out.sort_by_key(|timer| timer.due);
            out
        })
    }

    /// Drop the timer `id`, whether one-shot or repeating. Returns whether
    /// a timer was there to drop.
    #[must_use]
    pub fn cancel(&self, id: u64) -> bool {
        self.locked(|timers| {
            let before = timers.len();
            timers.retain(|timer| timer.id != id);
            timers.len() != before
        })
    }

    /// Drop every pending timer, every owner included. A `/new` session
    /// must not be woken by the previous conversation's timers.
    pub fn reset(&self) {
        self.locked(Vec::clear);
    }

    /// Drop every pending timer of `owner`, both `notifyAt` and
    /// `notifyEvery`, returning how many were cancelled.
    #[must_use]
    pub fn cancel_owner(&self, owner: Option<&str>) -> usize {
        self.locked(|timers| {
            let before = timers.len();
            timers.retain(|t| t.owner.as_deref() != owner);
            before - timers.len()
        })
    }

    /// Take every notice due now, oldest first. One-shot timers are removed;
    /// `notifyEvery` timers advance to their next future slot, so a host that
    /// polls late gets one notice per timer, never a burst of the missed
    /// intervals.
    #[must_use]
    pub fn take_notices(&self) -> Vec<String> {
        self.locked(|timers| {
            let now = SystemTime::now();
            let mut fired = Vec::new();
            timers.retain_mut(|timer| {
                if timer.due > now {
                    return true;
                }
                fired.push((timer.due, timer.notice_text()));
                match timer.every {
                    // Arithmetically past `now`, so a long-unpolled gap
                    // coalesces instead of looping once per missed interval.
                    Some(every) => {
                        let behind = now.duration_since(timer.due).unwrap_or_default();
                        let steps = u32::try_from(behind.as_nanos() / every.as_nanos().max(1))
                            .unwrap_or(u32::MAX)
                            .saturating_add(1);
                        timer.due += every.saturating_mul(steps);
                        true
                    }
                    None => false,
                }
            });
            fired.sort_by_key(|(due, _)| *due);
            fired.into_iter().map(|(_, text)| text).collect()
        })
    }

    fn push(&self, mut timer: Timer) {
        timer.id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.locked(|timers| timers.push(timer));
    }

    fn locked<R>(&self, f: impl FnOnce(&mut Vec<Timer>) -> R) -> R {
        f(&mut self
            .timers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}
