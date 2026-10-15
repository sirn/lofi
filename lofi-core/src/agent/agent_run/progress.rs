#![allow(clippy::wildcard_imports)]

use super::*;

/// Checkpoint the turn suffix; a write failure is notified, not fatal.
/// The recorder keeps its counts, so the same suffix is retried by the
/// next checkpoint or the final flush.
pub(super) async fn commit_progress(
    recorder: Option<&mut SessionRecorder>,
    messages: &[Message],
    stats: &TurnStats,
    tx: &Sender<AgentEvent>,
) {
    if let Some((byte_start, byte_end)) = checkpoint_progress(recorder, messages, stats, tx).await {
        let _ = tx
            .send(AgentEvent::RoundCommitted {
                byte_start,
                byte_end,
            })
            .await;
    }
}

pub(super) async fn checkpoint_progress(
    recorder: Option<&mut SessionRecorder>,
    messages: &[Message],
    stats: &TurnStats,
    tx: &Sender<AgentEvent>,
) -> Option<(u64, u64)> {
    let recorder = recorder?;
    let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
    match recorder.checkpoint(messages, &stats.summary(elapsed_ms)) {
        Ok(range) => range,
        Err(error) => {
            let _ = emit(
                Some(tx),
                AgentEvent::Notice(format!(
                    "transcript write failed (will retry on the next round): {error}"
                )),
            )
            .await;
            None
        }
    }
}

/// Checkpoint the preceding response, then add one durable prompt and expose
/// the same transcript boundary live that replay derives from the message.
/// Splitting the checkpoints keeps each byte range with its matching turn.
pub(super) async fn append_prompt(
    messages: &mut Vec<Message>,
    prompt: Message,
    starts_run: bool,
    mut recorder: Option<&mut SessionRecorder>,
    prev_len: usize,
    stats: &TurnStats,
    tx: &Sender<AgentEvent>,
) -> bool {
    let Some(event) = AgentEvent::from_prompt(&prompt) else {
        debug_assert!(false, "append_prompt requires a user-role prompt");
        return false;
    };
    if let Some((byte_start, byte_end)) =
        checkpoint_progress(recorder.as_deref_mut(), &messages[prev_len..], stats, tx).await
    {
        if !emit(
            Some(tx),
            AgentEvent::RoundCommitted {
                byte_start,
                byte_end,
            },
        )
        .await
        {
            return false;
        }
    }
    messages.push(prompt);
    let range = checkpoint_progress(recorder, &messages[prev_len..], stats, tx).await;
    if starts_run && !emit(Some(tx), AgentEvent::RunStart).await {
        return false;
    }
    if !emit(Some(tx), event).await {
        return false;
    }
    if let Some((byte_start, byte_end)) = range {
        if !emit(
            Some(tx),
            AgentEvent::RoundCommitted {
                byte_start,
                byte_end,
            },
        )
        .await
        {
            return false;
        }
    }
    true
}

pub(super) async fn record_round_removal(
    recorder: Option<&mut SessionRecorder>,
    messages: &[Message],
    prev_len: usize,
    removed_count: usize,
    stats: &TurnStats,
    tx: &Sender<AgentEvent>,
    reason: &str,
) -> Result<()> {
    let Some(recorder) = recorder else {
        return Ok(());
    };
    let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
    let suffix = messages.get(prev_len..).unwrap_or_default();
    match recorder.checkpoint(suffix, &stats.summary(elapsed_ms)) {
        Ok(Some((byte_start, byte_end))) => {
            let _ = emit(
                Some(tx),
                AgentEvent::RoundCommitted {
                    byte_start,
                    byte_end,
                },
            )
            .await;
        }
        Ok(None) => {}
        Err(error) => {
            let _ = emit(
                Some(tx),
                AgentEvent::Notice(format!("transcript write failed: {error}")),
            )
            .await;
            return Err(error);
        }
    }
    // The checkpoint just counted the round the caller is about to remove;
    // without this credit, later slices would skip the first recovery
    // message — also on a boundary-write failure, since the checkpoint
    // already made the round durable.
    recorder.note_recorded_messages_removed(removed_count);
    if let Err(error) = recorder.discard_round(reason) {
        let _ = emit(
            Some(tx),
            AgentEvent::Notice(format!("discard boundary write failed: {error}")),
        )
        .await;
    }
    Ok(())
}
