use std::sync::OnceLock;
use std::time::Duration;

use lofi_error::Error;
use regex::{Regex, RegexBuilder};

pub const DEFAULT_MAX_RETRIES: u32 = 10;
pub const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(2);
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_mins(1);

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
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

static NON_RETRYABLE: OnceLock<Regex> = OnceLock::new();

fn non_retryable() -> &'static Regex {
    NON_RETRYABLE.get_or_init(|| {
        build_pattern(&[
            "GoUsageLimitError",
            "FreeUsageLimitError",
            "Monthly usage limit reached",
            "available balance",
            "insufficient_quota",
            "out of budget",
            "quota exceeded",
            "billing",
            "invalid.?api.?key",
            "incorrect.?api.?key",
            "authentication",
            "unauthorized",
            r"\b401\b",
            r"\b403\b",
            "invalid.?request",
            r"\b400\b",
            "bad.?request",
            "context.?length",
            "context.?window",
            "maximum.?context",
            "too.?many.?tokens",
            "context.?overflow",
        ])
    })
}

static RETRYABLE: OnceLock<Regex> = OnceLock::new();

fn retryable() -> &'static Regex {
    RETRYABLE.get_or_init(|| {
        build_pattern(&[
            "overloaded",
            "rate.?limit",
            "too many requests",
            r"\b429\b",
            r"\b500\b",
            r"\b502\b",
            r"\b503\b",
            r"\b504\b",
            r"\b524\b",
            "service.?unavailable",
            "server.?error",
            "internal.?error",
            // Wrapper/provider text for transient upstream failures.
            "provider.?returned.?error",
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
            "ended without",
            "stream ended before",
            "http2 request did not get a response",
            "you can retry your request",
            "try your request again",
            "please retry your request",
            "ResourceExhausted",
        ])
    })
}

fn error_text(e: &Error) -> String {
    e.to_string()
}

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
        assert!(is_retryable_error(&Error::Provider(
            "HTTP 429 Too Many Requests".into()
        )));
        assert!(is_retryable_error(&Error::Provider(
            "HTTP 503 Service Unavailable from http://127.0.0.1:44037/v1/responses".into()
        )));
        assert!(is_retryable_error(&Error::Provider(
            "stream idle timeout".into()
        )));
        assert!(is_retryable_error(&Error::Provider(
            "stream ended before response.completed".into()
        )));
        assert!(is_retryable_error(&Error::Provider(
            "connection refused: upstream connect".into()
        )));
        assert!(is_retryable_error(&Error::Provider(
            "socket hang up".into()
        )));
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
        assert!(!is_retryable_error(&Error::Tool(
            "some tool failure".into()
        )));
    }

    #[test]
    fn backoff_doubles_and_clamps() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay_for(1), Duration::from_secs(2));
        assert_eq!(p.delay_for(2), Duration::from_secs(4));
        assert_eq!(p.delay_for(3), Duration::from_secs(8));
        assert_eq!(p.delay_for(6), Duration::from_mins(1));
        assert_eq!(p.delay_for(20), Duration::from_mins(1));
    }

    #[test]
    fn can_retry_respects_budget() {
        let p = RetryPolicy::default();
        assert!(p.can_retry(0));
        assert!(p.can_retry(9));
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
        assert_eq!(p.delay_for(5), Duration::from_secs(8));
        assert_eq!(p.delay_for(6), Duration::from_secs(10));
        assert!(!p.can_retry(5));
    }
}
