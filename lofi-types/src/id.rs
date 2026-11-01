//! The one handle format for work the agent owns: jobs, subagents, and
//! timers.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Longest handle kept as one. A clock-seeded handle is thirteen characters;
/// the cap rejects an id of the older format -- nineteen decimal digits of
/// the clock -- read back as base 36.
const MAX_CHARS: usize = 15;

/// Bits a session draws at random when it seeds. Two sessions that reach the
/// same microsecond still start thousands of handles apart.
const ENTROPY_BITS: u32 = 12;

/// One clock tick per handle. A step clears the random bits, so the counter
/// never carries into them and two handles of one session cannot meet.
const STRIDE: u64 = 1 << ENTROPY_BITS;

const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// The handle source a session shares across its jobs, subagents, and
/// timers. One counter is what keeps a handle naming exactly one of them:
/// two counters could each mint the same one, and no reader could tell which
/// was meant.
///
/// The counter starts at the clock in microseconds, with random low bits from
/// the session, so two sessions seed apart even when they start together. The
/// handles sort by the time the work started.
#[derive(Debug, Clone)]
pub struct IdSpace(Arc<AtomicU64>);

impl Default for IdSpace {
    fn default() -> Self {
        let micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
        Self(Arc::new(AtomicU64::new(
            micros.saturating_mul(STRIDE) | entropy(),
        )))
    }
}

/// A random number below [`STRIDE`]. `RandomState` seeds itself from the
/// system, which costs no dependency of its own.
fn entropy() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u8(0);
    hasher.finish() & (STRIDE - 1)
}

impl IdSpace {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The next handle: one clock tick past the seed, then one per call.
    #[must_use]
    pub fn next(&self) -> u64 {
        self.0.fetch_add(STRIDE, Ordering::SeqCst) + STRIDE
    }
}

/// A handle, base 36: about five bits per character, lowercase, no padding.
#[must_use]
pub fn short_id(id: u64) -> String {
    let mut out = String::new();
    let mut left = id;
    loop {
        #[allow(clippy::cast_possible_truncation)]
        let digit = DIGITS[(left % 36) as usize];
        out.insert(0, char::from(digit));
        left /= 36;
        if left == 0 {
            return out;
        }
    }
}

/// Read a handle back. Case-insensitive, because a model that shouts an id
/// still means it.
#[must_use]
pub fn parse_short_id(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_CHARS {
        return None;
    }
    text.chars().try_fold(0_u64, |acc, c| {
        let digit = c.to_digit(36)?;
        acc.checked_mul(36)?.checked_add(u64::from(digit))
    })
}

#[cfg(test)]
mod tests {
    use super::{entropy, parse_short_id, short_id, IdSpace, STRIDE};

    #[test]
    fn handles_count_up_in_base_36() {
        assert_eq!(short_id(1), "1");
        assert_eq!(short_id(10), "a");
        assert_eq!(short_id(35), "z");
        assert_eq!(short_id(36), "10");
        assert_eq!(short_id(1_679_615), "zzzz");
    }

    #[test]
    fn a_handle_reads_back() {
        for id in [1, 9, 36, 1295, 1_679_615, 1_793_519_200_510_057_157] {
            assert_eq!(parse_short_id(&short_id(id)), Some(id));
        }
        assert_eq!(parse_short_id("1F"), Some(51), "shouting still works");
    }

    #[test]
    fn an_older_id_is_not_a_handle() {
        // Nineteen decimal digits of the clock: one character too wide.
        assert_eq!(parse_short_id("1793519200510057157"), None);
        assert_eq!(parse_short_id(""), None);
        assert_eq!(parse_short_id("  "), None);
        assert_eq!(parse_short_id("1!"), None);
    }

    #[test]
    fn a_clock_seed_is_thirteen_characters() {
        assert_eq!(short_id(1_793_641_087_000_000 * STRIDE).len(), 13);
    }

    #[test]
    fn a_space_counts_up_from_the_clock() {
        let space = IdSpace::new();
        let first = space.next();
        // Past 2024, so the seed came from the clock rather than from zero.
        assert!(first > 1_700_000_000_000_000_000, "{first} is clock-seeded");
        assert_eq!(space.next(), first + STRIDE);
        let twin = space.clone();
        assert_eq!(
            twin.next(),
            first + 2 * STRIDE,
            "clones advance one counter"
        );
    }

    #[test]
    fn a_step_keeps_the_random_bits() {
        let space = IdSpace::new();
        let (first, second) = (space.next(), space.next());
        assert_eq!(first % STRIDE, second % STRIDE, "a step never carries");
        assert!(entropy() < STRIDE);
    }
}
