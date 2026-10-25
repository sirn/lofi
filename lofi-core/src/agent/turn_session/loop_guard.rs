//! Loop detection: a reasoning pattern or tool cycle that repeats.
//!
//! The turn judges the pattern. Every thinking delta reaches the turn's sink
//! and the turn owns the history a cycle repeats in, so both transports get
//! the same check without code of their own.

use lofi_types::{ContentBlock, Message};

const THINKING_LOOP_MIN_BYTES: usize = 100;
const THINKING_LOOP_REPETITIONS: usize = 3;
const THINKING_LOOP_MAX_PERIOD_BYTES: usize = 2 * 1024;
const THINKING_LOOP_HISTORY_BYTES: usize = 24 * 1024;
const TOOL_LOOP_REPETITIONS: usize = 5;
const TOOL_LOOP_MAX_CYCLE: usize = 5;

#[derive(Default)]
pub(super) struct LoopGuard {
    thinking: Vec<u8>,
    thinking_checked_at: usize,
    tool_rounds: Vec<String>,
}

impl LoopGuard {
    /// A new round starts no loop: only what one round streams can repeat.
    pub(super) fn start_round(&mut self) {
        self.thinking.clear();
        self.thinking_checked_at = 0;
    }

    pub(super) fn add_thinking(&mut self, delta: &str) -> Option<String> {
        self.thinking.extend_from_slice(delta.as_bytes());
        if self.thinking.len() > THINKING_LOOP_HISTORY_BYTES {
            let excess = self.thinking.len() - THINKING_LOOP_HISTORY_BYTES;
            self.thinking.drain(..excess);
            self.thinking_checked_at = self.thinking_checked_at.saturating_sub(excess);
        }
        if self.thinking.len() < self.thinking_checked_at + THINKING_LOOP_MIN_BYTES {
            return None;
        }
        self.check_thinking()
    }

    pub(super) fn finish_thinking(&mut self) -> Option<String> {
        if self.thinking.len() == self.thinking_checked_at {
            return None;
        }
        self.check_thinking()
    }

    fn check_thinking(&mut self) -> Option<String> {
        self.thinking_checked_at = self.thinking.len();
        let relevant = THINKING_LOOP_REPETITIONS * THINKING_LOOP_MAX_PERIOD_BYTES;
        let start = self.thinking.len().saturating_sub(relevant);
        let reversed = self.thinking[start..]
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>();
        let prefix_matches = z_array(&reversed);
        let max_period =
            (reversed.len() / THINKING_LOOP_REPETITIONS).min(THINKING_LOOP_MAX_PERIOD_BYTES);
        for (period, prefix_match) in prefix_matches
            .iter()
            .enumerate()
            .take(max_period + 1)
            .skip(THINKING_LOOP_MIN_BYTES)
        {
            if *prefix_match >= period * (THINKING_LOOP_REPETITIONS - 1) {
                return Some(format!(
                    "repeated thinking pattern detected ({period} bytes repeated {THINKING_LOOP_REPETITIONS} times)"
                ));
            }
        }
        None
    }

    pub(super) fn add_tool_round(&mut self, fingerprint: String) -> Option<String> {
        self.tool_rounds.push(fingerprint);
        let keep = TOOL_LOOP_REPETITIONS * TOOL_LOOP_MAX_CYCLE;
        if self.tool_rounds.len() > keep {
            self.tool_rounds.remove(0);
        }
        for cycle in 1..=TOOL_LOOP_MAX_CYCLE {
            let repeated = cycle * TOOL_LOOP_REPETITIONS;
            if self.tool_rounds.len() < repeated {
                continue;
            }
            let tail = &self.tool_rounds[self.tool_rounds.len() - repeated..];
            if tail
                .chunks_exact(cycle)
                .all(|chunk| chunk == &tail[..cycle])
            {
                return Some(format!(
                    "repeated tool-result cycle detected ({cycle} round cycle repeated {TOOL_LOOP_REPETITIONS} times)"
                ));
            }
        }
        None
    }
}

