use futures::StreamExt;
use lofi_code::policy::auto_mode::{self, AutoModeRisk};
use lofi_code::policy::decision;
use lofi_code::{AutoModeFn, AutoModeHintHandle, AutoModeHints, AutoModeOutcome};
use lofi_types::{AutoModeConfig, Config, ContentBlock, Message, PromptKind, Role};
use std::sync::Arc;

use lofi_error::{Error, Result};

/// One evaluation's default wait. A stalled evaluator holds the command and
/// the turn with it, but a timeout is a prompt, so the wait errs long: a
/// slow provider answer beats an interrupt.
const CHAT_TIMEOUT_MS: u64 = 30_000;
/// The same default for the decision stage: a slow first answer falls to
/// the chat stage, which is still quieter than a prompt.
const DECISION_TIMEOUT_MS: u64 = 30_000;

use crate::backend::{BackendFactory, BackendRef};

/// Attach the shell evaluator to a session executor.
///
/// Only the named provider credentials are resolved, and no backend process
/// starts until a command needs a decision. The evaluator is a backend of its
/// own, so an HTTP provider and a CLI child both work, and neither shares the
/// turn slot of the session whose command is judged.
///
/// # Errors
/// Returns [`Error::Config`] when the evaluator model or its credentials are
/// missing.
pub async fn configure(
    mut executor: super::ToolExecutor,
    config: &Config,
    env: &[(String, String)],
) -> Result<super::ToolExecutor> {
    let Some(auto_cfg) = config
        .shell_policy
        .auto_mode
        .as_ref()
        .filter(|mode| mode.enable)
        .cloned()
    else {
        executor.auto_mode = None;
        executor.auto_mode_config = None;
        return Ok(executor);
    };
    let _env_guard = crate::config_loader::apply_env_specs(env);
    // Each configured stage runs on its own provider, so both stay in the
    // resolved config; every other provider's credentials stay unresolved.
    let mut keep = Vec::new();
    for (field, query) in [
        ("decision_model", auto_cfg.decision_query()),
        ("model", auto_cfg.chat_query()),
    ] {
        let Some(query) = query else { continue };
        let parsed = crate::backend::parse_query(&query)
            .map_err(|error| Error::Config(format!("auto_mode.{field}: {error}")))?;
        keep.push(parsed.provider);
    }
    if keep.is_empty() {
        return Err(Error::Config(
            "auto_mode: set model or decision_model".to_string(),
        ));
    }
    let mut evaluator_config = config.clone();
    evaluator_config
        .providers
        .retain(|name, _| keep.contains(name));
    evaluator_config
        .backends
        .retain(|name, _| keep.contains(name));
    crate::config_loader::resolve_config(&mut evaluator_config).await?;
    let registry = match crate::models::ModelRegistry::load_async(&evaluator_config).await {
        Ok(registry) => registry,
        Err(error) => {
            tracing::debug!(%error, "auto-mode discovery failed; using static models");
            crate::models::ModelRegistry::load(&evaluator_config)?
        }
    };
    attach(&mut executor, &evaluator_config, &registry)?;
    Ok(executor)
}

