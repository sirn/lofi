//! Retry classification and backoff for provider errors.
//!
//! A failed assistant round whose error message matches a transient
//! provider/transport pattern (overloaded, rate limit, 429/5xx, network drops,
//! stream truncation) is retried with exponential backoff. Non-transient
//! failures (auth, quota/billing exhaustion, context overflow, bad requests)
//! are not retried — the caller surfaces them immediately.

use std::sync::OnceLock;
use std::time::Duration;

use lofi_error::Error;
use regex::{Regex, RegexBuilder};

/// Default cap on retry attempts (not counting the initial try).
pub const DEFAULT_MAX_RETRIES: u32 = 10;
/// Base delay for the first retry; subsequent retries double it.
pub const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(2);
/// Per-retry delay ceiling; the exponential backoff clamps here so a long
/// retry tail under persistent transient errors waits in bounded steps.
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_mins(1);

/// Per-call retry budget and backoff schedule.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Maximum retry attempts after the initial try.
    pub max_retries: u32,
    /// Base delay; attempt N (1-indexed) waits `base * 2^(N-1)`, clamped to
    /// `max_delay`.
    pub base_delay: Duration,
    /// Per-retry delay ceiling the exponential backoff never exceeds.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
        }
    }
}

impl From<lofi_types::RetryConfig> for RetryPolicy {
    fn from(c: lofi_types::RetryConfig) -> Self {
        Self {
            max_retries: c.max_retries,
            base_delay: Duration::from_millis(c.base_delay_ms),
            max_delay: Duration::from_millis(c.max_delay_ms),
        }
    }
}

impl RetryPolicy {
    /// Delay before the Nth retry (1-indexed): `base * 2^(n-1)`, clamped to
    /// `max_delay` so a long retry tail waits in bounded steps.
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        // Saturating shift so a very high attempt number can't overflow.
        let shift = attempt.saturating_sub(1).min(20);
        let raw = self
            .base_delay
            .checked_mul(1u32 << shift)
            .unwrap_or(self.base_delay);
        raw.min(self.max_delay)
    }

    /// Whether `attempt` (already-attempted retries) is still within budget.
    #[must_use]
    pub fn can_retry(&self, attempt: u32) -> bool {
        attempt < self.max_retries
    }
}

#[allow(clippy::expect_used)] // the patterns are compile-time constants; a build failure is a programmer error, not a runtime condition
fn build_pattern(patterns: &[&str]) -> Regex {
    RegexBuilder::new(&patterns.join("|"))
        .case_insensitive(true)
        .build()
        .expect("retry patterns are compile-time constants and must compile")
}

/// Patterns that look like permanent quota/billing/auth exhaustion — a retry
/// will not help.
static NON_RETRYABLE: OnceLock<Regex> = OnceLock::new();

fn non_retryable() -> &'static Regex {
    NON_RETRYABLE.get_or_init(|| {
        build_pattern(&[
            // OpenCode/Zen free-tier limits.
            "GoUsageLimitError",
            "FreeUsageLimitError",
            "Monthly usage limit reached",
            "available balance",
            // Generic quota/budget/billing exhaustion.
            "insufficient_quota",
            "out of budget",
            "quota exceeded",
            "billing",
            // Auth failures.
            "invalid.?api.?key",
            "incorrect.?api.?key",
            "authentication",
            "unauthorized",
            "401",
            "403",
            // Bad requests are deterministic.
            "invalid.?request",
            "400",
            "bad.?request",
            // Context overflow is handled by compaction, not retry.
            "context.?length",
            "context.?window",
            "maximum.?context",
            "too.?many.?tokens",
            "context.?overflow",
        ])
    })
}

/// Patterns that look like transient provider/transport failures.
static RETRYABLE: OnceLock<Regex> = OnceLock::new();