fn z_array(bytes: &[u8]) -> Vec<usize> {
    let mut matches = vec![0; bytes.len()];
    let (mut left, mut right) = (0, 0);
    for index in 1..bytes.len() {
        if index < right {
            matches[index] = matches[index - left].min(right - index);
        }
        while index + matches[index] < bytes.len()
            && bytes[matches[index]] == bytes[index + matches[index]]
        {
            matches[index] += 1;
        }
        if index + matches[index] > right {
            left = index;
            right = index + matches[index];
        }
    }
    matches
}

pub(super) fn tool_round_fingerprint(assistant: &Message, result: &Message) -> String {
    use std::hash::{Hash, Hasher};

    let mut value = String::new();
    for block in &assistant.blocks {
        if let ContentBlock::ToolUse { name, input, .. } = block {
            value.push_str(name);
            value.push('\0');
            value.push_str(&serde_json::to_string(input).unwrap_or_default());
            value.push('\0');
        }
    }
    for block in &result.blocks {
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = block
        {
            value.push_str(if *is_error { "error" } else { "ok" });
            value.push('\0');
            value.push_str(content);
            value.push('\0');
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_loop_requires_three_repeated_long_suffixes() {
        let mut detector = LoopGuard::default();
        let pattern = "abcdefghij".repeat(10);
        assert!(detector.add_thinking(&pattern).is_none());
        assert!(detector.add_thinking(&pattern).is_none());
        assert!(detector
            .add_thinking(&pattern)
            .is_some_and(|detail| detail.contains("repeated thinking")));
    }

    #[test]
    fn thinking_loop_checks_a_short_final_delta() {
        let mut detector = LoopGuard::default();
        let pattern = "abcdefghij".repeat(10);
        assert!(detector
            .add_thinking(&format!("{}{}", pattern.repeat(2), &pattern[..1]))
            .is_none());
        assert!(detector.add_thinking(&pattern[1..]).is_none());
        assert!(detector
            .finish_thinking()
            .is_some_and(|detail| detail.contains("repeated thinking")));
    }

    #[test]
    fn thinking_loop_does_not_span_provider_rounds() {
        let mut detector = LoopGuard::default();
        let pattern = "abcdefghij".repeat(10);
        for _ in 0..3 {
            detector.start_round();
            assert!(detector.add_thinking(&pattern).is_none());
            assert!(detector.finish_thinking().is_none());
        }
    }

    #[test]
    fn thinking_loop_detects_varied_period_and_rejects_near_match() {
        let pattern = (0..137)
            .map(|index| char::from(b'!' + (index * 17 % 90) as u8))
            .collect::<String>();
        let mut near = format!("{pattern}{pattern}{pattern}");
        near.replace_range(near.len() - 1.., "~");

        let mut detector = LoopGuard::default();
        assert!(detector.add_thinking(&near).is_none());
        detector.start_round();
        assert!(detector
            .add_thinking(&pattern.repeat(3))
            .is_some_and(|detail| detail.contains("137 bytes")));
    }

    #[test]
    fn changing_tool_results_do_not_trigger_a_loop() {
        let mut detector = LoopGuard::default();
        for result in ["1", "2", "3", "4", "5"] {
            assert!(detector
                .add_tool_round(format!("same-call:{result}"))
                .is_none());
        }
    }

    #[test]
    fn identical_tool_result_rounds_trigger_after_five_repetitions() {
        let mut detector = LoopGuard::default();
        for _ in 0..TOOL_LOOP_REPETITIONS - 1 {
            assert!(detector.add_tool_round("same".into()).is_none());
        }
        assert!(detector
            .add_tool_round("same".into())
            .is_some_and(|detail| detail.contains("tool-result cycle")));
    }

    #[test]
    fn alternating_tool_result_cycle_is_detected() {
        let mut detector = LoopGuard::default();
        for fingerprint in ["a", "b"].into_iter().cycle().take(9) {
            assert!(detector.add_tool_round(fingerprint.into()).is_none());
        }
        assert!(detector
            .add_tool_round("b".into())
            .is_some_and(|detail| detail.contains("2 round cycle")));
    }
}