/// Build the evaluator from an already resolved config and its catalog.
///
/// The session resolves the evaluator's provider once and a child re-attaches
/// from that same config, so neither builds a request from the raw
/// `$VAR` and `!cmd` forms. The catalog comes from the caller: a provider
/// that discovers its models lists models a static catalog does not have, and
/// the evaluator may name one of them.
pub(super) fn attach(
    executor: &mut super::ToolExecutor,
    config: &Config,
    registry: &crate::models::ModelRegistry,
) -> Result<()> {
    let Some(auto_cfg) = config
        .shell_policy
        .auto_mode
        .as_ref()
        .filter(|mode| mode.enable)
        .cloned()
    else {
        executor.auto_mode = None;
        executor.auto_mode_config = None;
        return Ok(());
    };
    // Two optional gates: a decision model gates first, and a chat model
    // judges what it does not allow. One of the two must be configured.
    let decision_query = auto_cfg.decision_query();
    let chat_query = auto_cfg.chat_query();
    if let Some(query) = decision_query {
        let parsed = crate::backend::parse_query(&query)
            .map_err(|error| Error::Config(format!("auto_mode.decision_model: {error}")))?;
        // A backend name cannot serve the decision gate.
        if config.backends.contains_key(&parsed.provider) {
            return Err(Error::Config(
                "auto_mode.decision_model: a backend cannot run a decision model".to_string(),
            ));
        }
        // Resolve on the bare model id: the query's level or tier suffix
        // would defeat pattern matching, and the decision path rejects
        // those suffixes itself.
        let model = registry
            .resolve_for_provider(&parsed.model, &parsed.provider)
            .filter(|m| m.api.is_decision())
            .ok_or_else(|| {
                Error::Config(
                    "auto_mode.decision_model: the model does not resolve to a decision model"
                        .to_string(),
                )
            })?;
        return attach_decision(executor, config, &auto_cfg, model, &parsed, registry);
    }
    let Some(query) = chat_query else {
        return Err(Error::Config(
            "auto_mode: set model or decision_model".to_string(),
        ));
    };
    let parsed = crate::backend::parse_query(&query)
        .map_err(|error| Error::Config(format!("auto_mode.model: {error}")))?;
    // A decision model named as the chat stage is a naming mistake: it
    // cannot run the chat rubric.
    if !config.backends.contains_key(&parsed.provider)
        && registry
            .resolve_for_provider(&parsed.model, &parsed.provider)
            .is_some_and(|m| m.api.is_decision())
    {
        return Err(Error::Config(
            "auto_mode.model: a decision model belongs in decision_model".to_string(),
        ));
    }
    let factory = BackendFactory::new(config.clone(), &executor.root, Vec::new())?
        .with_resolved_registry(registry.clone());
    let evaluator = factory
        .open_resolved(Some(&query), executor.clone())
        .map_err(|error| Error::Config(format!("auto_mode.model: {error}")))?;
    let hints = executor.auto_mode_hints.clone();
    executor.auto_mode = Some(build_auto_mode(
        evaluator,
        &auto_cfg,
        &executor.root,
        executor.skills_dir.as_deref(),
        &hints,
    ));
    executor.auto_mode_config = Some(std::sync::Arc::new(config.clone()));
    Ok(())
}

/// Build the gate from a decision model. The model, its provider, and its
/// credentials are resolved here, once: a misconfigured gate stops the
/// session from starting instead of failing on the first dangerous command.
fn attach_decision(
    executor: &mut super::ToolExecutor,
    config: &Config,
    auto_cfg: &AutoModeConfig,
    model: &lofi_types::Model,
    parsed: &crate::backend::ParsedQuery,
    registry: &crate::models::ModelRegistry,
) -> Result<()> {
    if parsed.level.is_some() || parsed.tier != lofi_types::ServiceTier::Auto {
        return Err(Error::Config(
            "auto_mode.decision_model: a decision model has no thinking level or service tier"
                .to_string(),
        ));
    }
    // A decision model answers typed questions, so max_tokens does not
    // apply. Ignored rather than rejected: a file-level value cannot be
    // unset through an env override, and the chat evaluator still uses it.
    let provider_cfg = config.providers.get(&parsed.provider).ok_or_else(|| {
        Error::Config(
            "auto_mode.model: the provider is missing from the resolved config".to_string(),
        )
    })?;
    let client = lofi_providers::open_decision(model.api, provider_cfg, model.base_url.as_deref())
        .map_err(|error| Error::Config(format!("auto_mode.model: {error}")))?;
    let client = Arc::from(client);
    let hints = executor.auto_mode_hints.clone();
    let gate = build_decision_auto_mode(
        client,
        &model.id,
        std::time::Duration::from_millis(auto_cfg.timeout_ms.unwrap_or(DECISION_TIMEOUT_MS)),
        &executor.root,
        executor.skills_dir.as_deref(),
        &hints,
    );
    // A chat stage answers what the decision model does not allow, so an
    // unknown or hedged command gets a full rubric judgment before the
    // prompt shows. Without one, the ask goes straight to the user.
    let gate = match auto_cfg.chat_query() {
        Some(chat_query) => {
            let factory = BackendFactory::new(config.clone(), &executor.root, Vec::new())?
                .with_resolved_registry(registry.clone());
            let evaluator = factory
                .open_resolved(Some(&chat_query), executor.clone())
                .map_err(|error| Error::Config(format!("auto_mode.model: {error}")))?;
            let chat = build_auto_mode(
                evaluator,
                auto_cfg,
                &executor.root,
                executor.skills_dir.as_deref(),
                &hints,
            );
            chat_stage_after_decision(gate, chat)
        }
        None => gate,
    };
    executor.auto_mode = Some(gate);
    executor.auto_mode_config = Some(std::sync::Arc::new(config.clone()));
    Ok(())
}

