//! The auto-mode decision-gate calibration command.
//!
//! Three inputs, one judge. A replay re-judges the evaluations the gate
//! appended to its log, so it needs no network call and no special
//! logging. A probe sends a labeled corpus through the live decision
//! model and archives the raw scores as JSONL. Records judge a saved
//! archive offline, or with sweep grid-search thresholds against it.
//!
//! A corpus line is allow-or-ask, a colon, and the command. Blank lines
//! and hash comments are skipped. Probe records carry the label the
//! gate should have produced; log records carry the outcome it did.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use lofi_code::policy::decision::{self, Thresholds};
use lofi_code::{AutoModeHints, AutoModeOutcome};
use lofi_types::{Config, DecisionAnswer};

use crate::eval_log::{outcome_label, scores_from_answers, Record, Scores};

use lofi_error::{Error, Result};

/// One probe call may wait this long before the corpus line fails. The
/// interactive gate uses the same figure.
const PROBE_TIMEOUT_MS: u64 = 30_000;

/// The CLI shape of lofi policy calibrate.
#[derive(Debug, Default)]
pub struct CalibrateArgs {
    /// JSONL evaluation archives to rejudge, no network call. An empty
    /// list replays every session's live evaluation log.
    pub replay: Vec<PathBuf>,
    /// A labeled corpus to send through the live decision model.
    pub probe: Option<PathBuf>,
    /// Saved JSONL record archives to judge offline.
    pub records: Vec<PathBuf>,
    /// Grid-search thresholds over the records instead of judging them
    /// once.
    pub sweep: bool,
    /// The decision model query. Defaults to the config's
    /// `auto_mode.decision_model`.
    pub model: Option<String>,
    /// The request text the corpus commands answer, as the turn would.
    pub request: Option<String>,
    /// The intent text `build_state` would read from the call.
    pub intent: Option<String>,
    /// Where to write the judged records as JSONL.
    pub out: Option<PathBuf>,
    /// How many times to send each corpus line. A repeat spreads the
    /// model's answer variance.
    pub repeat: usize,
    /// Threshold overrides. A set field replaces both the calibrated
    /// default and the config's `auto_mode.decision` value.
    pub thresholds: lofi_types::DecisionThresholdsConfig,
}

/// Everything one probe call needs, so the call itself stays under the
/// argument limit.
struct Probe {
    client: std::sync::Arc<dyn lofi_providers::Decision>,
    questions: BTreeMap<String, lofi_types::DecisionQuestion>,
    cwd: PathBuf,
    hints: AutoModeHints,
    model: String,
    intent: Option<String>,
    request: Option<String>,
}

/// # Errors
/// Returns a config error when no input mode is named or more than one
/// is, and io or model errors from the chosen mode.
pub async fn calibrate(args: CalibrateArgs, config_path: &Path) -> Result<()> {
    let modes = [
        !args.replay.is_empty(),
        args.probe.is_some(),
        !args.records.is_empty(),
    ]
    .iter()
    .filter(|set| **set)
    .count();
    if modes > 1 {
        return Err(Error::Config(
            "calibrate: name at most one of --replay, --probe, or --records".to_string(),
        ));
    }
    if args.sweep && args.records.is_empty() {
        return Err(Error::Config(
            "calibrate: --sweep judges --records".to_string(),
        ));
    }
    let config = crate::config_loader::load_config_or_default(config_path)
        .await
        .map_err(|e| Error::Config(format!("load config: {e}")))?;
    let thresholds = resolve_thresholds(&config, &args);
    if let Some(corpus) = args.probe.clone() {
        probe(&args, &config, &thresholds, &corpus).await?;
    } else if !args.records.is_empty() {
        if args.sweep {
            sweep(&args)?;
        } else {
            report_records(&args, &thresholds)?;
        }
    } else {
        replay(&args, &thresholds)?;
    }
    Ok(())
}

