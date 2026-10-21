//! The record decision of one turn: what is durable, and when.
//!
//! Two rules hold for every transport. The prompt is durable before the first
//! tool runs, and a message leaves the model history only after the transcript
//! has a boundary that explains it.

use lofi_error::Result;
use lofi_types::Message;

use super::super::turn::TurnDriver;
use super::super::AgentEvent;
use super::io::TurnIo;
use crate::session::recorder::TurnOutcome;

/// Commit the preceding response, append one prompt, record it, then announce
/// it and publish its range. Returns false when the consumer is gone.
///
/// Splitting the two checkpoints keeps each byte range with its matching
/// prompt row, and both are fatal: the turn must not ask the model for anything
/// the transcript does not hold.
pub(crate) async fn record_prompt(
    driver: &mut TurnDriver,
    io: &TurnIo,
    history: &mut Vec<Message>,
    message: Message,
) -> Result<bool> {
    let Some(AgentEvent::Prompt { prompt, kind }) = AgentEvent::from_prompt(&message) else {
        debug_assert!(false, "record_prompt requires a user-role prompt");
        return Ok(false);
    };
    io.commit_durable(history).await?;
    history.push(message);
    // Recorded but not published: the range belongs to the row the prompt
    // event opens, and that row does not exist yet. A prompt the transcript
    // cannot hold leaves the history again: live and durable stay in step when
    // the turn fails here.
    if let Err(error) = io.record(history) {
        history.pop();
        return Err(error);
    }
    if !driver.prompt(&prompt, kind).await {
        return Ok(false);
    }
    io.publish_pending().await;
    Ok(true)
}

/// Record a repair or auto-continue notice as the frame the next exchange
/// sends, so a continuation never resends the request it continues.
pub(crate) async fn record_notice(
    driver: &mut TurnDriver,
    io: &TurnIo,
    history: &mut Vec<Message>,
    message: Message,
) -> Result<bool> {
    let Some(AgentEvent::Prompt { prompt, kind }) = AgentEvent::from_prompt(&message) else {
        debug_assert!(false, "record_notice requires a user-role prompt");
        return Ok(false);
    };
    let recorded = record_prompt(driver, io, history, message).await;
    io.set_input(super::io::ExchangeInput {
        text: Some(prompt),
        kind,
        attachments: Vec::new(),
        notices: Vec::new(),
    });
    recorded
}

/// Record the round that is leaving the history, then let it go.
///
/// The checkpoint is fatal, so the credit it grants later slices is never
/// larger than what the transcript holds. A failure removes nothing: the
/// boundary is dropped and the caller keeps the round.
pub(crate) async fn discard_round(
    io: &TurnIo,
    history: &[Message],
    removed: usize,
    reason: &str,
) -> Result<()> {
    io.commit_durable(history).await?;
    let (range, failure) = {
        let mut guard = io.recorder();
        let Some(recorder) = guard.as_mut() else {
            return Ok(());
        };
        recorder.note_recorded_messages_removed(removed);
        match recorder.discard_round(reason) {
            Ok(range) => (range, None),
            Err(error) => {
                // No boundary reached the transcript, so the round is not
                // removed and the credit it granted must not survive.
                recorder.cancel_discard(removed);
                (None, Some(error))
            }
        }
    };
    // The marker is a row of the transcript: its bytes belong to the next
    // published range, or the frozen transcript cannot rebuild the round it
    // explains.
    io.unpublish(range);
    if let Some(error) = failure {
        // The marker is the boundary that explains the removal, so the turn
        // must not let the messages go: a live history ahead of the disk
        // resumes with messages the transcript cannot account for. The
        // recorder drops the marker, and the caller keeps the round it
        // already committed.
        return Err(error);
    }
    Ok(())
}

/// Flush the turn's remaining data with its outcome, and return the byte range
/// this call wrote.
pub(crate) fn finish(
    io: &TurnIo,
    history: &[Message],
    outcome: &TurnOutcome,
) -> Result<Option<(u64, u64)>> {
    let summary = io.events().accounting().summary(io.elapsed_ms());
    let mut guard = io.recorder();
    let Some(recorder) = guard.as_mut() else {
        return Ok(None);
    };
    let suffix = history.get(io.frontier().upto..).unwrap_or_default();
    let range = recorder.flush_incremental(suffix, outcome, &summary)?;
    // A transport that folded its own messages recorded bytes this turn never
    // published; the terminal range must cover them, or the frozen transcript
    // cannot rebuild the turn from disk.
    Ok(super::io::merge_range(io.take_unpublished(), range))
}