/// Everything one decision call needs. Held in one struct so the closure
/// clones a single value and the evaluation stays under the argument limit.
struct DecisionGate {
    client: Arc<dyn lofi_providers::Decision>,
    model: String,
    questions: std::collections::BTreeMap<String, lofi_types::DecisionQuestion>,
    timeout: std::time::Duration,
    cwd: std::path::PathBuf,
    skill_dirs: Vec<std::path::PathBuf>,
}

/// Build the gate from an opened decision client. Mirrors
/// `build_auto_mode`: the questions are static, the state is built per
/// command, and the call runs under the decision deadline.
#[must_use]
pub fn build_decision_auto_mode(
    client: Arc<dyn lofi_providers::Decision>,
    model: &str,
    timeout: std::time::Duration,
    cwd: &std::path::Path,
    skills_dir: Option<&std::path::Path>,
    hints: &AutoModeHintHandle,
) -> AutoModeFn {
    // Skill roots that actually exist are named in the state so a decision
    // model can treat installed skill scripts as pre-vetted.
    let mut skill_dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(dir) = skills_dir.filter(|d| d.is_dir()) {
        skill_dirs.push(dir.to_path_buf());
    }
    let ws_skills = cwd.join(".lofi").join("skills");
    if ws_skills.is_dir() {
        skill_dirs.push(ws_skills);
    }
    let gate = Arc::new(DecisionGate {
        client,
        model: model.to_string(),
        questions: decision::questions(),
        timeout,
        cwd: cwd.to_path_buf(),
        skill_dirs,
    });

    let hints = hints.clone();
    Arc::new(move |command: String| {
        let gate = gate.clone();
        let turn = hints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move {
            let handle =
                tokio::task::spawn(async move { evaluate_decision(&gate, &command, &turn).await });
            let mut abort_on_drop = AbortOnDrop(Some(handle.abort_handle()));
            let outcome = match handle.await {
                Ok(outcome) => outcome,
                Err(e) => AutoModeOutcome::Failed {
                    reason: format!("evaluation task failed: {e}"),
                },
            };
            abort_on_drop.disarm();
            outcome
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = AutoModeOutcome> + Send + Sync>>
    })
}

/// The message every outcome carries, for chaining two stages.
fn outcome_reason(outcome: &AutoModeOutcome) -> &str {
    match outcome {
        AutoModeOutcome::Allow { reason }
        | AutoModeOutcome::Ask { reason }
        | AutoModeOutcome::HighRisk { reason }
        | AutoModeOutcome::Failed { reason } => reason,
    }
}

/// Run the decision gate, then the chat evaluator on what it does not
/// allow. The decision stage stays the only path for an allow, so a cheap
/// fast gate keeps the common case; the chat stage sees exactly the
/// commands the decision model asked on or failed to judge, and its own
/// outcome stands: a low risk it finds runs, everything else shows the
/// prompt carrying both reasons.
#[must_use]
pub fn chat_stage_after_decision(decision: AutoModeFn, chat: AutoModeFn) -> AutoModeFn {
    Arc::new(move |command: String| {
        let decision = decision.clone();
        let chat = chat.clone();
        Box::pin(async move {
            let first = decision(command.clone()).await;
            if let AutoModeOutcome::Allow { reason } = first {
                return AutoModeOutcome::Allow { reason };
            }
            let second = chat(command).await;
            match second {
                AutoModeOutcome::Allow { reason } => AutoModeOutcome::Allow { reason },
                AutoModeOutcome::Ask { reason } => AutoModeOutcome::Ask {
                    reason: format!("{reason}; decision gate: {}", outcome_reason(&first)),
                },
                AutoModeOutcome::HighRisk { reason } => AutoModeOutcome::HighRisk {
                    reason: format!("{reason}; decision gate: {}", outcome_reason(&first)),
                },
                // A chat failure keeps the decision stage's verdict: the
                // command was already refused once, and a failed second
                // judgment must not read as a pass.
                AutoModeOutcome::Failed { reason } => AutoModeOutcome::Failed {
                    reason: format!("{reason}; decision gate: {}", outcome_reason(&first)),
                },
            }
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = AutoModeOutcome> + Send + Sync>>
    })
}

async fn evaluate_decision(
    gate: &DecisionGate,
    command: &str,
    turn: &AutoModeHints,
) -> AutoModeOutcome {
    // A command is never sent cut: a truncated command can hide its
    // dangerous tail, so an oversized one asks without a network call.
    let state = match decision::build_state(command, &gate.cwd, &gate.skill_dirs, turn) {
        Ok(state) => state,
        Err(too_long) => {
            return AutoModeOutcome::Ask {
                reason: format!(
                    "command is {} bytes, over the {}-byte gate limit",
                    too_long.bytes,
                    decision::MAX_COMMAND_BYTES
                ),
            };
        }
    };
    let request = lofi_providers::DecisionRequest {
        model: gate.model.clone(),
        state,
        questions: gate.questions.clone(),
    };
    let started = std::time::Instant::now();
    // The deadline covers the call and its rate-limit retry, so a long
    // Retry-After surfaces as a timeout instead of a hang. A timeout or a
    // transport failure falls back to the manual prompt.
    match tokio::time::timeout(gate.timeout, gate.client.decide(&request)).await {
        Ok(Ok(response)) => {
            let outcome = decision::decide(&response.answers);
            // The outcome carries the threshold summary; the answers matter
            // most when the outcome fails on them.
            if matches!(outcome, AutoModeOutcome::Failed { .. }) {
                tracing::debug!(answers = ?response.answers, "auto-mode: decision answers");
            }
            tracing::debug!(
                command = command,
                model = response.model.as_str(),
                outcome = ?outcome,
                elapsed_ms = started.elapsed().as_millis(),
                input_tokens = response.usage.input_tokens,
                output_tokens = response.usage.output_tokens,
                "auto-mode: decision answered"
            );
            outcome
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "auto-mode: decision call failed");
            AutoModeOutcome::Failed {
                reason: format!("decision call failed: {e}"),
            }
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = gate.timeout.as_millis(),
                "auto-mode: decision call timed out"
            );
            AutoModeOutcome::Failed {
                reason: format!(
                    "decision call timed out after {} ms",
                    gate.timeout.as_millis()
                ),
            }
        }
    }
}

/// A fresh, empty context handle. Each agent gets its own so a subagent
/// cannot read the hints of the session that spawned it.
#[must_use]
pub fn hint_handle() -> AutoModeHintHandle {
    Arc::new(std::sync::Mutex::new(AutoModeHints::default()))
}

/// Abort a spawned provider evaluation if the surrounding auto-mode future is
/// dropped because the user overrides it. Dropping a bare Tokio `JoinHandle`
/// would detach the request and let it keep consuming provider resources.
struct AbortOnDrop(Option<tokio::task::AbortHandle>);

impl AbortOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

/// Build the evaluator from an opened backend. The evaluator answers a
/// one-shot text call, so it works on an HTTP provider and on a CLI backend
/// alike, and it never shares the turn slot of the session it judges.
#[must_use]
pub fn build_auto_mode(
    evaluator: BackendRef,
    auto_cfg: &AutoModeConfig,
    cwd: &std::path::Path,
    skills_dir: Option<&std::path::Path>,
    hints: &AutoModeHintHandle,
) -> AutoModeFn {
    let max_tokens = auto_cfg.max_tokens;
    let timeout = std::time::Duration::from_millis(auto_cfg.timeout_ms.unwrap_or(CHAT_TIMEOUT_MS));
    let cwd = cwd.to_path_buf();

    // Skill roots that actually exist are marked pre-vetted in the evaluator
    // prompt so installed skill scripts stop triggering confirmation.
    let mut skill_dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(dir) = skills_dir.filter(|d| d.is_dir()) {
        skill_dirs.push(dir.to_path_buf());
    }
    let ws_skills = cwd.join(".lofi").join("skills");
    if ws_skills.is_dir() {
        skill_dirs.push(ws_skills);
    }

    let hints = hints.clone();
    Arc::new(move |command: String| {
        let evaluator = evaluator.clone();
        let cwd = cwd.clone();
        let skill_dirs = skill_dirs.clone();
        let turn = hints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move {
            let handle = tokio::task::spawn(async move {
                evaluate_command(
                    &evaluator,
                    &command,
                    &cwd,
                    max_tokens,
                    timeout,
                    &skill_dirs,
                    &turn,
                )
                .await
            });
            let mut abort_on_drop = AbortOnDrop(Some(handle.abort_handle()));
            let outcome = match handle.await {
                Ok(outcome) => outcome,
                Err(e) => AutoModeOutcome::Failed {
                    reason: format!("evaluation task failed: {e}"),
                },
            };
            abort_on_drop.disarm();
            outcome
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = AutoModeOutcome> + Send + Sync>>
    })
}

pub(super) async fn evaluate_command(
    evaluator: &BackendRef,
    command: &str,
    cwd: &std::path::Path,
    max_tokens: Option<u64>,
    timeout: std::time::Duration,
    skill_dirs: &[std::path::PathBuf],
    turn: &AutoModeHints,
) -> AutoModeOutcome {
    let skill_dirs: Vec<&std::path::Path> =
        skill_dirs.iter().map(std::path::PathBuf::as_path).collect();
    // The system message is the same on every call so the provider's prompt
    // cache covers it; only the user message changes per command.
    let user =
        auto_mode::build_user_message(command, &cwd.display().to_string(), &skill_dirs, turn);

    let started = std::time::Instant::now();
    let request = crate::backend::CompletionRequest {
        system: auto_mode::system_prompt().to_string(),
        prompt: user,
        max_tokens,
    };
    // A stalled evaluator holds the command and the turn with it, so one
    // judgement gets a shorter cap than the provider's start timeout, which is
    // sized for a whole turn. A timeout is a failure, so the command falls
    // back to the manual prompt.
    match tokio::time::timeout(timeout, evaluator.complete(request)).await {
        Ok(Ok(completion)) => {
            let text = completion.text;
            let usage = completion.usage;
            let decision = auto_mode::parse_decision(&text);
            if let Some(d) = decision {
                tracing::debug!(
                    command = command,
                    risk = ?d.risk,
                    reason = d.reason.as_str(),
                    elapsed_ms = started.elapsed().as_millis(),
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    cache_read_tokens = usage.cache_read_tokens,
                    "auto-mode: evaluated"
                );
                match d.risk {
                    AutoModeRisk::Low => AutoModeOutcome::Allow { reason: d.reason },
                    AutoModeRisk::Confirm => AutoModeOutcome::Ask { reason: d.reason },
                    AutoModeRisk::Block => AutoModeOutcome::HighRisk { reason: d.reason },
                }
            } else {
                tracing::warn!(
                    response = text.as_str(),
                    "auto-mode: could not parse LLM response"
                );
                AutoModeOutcome::Failed {
                    reason: "evaluator returned an invalid response".to_string(),
                }
            }
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "auto-mode: evaluation failed");
            AutoModeOutcome::Failed {
                reason: format!("evaluation failed: {e}"),
            }
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = timeout.as_millis(),
                "auto-mode: evaluation timed out"
            );
            AutoModeOutcome::Failed {
                reason: format!("evaluation timed out after {} ms", timeout.as_millis()),
            }
        }
    }
}

