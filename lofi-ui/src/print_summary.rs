//! One machine-readable record of a print-mode run.
//!
//! Print mode streams assistant text and nothing else, so the numbers a
//! caller budgets on - tokens, cost, rounds, retries, stop reason, wall
//! time - are lost unless the run reports them itself.

use lofi_core::AgentEvent;
use lofi_error::{Error, Result};
use lofi_types::{RunModel, StopReason, Usage};

/// Outcome of the run as the terminal event named it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Status {
    /// No terminal event: the run ended before the turn settled.
    #[default]
    Incomplete,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Incomplete => "incomplete",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// What a print-mode run reports about itself at exit. The consumer folds
/// every event of the run into this record, then writes it once.
#[derive(Debug, Clone, Default)]
pub(crate) struct PrintSummary {
    model: Option<RunModel>,
    status: Status,
    error: Option<String>,
    stop_reason: Option<StopReason>,
    /// Provider rounds of the turn; a tool loop reports one per round.
    rounds: u64,
    tool_calls: u64,
    retries: u64,
    /// Every round's tokens added up: a round event reports one round, and
    /// the terminal event reports only the final round.
    usage: Usage,
    /// The turn's accumulated USD cost, as last reported.
    cost: f64,
    /// Child spend, as deltas over the run.
    subagents: Usage,
    subagent_cost: f64,
    /// The session this run wrote or resumed; absent when it kept none.
    session: Option<String>,
}

impl PrintSummary {
    /// Name the model the agent resolved, so a run that fails before its
    /// first round still says what it was going to use.
    pub(crate) fn set_model(&mut self, model: RunModel) {
        self.model = Some(model);
    }

    /// Name the session a driver resumes this run by.
    pub(crate) fn set_session(&mut self, id: String) {
        self.session = Some(id);
    }

    /// Reconcile the record with the error the run returned. A consumer
    /// write failure, the context cap, and a setup error have no terminal
    /// event of their own.
    pub(crate) fn note_error(&mut self, error: &str) {
        if self.error.is_none() {
            self.error = Some(error.to_string());
        }
        // A cancelled run stays cancelled; every other error failed it.
        if self.status != Status::Cancelled {
            self.status = Status::Failed;
        }
    }

    pub(crate) fn note(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::RoundUsage { cost, usage } => {
                self.rounds += 1;
                add_usage(&mut self.usage, usage);
                self.cost = *cost;
            }
            AgentEvent::ToolStart { .. } => self.tool_calls += 1,
            AgentEvent::RetryStart { .. } => self.retries += 1,
            AgentEvent::SubagentUsage { cost, usage } => {
                self.subagent_cost += cost;
                add_usage(&mut self.subagents, usage);
            }
            AgentEvent::TurnEnd {
                model,
                cost,
                stop_reason,
                ..
            } => {
                self.model = Some(model.clone());
                self.status = Status::Completed;
                self.cost = *cost;
                self.stop_reason = *stop_reason;
            }
            AgentEvent::TurnFailed {
                model, error, cost, ..
            } => {
                self.model = Some(model.clone());
                self.status = Status::Failed;
                self.cost = *cost;
                self.error = Some(error.clone());
            }
            AgentEvent::TurnCancelled { model, cost, .. } => {
                self.model = Some(model.clone());
                self.status = Status::Cancelled;
                self.cost = *cost;
            }
            // The turn stopped at the context cap without settling; the
            // caller fails the run, and the record keeps the spend so far.
            AgentEvent::ContextPressure { cost, .. } => {
                self.cost = *cost;
            }
            AgentEvent::Error(message) => {
                self.status = Status::Failed;
                self.error = Some(message.clone());
            }
            _ => {}
        }
    }

    /// The record as JSON. `wall_ms` is the whole run: it covers a setup
    /// failure and a run that ended without a terminal event as well.
    pub(crate) fn record(&self, wall_ms: u64) -> serde_json::Value {
        let model = self.model.as_ref().map(|model| {
            serde_json::json!({
                "provider": model.provider,
                "id": model.id,
                "selected": model.selected,
                "thinking": model.thinking.as_str(),
                "service_tier": model.service_tier.as_str(),
            })
        });
        serde_json::json!({
            "ok": self.status == Status::Completed,
            "status": self.status.as_str(),
            "model": model,
            "rounds": self.rounds,
            "tool_calls": self.tool_calls,
            "retries": self.retries,
            "usage": usage_json(&self.usage, None),
            "cost": self.cost,
            "subagents": usage_json(&self.subagents, Some(self.subagent_cost)),
            "stop_reason": self.stop_reason.and_then(|reason| serde_json::to_value(reason).ok()),
            "session": self.session,
            "duration_ms": wall_ms,
            "error": self.error,
        })
    }

    /// Write the record as one JSON line into the file opened at startup.
    pub(crate) fn write(&self, file: &mut std::fs::File, wall_ms: u64) -> Result<()> {
        let mut line = serde_json::to_vec(&self.record(wall_ms))
            .map_err(|error| Error::State(format!("encode the run summary: {error}")))?;
        line.push(b'\n');
        // to_writer makes many small writes to the unbuffered file; one
        // buffer makes a half-written record after a signal rare.
        std::io::Write::write_all(file, &line)?;
        Ok(())
    }
}

/// One round reports its own tokens; the record adds every round up.
fn add_usage(total: &mut Usage, usage: &Usage) {
    total.input_tokens += usage.input_tokens;
    total.output_tokens += usage.output_tokens;
    total.cache_read_tokens += usage.cache_read_tokens;
    total.cache_write_tokens += usage.cache_write_tokens;
}