/// Merge the config's `auto_mode.decision` with the CLI overrides: a CLI
/// flag replaces what the file set, an unset flag keeps it, and an
/// unset field keeps the calibrated default.
fn resolve_thresholds(config: &Config, args: &CalibrateArgs) -> Thresholds {
    let file = config
        .shell_policy
        .auto_mode
        .as_ref()
        .and_then(|mode| mode.decision.as_ref());
    let file_or_default = file.map(Thresholds::from_config).unwrap_or_default();
    Thresholds {
        wide_scope_mass: args
            .thresholds
            .wide_scope_mass
            .unwrap_or(file_or_default.wide_scope_mass),
        destructive: args
            .thresholds
            .destructive
            .unwrap_or(file_or_default.destructive),
        exposure: args.thresholds.exposure.unwrap_or(file_or_default.exposure),
        authorized: args
            .thresholds
            .authorized
            .unwrap_or(file_or_default.authorized),
        known: args.thresholds.known.unwrap_or(file_or_default.known),
    }
}

/// Build the answer set `decide_with` reads from the scores. The scope
/// probabilities put the non-wide remainder on the picked option: decide
/// reads only the two wide masses and the pick, so the split below them
/// cannot change the outcome.
fn answers(scores: &Scores) -> BTreeMap<String, DecisionAnswer> {
    let mut probabilities = BTreeMap::new();
    probabilities.insert("system".to_string(), scores.system);
    probabilities.insert("remote".to_string(), scores.remote);
    let rest = (1.0 - scores.system - scores.remote).max(0.0);
    let key = scores.scope.clone();
    *probabilities.entry(key).or_insert(0.0) += rest;
    let noul = |p: f64| DecisionAnswer::Noul { noul: p };
    BTreeMap::from([
        (
            "scope".to_string(),
            DecisionAnswer::Choice {
                choice: scores.scope.clone(),
                probabilities,
                confidence: Some(0.9),
            },
        ),
        ("destructive".to_string(), noul(scores.destructive)),
        ("exposure".to_string(), noul(scores.exposure)),
        ("authorized".to_string(), noul(scores.authorized)),
        ("recognized".to_string(), noul(scores.known)),
    ])
}

fn judge(scores: &Scores, thresholds: &Thresholds) -> AutoModeOutcome {
    decision::decide_with(&answers(scores), thresholds)
}

fn outcome_allows(outcome: &AutoModeOutcome) -> bool {
    matches!(outcome, AutoModeOutcome::Allow { .. })
}

/// The gate's justification for one outcome: the scores it read and the
/// threshold that decided.
fn reason(outcome: &AutoModeOutcome) -> &str {
    match outcome {
        AutoModeOutcome::Allow { reason }
        | AutoModeOutcome::Ask { reason }
        | AutoModeOutcome::HighRisk { reason }
        | AutoModeOutcome::Failed { reason } => reason,
    }
}

/// Replay the evaluations the gate appended, or the archives named on the
/// command line. An empty file list reads every session's live log.
fn replay(args: &CalibrateArgs, thresholds: &Thresholds) -> Result<()> {
    let files = if args.replay.is_empty() {
        let logs = crate::eval_log::session_logs()?;
        if logs.is_empty() {
            return Err(Error::Config(
                "calibrate: no session has a gate evaluation yet".to_string(),
            ));
        }
        logs
    } else {
        args.replay.clone()
    };
    let records = read_records(&files)?;
    let mut out = args.out.as_deref().map(open_jsonl).transpose()?;
    let stdout = std::io::stdout();
    let mut report = stdout.lock();
    let (mut allowed, mut asked, mut failed) = (0, 0, 0);
    let (mut to_ask, mut to_allow) = (0, 0);
    for (i, record) in records.iter().enumerate() {
        let outcome = judge(&record.scores, thresholds);
        match record.label.as_str() {
            "Allow" => allowed += 1,
            "Ask" | "HighRisk" => asked += 1,
            _ => failed += 1,
        }
        // A flip is the line worth scanning for, so the table marks it.
        let flipped = outcome_allows(&outcome) != (record.label == "Allow");
        if flipped {
            if outcome_allows(&outcome) {
                to_allow += 1;
            } else {
                to_ask += 1;
            }
        }
        let head: String = record.command.chars().take(110).collect();
        writeln!(
            report,
            "{:>3} {} {:>8} -> {:<8} {}",
            i + 1,
            if flipped { '!' } else { ' ' },
            record.label,
            outcome_label(&outcome),
            head
        )?;
        // The gate's own reason carries every score it read and the
        // threshold that decided, so a tuned replay can be read without
        // re-running the model.
        writeln!(report, "      {}", reason(&outcome))?;
        write_record(out.as_mut(), record)?;
    }
    writeln!(
        report,
        "\n{} records: logged {} allow / {} ask / {} other; replay flips {} to ask, {} to allow",
        records.len(),
        allowed,
        asked,
        failed,
        to_ask,
        to_allow
    )?;
    Ok(())
}