/// Derive the evaluator context for the turn that is starting. Derived from
/// the live history instead of accumulated, so compaction, rewind, and a new
/// session drop the context without extra bookkeeping.
///
/// The current request is always passed. The earlier request and the agent's
/// last message are passed only for a terse request, because "yes" answers
/// the agent's question and cannot be judged on its own.
#[must_use]
pub fn derive_hints(messages: &[Message], current: Option<(&str, PromptKind)>) -> AutoModeHints {
    // A subagent's task is written by the parent agent, so it is context,
    // never authorization. It is found in the current prompt of the child's
    // first turn, or in the history a resumed child replays.
    let mut hints = AutoModeHints {
        parent_task: current
            .filter(|(_, kind)| *kind == PromptKind::Task)
            .map(|(text, _)| auto_mode::bound(text))
            .or_else(|| messages.iter().find_map(task_text).map(auto_mode::bound)),
        ..AutoModeHints::default()
    };
    let Some((current, PromptKind::User)) = current else {
        return hints;
    };
    hints.prompt = Some(auto_mode::bound_request(current));
    if !auto_mode::is_terse(current) {
        return hints;
    }
    hints.prior_prompt = messages
        .iter()
        .rev()
        .find_map(typed_prompt)
        .map(auto_mode::bound);
    hints.prior_reply = messages
        .iter()
        .rev()
        .find_map(assistant_text)
        .map(|text| auto_mode::bound(&text));
    hints
}

