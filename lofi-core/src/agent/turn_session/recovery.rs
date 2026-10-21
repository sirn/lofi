//! The repair rules of a turn that keeps talking to the model.
//!
//! These are the loop's decisions, not a transport's: a repeated reasoning
//! pattern, a response that held only thinking, a terminal stop the model did
//! not mean. Every transport reports the fact; only this file decides what to
//! do about it.

use lofi_types::{ContentBlock, Message, PromptKind, Role, StopReason};

pub(super) use super::super::THINKING_ONLY_CONTINUATION_PROMPT as THINKING_ONLY_PROMPT;
use super::super::{
    INTENT_CONTINUATION_PROMPT, LOST_TOOL_CONTINUATION_PROMPT, TRUNCATION_CONTINUATION_PROMPT,
};
use super::contract::Exchange;

pub(in crate::agent) const MAX_RECOVERY_ROUNDS: usize = 100;
const LOOP_RECOVERY_PROMPT: &str = "A potential loop was detected in your repeated reasoning or tool results. Stop repeating the same approach. Review the latest result, choose a different concrete action, and continue only if it makes forward progress.";

pub(super) fn notice_message(text: impl Into<String>) -> Message {
    Message {
        origin: None,
        role: Role::User,
        blocks: vec![ContentBlock::Text { text: text.into() }],
        kind: PromptKind::Notice,
    }
}

/// The continuation a terminal stop earns, if the policy grants one.
pub(super) fn terminal_recovery_prompt(
    exchange: &Exchange,
    announced_tool_intent: bool,
    policy: lofi_types::AutoContinuePolicy,
) -> Option<&'static str> {
    if !exchange.finished {
        return None;
    }
    match exchange.stop_reason {
        Some(StopReason::MaxTokens) => Some(TRUNCATION_CONTINUATION_PROMPT),
        Some(StopReason::ToolUse) if policy.lost_tool_call => Some(LOST_TOOL_CONTINUATION_PROMPT),
        Some(StopReason::EndTurn) if policy.intent && announced_tool_intent => {
            Some(INTENT_CONTINUATION_PROMPT)
        }
        _ => None,
    }
}

fn loop_recovery_message(detail: &str) -> Message {
    notice_message(format!("{LOOP_RECOVERY_PROMPT} Detection: {detail}"))
}

pub(super) enum RecoveryAction {
    Continue,
    Stop,
}

/// Bound the number of recovery rounds in one turn. Returns the notice that
/// ends the turn once the budget is gone.
pub(super) fn recovery_round_exceeded(recovery_rounds: &mut usize) -> Option<Message> {
    *recovery_rounds += 1;
    if *recovery_rounds <= MAX_RECOVERY_ROUNDS {
        return None;
    }
    Some(notice_message(format!(
        "agent stopped after {MAX_RECOVERY_ROUNDS} consecutive recovery rounds without forward progress"
    )))
}

pub(super) struct RecoveryPlan {
    pub(super) action: RecoveryAction,
    /// The history index to remove, with the reason the transcript records.
    pub(super) removal: Option<(usize, String)>,
    pub(super) notice: Message,
}

pub(super) fn handle_loop_detection(
    detail: Option<&str>,
    interrupted_thinking_index: Option<usize>,
    recovered: &mut bool,
) -> Option<RecoveryPlan> {
    let detail = detail?;
    if *recovered {
        let detail = format!("agent stopped after loop recovery failed: {detail}");
        return Some(RecoveryPlan {
            action: RecoveryAction::Stop,
            removal: interrupted_thinking_index.map(|index| (index, detail.clone())),
            notice: notice_message(detail),
        });
    }
    *recovered = true;
    let notice =
        format!("potential agent loop detected; requesting a different approach: {detail}");
    Some(RecoveryPlan {
        action: RecoveryAction::Continue,
        removal: interrupted_thinking_index.map(|index| (index, notice)),
        notice: loop_recovery_message(detail),
    })
}

pub(super) fn handle_thinking_only_stop(
    thinking_only_index: Option<usize>,
    continuation_prompt: Option<&str>,
    recovered: &mut bool,
) -> Option<RecoveryPlan> {
    let index = thinking_only_index?;
    let (action, detail, prompt) = match (continuation_prompt, *recovered) {
        (None, _) => (
            RecoveryAction::Stop,
            "discarding thinking-only response after terminal provider stop",
            "Agent stopped because the provider ended generation before a final answer.",
        ),
        (Some(_), true) => (
            RecoveryAction::Stop,
            "discarding repeated thinking-only response after automatic continuation",
            "Agent stopped after the continuation also produced reasoning without a final answer.",
        ),
        (Some(prompt), false) => {
            *recovered = true;
            (
                RecoveryAction::Continue,
                "discarding thinking-only response before automatic continuation",
                prompt,
            )
        }
    };
    Some(RecoveryPlan {
        action,
        removal: Some((index, detail.to_string())),
        notice: notice_message(prompt),
    })
}