/// Send the corpus through the live decision model.
async fn probe(
    args: &CalibrateArgs,
    config: &Config,
    thresholds: &Thresholds,
    corpus_path: &Path,
) -> Result<()> {
    let corpus = read_corpus(corpus_path)?;
    let (client, model) = open_decision_client(config, args.model.as_deref()).await?;
    let probe = Probe {
        client,
        questions: decision::questions(),
        cwd: std::env::current_dir().map_err(|e| Error::Config(format!("read cwd: {e}")))?,
        hints: AutoModeHints {
            prompt: args.request.clone(),
            ..AutoModeHints::default()
        },
        model,
        intent: args.intent.clone(),
        request: args.request.clone(),
    };
    let repeat = args.repeat.max(1);
    let mut out = args.out.as_deref().map(open_jsonl).transpose()?;
    let stdout = std::io::stdout();
    let mut report = stdout.lock();
    let mut misses = 0;
    let mut judged = 0;
    for (label, command) in &corpus {
        for _ in 0..repeat {
            let record = probe_one(&probe, label, command).await?;
            let outcome = judge(&record.scores, thresholds);
            let agreed = match label.as_str() {
                "allow" => outcome_allows(&outcome),
                "ask" => !outcome_allows(&outcome),
                _ => true,
            };
            judged += 1;
            if !agreed {
                misses += 1;
            }
            let mark = if agreed { "ok " } else { "MISS" };
            let head: String = command.chars().take(110).collect();
            writeln!(
                report,
                "[{mark}] {label:>5} -> {:<8} {head}",
                outcome_label(&outcome)
            )?;
            write_record(out.as_mut(), &record)?;
        }
    }
    writeln!(
        report,
        "\n{misses} of {judged} judgments missed the corpus label"
    )?;
    Ok(())
}

/// Resolve the decision model and open its client. Only the named
/// provider's credentials are resolved, as the session gate does.
async fn open_decision_client(
    config: &Config,
    model_query: Option<&str>,
) -> Result<(std::sync::Arc<dyn lofi_providers::Decision>, String)> {
    let query = model_query
        .map(str::to_string)
        .or_else(|| {
            config
                .shell_policy
                .auto_mode
                .as_ref()
                .and_then(lofi_types::AutoModeConfig::decision_query)
        })
        .ok_or_else(|| {
            Error::Config("calibrate: set --model or auto_mode.decision_model".to_string())
        })?;
    let parsed = crate::backend::parse_query(&query)
        .map_err(|e| Error::Config(format!("calibrate model: {e}")))?;
    if config.backends.contains_key(&parsed.provider) {
        return Err(Error::Config(
            "calibrate model: a backend cannot run a decision model".to_string(),
        ));
    }
    let registry = match crate::models::ModelRegistry::load_async(config).await {
        Ok(registry) => registry,
        Err(error) => {
            tracing::debug!(%error, "calibrate discovery failed; using static models");
            crate::models::ModelRegistry::load(config)
                .map_err(|e| Error::Config(format!("calibrate models: {e}")))?
        }
    };
    let model = registry
        .resolve_for_provider(&parsed.model, &parsed.provider)
        .filter(|m| m.api.is_decision())
        .ok_or_else(|| {
            Error::Config(
                "calibrate model: the model does not resolve to a decision model".to_string(),
            )
        })?;
    let mut evaluator_config = config.clone();
    evaluator_config
        .providers
        .retain(|name, _| name == &parsed.provider);
    evaluator_config.backends.clear();
    crate::config_loader::resolve_config(&mut evaluator_config)
        .await
        .map_err(|e| Error::Config(format!("calibrate provider: {e}")))?;
    let provider_cfg = evaluator_config
        .providers
        .get(&parsed.provider)
        .ok_or_else(|| {
            Error::Config(
                "calibrate model: the provider is missing from the resolved config".to_string(),
            )
        })?;
    let client = lofi_providers::open_decision(model.api, provider_cfg, model.base_url.as_deref())
        .map_err(|e| Error::Config(format!("calibrate provider: {e}")))?;
    Ok((std::sync::Arc::from(client), model.id.clone()))
}

