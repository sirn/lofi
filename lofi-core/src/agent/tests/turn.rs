#![allow(clippy::unwrap_used)]

use super::*;
use crate::agent::turn::TurnDriver;
use crate::agent::turn_session::terminal_assistant;

/// Collect everything the driver published.
fn drain(rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn a_turn_is_announced_once_however_many_prompts_it_carries() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
    let mut driver = TurnDriver::new(tx.clone());
    assert!(
        driver
            .prompt("notice", lofi_types::PromptKind::Notice)
            .await
    );
    assert!(driver.prompt("asked", lofi_types::PromptKind::User).await);

    let events = drain(&mut rx);
    let starts = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::RunStart))
        .count();
    assert_eq!(starts, 1, "{events:?}");
    assert!(matches!(&events[0], AgentEvent::RunStart));
    assert!(matches!(
        &events[1],
        AgentEvent::Prompt { prompt, .. } if prompt == "notice"
    ));
    assert!(matches!(
        &events[2],
        AgentEvent::Prompt { prompt, .. } if prompt == "asked"
    ));
}

#[tokio::test]
async fn a_continuation_announces_itself_and_publishes_no_prompt() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
    let mut driver = TurnDriver::new(tx.clone());
    assert!(driver.continuation().await);

    let events = drain(&mut rx);
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(&events[0], AgentEvent::TurnContinue));
}

#[test]
fn the_terminal_answer_is_the_last_assistant_message_of_the_round() {
    let user = user_msg("go");
    let reasoning = Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::Thinking {
            text: "weighing options".into(),
            signature: Some("signature".into()),
            redacted: false,
        }],
        kind: PromptKind::User,
    };
    let answer = Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: "final answer".into(),
        }],
        kind: PromptKind::User,
    };
    // A provider round ends with its answer, which is also the round's first
    // message.
    let native = vec![user.clone(), reasoning.clone()];
    assert_eq!(terminal_assistant(&native, 1), Some(1));
    // A CLI child flushes the reasoning before its tool cycle and answers
    // after it, so the round does not start with the message that answered.
    let cli = vec![user, reasoning, answer];
    assert_eq!(terminal_assistant(&cli, 1), Some(2));
    // A round that produced no assistant message has no answer to inspect.
    assert_eq!(terminal_assistant(&cli[..1], 1), None);
}

#[tokio::test]
async fn a_closed_consumer_stops_the_turn_before_its_first_event() {
    let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
    drop(rx);
    let mut driver = TurnDriver::new(tx.clone());
    assert!(!driver.prompt("asked", lofi_types::PromptKind::User).await);
}