fn usage_json(usage: &Usage, cost: Option<f64>) -> serde_json::Value {
    let mut out = serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_read_tokens": usage.cache_read_tokens,
        "cache_write_tokens": usage.cache_write_tokens,
    });
    if let Some(cost) = cost {
        out["cost"] = serde_json::json!(cost);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary() -> PrintSummary {
        let mut summary = PrintSummary::default();
        summary.set_model(model());
        summary
    }

    fn model() -> RunModel {
        RunModel {
            provider: "p".to_string(),
            id: "m".to_string(),
            selected: "m".to_string(),
            ..RunModel::default()
        }
    }

    #[test]
    fn the_record_folds_rounds_tools_retries_and_child_spend() {
        let mut summary = summary();
        summary.note(&AgentEvent::RoundUsage {
            cost: 0.5,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 4,
                ..Usage::default()
            },
        });
        summary.note(&AgentEvent::RoundUsage {
            cost: 0.75,
            usage: Usage {
                input_tokens: 20,
                cache_read_tokens: 5,
                ..Usage::default()
            },
        });
        summary.note(&AgentEvent::ToolStart {
            id: "1".to_string(),
            name: "exec".to_string(),
        });
        summary.note(&AgentEvent::RetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 100,
            error: "transient".to_string(),
        });
        summary.note(&AgentEvent::SubagentUsage {
            cost: 0.25,
            usage: Usage {
                input_tokens: 7,
                ..Usage::default()
            },
        });
        summary.note(&AgentEvent::TurnEnd {
            model: model(),
            elapsed_ms: 1234,
            cost: 0.75,
            usage: Usage::default(),
            stop_reason: Some(StopReason::EndTurn),
        });

        let record = summary.record(9999);
        assert_eq!(record["ok"], json!(true), "{record}");
        assert_eq!(record["status"], json!("completed"), "{record}");
        assert_eq!(record["model"]["id"], json!("m"), "{record}");
        assert_eq!(record["rounds"], json!(2), "{record}");
        assert_eq!(record["tool_calls"], json!(1), "{record}");
        assert_eq!(record["retries"], json!(1), "{record}");
        assert_eq!(record["usage"]["input_tokens"], json!(30), "{record}");
        assert_eq!(record["usage"]["output_tokens"], json!(4), "{record}");
        assert_eq!(record["usage"]["cache_read_tokens"], json!(5), "{record}");
        assert_eq!(record["cost"], json!(0.75), "{record}");
        assert_eq!(record["subagents"]["input_tokens"], json!(7), "{record}");
        assert_eq!(record["subagents"]["cost"], json!(0.25), "{record}");
        assert_eq!(record["stop_reason"], json!("end_turn"), "{record}");
        assert_eq!(
            record["duration_ms"],
            json!(9999),
            "the record covers the whole run, not just the turn: {record}"
        );
        assert_eq!(record["error"], serde_json::Value::Null, "{record}");
        assert_eq!(
            record["session"],
            serde_json::Value::Null,
            "a run that kept no transcript names no session: {record}"
        );
    }

    #[test]
    fn the_record_names_the_session_a_driver_resumes_by() {
        let mut summary = summary();
        summary.set_session("abc123".to_string());
        summary.note(&AgentEvent::TurnEnd {
            model: model(),
            elapsed_ms: 10,
            cost: 0.1,
            usage: Usage::default(),
            stop_reason: Some(StopReason::EndTurn),
        });

        let record = summary.record(20);
        assert_eq!(record["session"], json!("abc123"), "{record}");
    }

    #[test]
    fn a_failed_run_records_its_error_and_the_wall_time() {
        let mut summary = summary();
        summary.note(&AgentEvent::TurnFailed {
            model: model(),
            elapsed_ms: 0,
            error: "boom".to_string(),
            cost: 0.1,
            usage: Usage::default(),
        });

        let record = summary.record(4321);
        assert_eq!(record["ok"], json!(false), "{record}");
        assert_eq!(record["status"], json!("failed"), "{record}");
        assert_eq!(record["error"], json!("boom"), "{record}");
        assert_eq!(record["duration_ms"], json!(4321), "{record}");
    }

    #[test]
    fn an_error_the_turn_never_named_fails_the_record() {
        let mut summary = summary();
        summary.note(&AgentEvent::TurnEnd {
            model: model(),
            elapsed_ms: 12,
            cost: 0.4,
            usage: Usage::default(),
            stop_reason: Some(StopReason::EndTurn),
        });
        // A broken stdout pipe fails the run after the turn settled.
        summary.note_error("write the stream: broken pipe");

        let record = summary.record(30);
        assert_eq!(record["ok"], json!(false), "{record}");
        assert_eq!(record["status"], json!("failed"), "{record}");
        assert_eq!(
            record["error"],
            json!("write the stream: broken pipe"),
            "{record}"
        );
        assert_eq!(record["cost"], json!(0.4), "{record}");
        assert_eq!(record["duration_ms"], json!(30), "{record}");
    }

    #[test]
    fn a_cancelled_run_keeps_its_status() {
        let mut summary = summary();
        summary.note(&AgentEvent::TurnCancelled {
            model: model(),
            elapsed_ms: 5,
            cost: 0.1,
            usage: Usage::default(),
        });
        summary.note_error("interrupted");

        let record = summary.record(0);
        assert_eq!(record["status"], json!("cancelled"), "{record}");
        assert_eq!(record["error"], json!("interrupted"), "{record}");
    }

    #[test]
    fn a_run_that_never_settled_reports_incomplete() {
        let mut summary = summary();
        summary.note(&AgentEvent::RoundUsage {
            cost: 0.2,
            usage: Usage::default(),
        });

        let record = summary.record(10);
        assert_eq!(record["ok"], json!(false), "{record}");
        assert_eq!(record["status"], json!("incomplete"), "{record}");
        assert_eq!(record["rounds"], json!(1), "{record}");
    }
}