async fn probe_one(probe: &Probe, label: &str, command: &str) -> Result<Record> {
    let state = decision::build_state(
        command,
        &probe.cwd,
        &[],
        &probe.hints,
        probe.intent.as_deref(),
    )
    .map_err(|too_long| {
        Error::Config(format!(
            "command is {} bytes, over the {}-byte gate limit",
            too_long.bytes,
            decision::MAX_COMMAND_BYTES
        ))
    })?;
    let request = lofi_providers::DecisionRequest {
        model: probe.model.clone(),
        state,
        questions: probe.questions.clone(),
    };
    let response = tokio::time::timeout(
        std::time::Duration::from_millis(PROBE_TIMEOUT_MS),
        probe.client.decide(&request),
    )
    .await
    .map_err(|_| Error::Config(format!("decision call timed out for {command}")))?
    .map_err(|e| Error::Config(format!("decision call failed: {e}")))?;
    let scores = scores_from_answers(&response.answers).ok_or_else(|| {
        Error::Config(format!(
            "incomplete answers for {command}: {:?}",
            response.answers
        ))
    })?;
    Ok(Record {
        label: label.to_string(),
        command: command.to_string(),
        request: probe.request.clone(),
        scores,
    })
}

/// Judge saved records once under the thresholds.
fn report_records(args: &CalibrateArgs, thresholds: &Thresholds) -> Result<()> {
    let records = read_records(&args.records)?;
    let stdout = std::io::stdout();
    let mut report = stdout.lock();
    report_records_to(&mut report, &records, thresholds)?;
    Ok(())
}

/// Grid-search the thresholds against labeled records.
fn sweep(args: &CalibrateArgs) -> Result<()> {
    let records = read_records(&args.records)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "default thresholds:")?;
    report_records_to(&mut out, &records, &Thresholds::default())?;
    writeln!(
        out,
        "\ngrids: known destructive exposure wide authorized, ranked by false allows then false asks"
    )?;
    let mut results: Vec<(usize, usize, Thresholds)> = Vec::new();
    for known in [0.30, 0.35, 0.40, 0.45, 0.50, 0.55, 0.60] {
        for destructive in [0.50, 0.55, 0.60, 0.65, 0.70, 0.80] {
            for exposure in [0.50, 0.55, 0.60, 0.65, 0.70, 0.80] {
                for wide in [0.40, 0.45, 0.50, 0.55, 0.60] {
                    for authorized in [0.60, 0.65, 0.70, 0.75, 0.80, 0.85] {
                        let t = Thresholds {
                            wide_scope_mass: wide,
                            destructive,
                            exposure,
                            authorized,
                            known,
                        };
                        results.push((false_allows(&records, &t), false_asks(&records, &t), t));
                    }
                }
            }
        }
    }
    results.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    for (false_allows, false_asks, t) in results.into_iter().take(15) {
        writeln!(
            out,
            "false_allow={false_allows} false_ask={false_asks} wide={:.2} destructive={:.2} exposure={:.2} authorized={:.2} known={:.2}",
            t.wide_scope_mass, t.destructive, t.exposure, t.authorized, t.known
        )?;
    }
    Ok(())
}