fn retryable() -> &'static Regex {
    RETRYABLE.get_or_init(|| {
        build_pattern(&[
            // Generic provider load, HTTP status, and server-side transient failures.
            "overloaded",
            "rate.?limit",
            "too many requests",
            "429",
            "500",
            "502",
            "503",
            "504",
            "524",
            "service.?unavailable",
            "server.?error",
            "internal.?error",
            // Wrapper/provider text for transient upstream failures.
            "provider.?returned.?error",
            // Network, proxy, and fetch transport failures.
            "network.?error",
            "connection.?error",
            "connection.?refused",
            "connection.?lost",
            "other side closed",
            "fetch failed",
            "upstream.?connect",
            "reset before headers",
            "socket hang up",
            "socket connection was closed",
            "timed? out",
            "timeout",
            "terminated",
            // Premature stream endings.
            "ended without",
            "stream ended before message_stop",
            "http2 request did not get a response",
            // Explicit retry guidance.
            "you can retry your request",
            "try your request again",
            "please retry your request",
            // gRPC ResourceExhausted (e.g. NVIDIA NIM).
            "ResourceExhausted",
        ])
    })
}

/// Extract a lowercase message string from an error for pattern matching.
fn error_text(e: &Error) -> String {
    // `to_string` includes the variant prefix (e.g. "provider error: ...");
    // the patterns above match the inner text, and some (like "timeout")
    // also match the prefix. Matching on the full display string covers both.
    e.to_string()
}

/// Classify whether `e` looks like a transient provider or transport error
/// that warrants an automatic retry.
///
/// Non-retryable patterns take precedence: a 429 that says `insufficient_quota`
/// is a billing failure, not a throttle.
#[must_use]
pub fn is_retryable_error(e: &Error) -> bool {
    if matches!(e, Error::Cancelled) {
        return false;
    }
    let text = error_text(e);
    if non_retryable().is_match(&text) {
        return false;
    }
    retryable().is_match(&text)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn retryable_provider_errors() {
        assert!(is_retryable_error(&Error::Provider("overloaded".into())));
        assert!(is_retryable_error(&Error::Provider("HTTP 429 Too Many Requests".into())));
        assert!(is_retryable_error(&Error::Provider("503 service unavailable".into())));
        assert!(is_retryable_error(&Error::Provider("stream idle timeout".into())));
        assert!(is_retryable_error(&Error::Provider(
            "connection refused: upstream connect".into()
        )));
        assert!(is_retryable_error(&Error::Provider("socket hang up".into())));
        assert!(is_retryable_error(&Error::Provider(
            "Please retry your request".into()
        )));
    }

    #[test]
    fn non_retryable_quota_and_auth_errors() {
        assert!(!is_retryable_error(&Error::Provider(
            "insufficient_quota: quota exceeded".into()
        )));
        assert!(!is_retryable_error(&Error::Provider(
            "Invalid API key".into()
        )));
        assert!(!is_retryable_error(&Error::Provider(
            "401 Unauthorized".into()
        )));
        assert!(!is_retryable_error(&Error::Provider(
            "context length exceeded".into()
        )));
    }

    #[test]
    fn non_error_variants_are_not_retryable() {
        assert!(!is_retryable_error(&Error::Cancelled));
        assert!(!is_retryable_error(&Error::Tool("some tool failure".into())));
    }

    #[test]
    fn backoff_doubles_and_clamps() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_for(1), Duration::from_secs(2));
        assert_eq!(p.delay_for(2), Duration::from_secs(4));
        assert_eq!(p.delay_for(3), Duration::from_secs(8));
        // 2 * 2^5 = 64s, clamped to the 60s ceiling.
        assert_eq!(p.delay_for(6), Duration::from_mins(1));
        // The ceiling holds for the whole tail.
        assert_eq!(p.delay_for(20), Duration::from_mins(1));
    }

    #[test]
    fn can_retry_respects_budget() {
        let p = RetryPolicy::default();
        assert!(p.can_retry(0));
        assert!(p.can_retry(9));
        // Default budget is 10 retries (after the initial try).
        assert!(!p.can_retry(10));
    }

    #[test]
    fn from_retry_config() {
        let p = RetryPolicy::from(lofi_types::RetryConfig {
            max_retries: 5,
            base_delay_ms: 500,
            max_delay_ms: 10_000,
        });
        assert_eq!(p.max_retries, 5);
        assert_eq!(p.base_delay, Duration::from_millis(500));
        assert_eq!(p.max_delay, Duration::from_secs(10));
        // 500ms * 2^4 = 8s, under the 10s ceiling.
        assert_eq!(p.delay_for(5), Duration::from_secs(8));
        // 500ms * 2^5 = 16s, clamped to 10s.
        assert_eq!(p.delay_for(6), Duration::from_secs(10));
        assert!(!p.can_retry(5));
    }
}