/// The task a parent agent gave a subagent. It renders as context, not as a
/// request.
fn task_text(message: &Message) -> Option<&str> {
    if message.role != Role::User || message.kind != PromptKind::Task {
        return None;
    }
    message.blocks.iter().find_map(|block| match block {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })
}

/// The text of a request that a human typed. Compaction summaries and
/// app-injected notices are model- or app-written, so they are not requests.
pub(super) fn typed_prompt(message: &Message) -> Option<&str> {
    if message.role != Role::User || message.kind != lofi_types::PromptKind::User {
        return None;
    }
    let text = message.blocks.iter().find_map(|block| match block {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })?;
    (!text.starts_with(crate::compact::HANDOFF_PREAMBLE)).then_some(text)
}

/// The last text the agent produced, which carries the question a terse
/// request answers.
fn assistant_text(message: &Message) -> Option<String> {
    if message.role != Role::Assistant {
        return None;
    }
    message.blocks.iter().rev().find_map(|block| match block {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    })
}

/// Collect the answer and the usage the evaluation spent, so the cost and
/// the prompt-cache hit rate stay observable.
pub(crate) async fn collect_text(
    mut stream: futures::stream::BoxStream<'static, Result<lofi_types::StreamingEvent>>,
) -> Result<(String, lofi_types::Usage)> {
    let mut text = String::new();
    let mut usage = lofi_types::Usage::default();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(lofi_types::StreamingEvent::TextDelta(d)) => text.push_str(&d),
            Ok(lofi_types::StreamingEvent::Done { usage: spent, .. }) => {
                usage = spent;
                break;
            }
            Ok(lofi_types::StreamingEvent::Error(msg)) => {
                return Err(Error::Provider(msg));
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok((text, usage))
}