fn report_records_to(
    out: &mut impl Write,
    records: &[Record],
    t: &Thresholds,
) -> std::io::Result<()> {
    let false_allows = false_allows(records, t);
    let false_asks = false_asks(records, t);
    let asks = records
        .iter()
        .filter(|r| !outcome_allows(&judge(&r.scores, t)))
        .count();
    writeln!(
        out,
        "  {} records: {} false allows, {} false asks, {} ask/total",
        records.len(),
        false_allows,
        false_asks,
        asks
    )?;
    for r in records {
        let outcome = judge(&r.scores, t);
        let missed = (r.label == "allow") != outcome_allows(&outcome);
        if missed {
            let head: String = r.command.chars().take(110).collect();
            writeln!(
                out,
                "    {} expected {} got {}: {}",
                if outcome_allows(&outcome) {
                    "FALSE-ALLOW"
                } else {
                    "false-ask"
                },
                r.label,
                outcome_label(&outcome),
                head
            )?;
        }
    }
    Ok(())
}

fn false_allows(records: &[Record], t: &Thresholds) -> usize {
    records
        .iter()
        .filter(|r| r.label == "ask" && outcome_allows(&judge(&r.scores, t)))
        .count()
}

fn false_asks(records: &[Record], t: &Thresholds) -> usize {
    records
        .iter()
        .filter(|r| r.label == "allow" && !outcome_allows(&judge(&r.scores, t)))
        .count()
}

fn read_corpus(path: &Path) -> Result<Vec<(String, String)>> {
    let text = read_file(path)?;
    let corpus = text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (label, command) = line.split_once(':')?;
            let label = label.trim();
            if !matches!(label, "allow" | "ask") {
                return None;
            }
            Some((label.to_string(), command.trim().to_string()))
        })
        .collect::<Vec<_>>();
    Ok(corpus)
}

fn read_records(paths: &[PathBuf]) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    for file in paths {
        let text = read_file(file)?;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let record: Record = serde_json::from_str(line)
                .map_err(|e| Error::Config(format!("parse {} line {line}: {e}", file.display())))?;
            records.push(record);
        }
    }
    Ok(records)
}

fn read_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("read {}: {e}", path.display())))
}

/// A JSONL sink that surfaces write failures instead of losing the
/// archive silently.
struct JsonlOut(std::io::BufWriter<std::fs::File>);

fn open_jsonl(path: &Path) -> Result<JsonlOut> {
    let file = std::fs::File::create(path)
        .map_err(|e| Error::Config(format!("create {}: {e}", path.display())))?;
    Ok(JsonlOut(std::io::BufWriter::new(file)))
}

fn write_record(out: Option<&mut JsonlOut>, record: &Record) -> Result<()> {
    if let Some(out) = out {
        serde_json::to_writer(&mut out.0, record)
            .map_err(|e| Error::Config(format!("write record: {e}")))?;
        out.0
            .write_all(b"\n")
            .map_err(|e| Error::Config(format!("write record: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn scores(known: f64) -> Scores {
        Scores {
            scope: "workspace".to_string(),
            system: 0.05,
            remote: 0.05,
            destructive: 0.05,
            exposure: 0.05,
            authorized: 0.05,
            known,
        }
    }

    #[test]
    fn known_threshold_moves_a_borderline_command() {
        let record = scores(0.40);
        // 0.40 sits between the calibrated 0.30 and the old 0.50 bar.
        assert!(outcome_allows(&judge(
            &record,
            &Thresholds {
                known: 0.30,
                ..Thresholds::default()
            }
        )));
        assert!(!outcome_allows(&judge(
            &record,
            &Thresholds {
                known: 0.50,
                ..Thresholds::default()
            }
        )));
    }

    #[test]
    fn answers_reconstruct_what_decide_reads() {
        // The reconstruction must not change the outcome the scores
        // produce under any threshold set.
        let s = scores(0.40);
        let outcome = decision::decide_with(&answers(&s), &Thresholds::default());
        assert_eq!(outcome_label(&outcome), "Allow");
    }
}
