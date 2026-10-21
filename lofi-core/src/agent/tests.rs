#![allow(clippy::unwrap_used)]
#![allow(clippy::wildcard_imports)]

use super::*;
use async_trait::async_trait;
use futures::{stream, StreamExt};
use lofi_types::{Api, PromptKind, SessionEventKind, Usage};
use tempfile::tempdir;

struct MockProvider {
    rounds: std::sync::Mutex<Vec<Vec<StreamingEvent>>>,
    requests: Option<std::sync::Mutex<Vec<Vec<Message>>>>,
}

impl MockProvider {
    fn new(rounds: Vec<Vec<StreamingEvent>>) -> Self {
        Self {
            rounds: std::sync::Mutex::new(rounds),
            requests: None,
        }
    }

    fn recording(rounds: Vec<Vec<StreamingEvent>>) -> Self {
        Self {
            requests: Some(std::sync::Mutex::new(Vec::new())),
            ..Self::new(rounds)
        }
    }

    fn requests(&self) -> std::sync::MutexGuard<'_, Vec<Vec<Message>>> {
        self.requests.as_ref().unwrap().lock().unwrap()
    }
}

#[derive(Default)]
struct RecordingProvider {
    requests: std::sync::Mutex<Vec<Vec<Message>>>,
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn stream(
        &self,
        _model: &Model,
        messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Ok(Box::pin(stream::iter([Ok(StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        })])))
    }
}

struct PendingAfterRoundProvider {
    first: std::sync::Mutex<Option<Vec<StreamingEvent>>>,
}

/// A provider whose stream never yields: the run stays live until it is
/// cancelled or timed out.
struct NeverProvider;

#[async_trait]
impl Provider for NeverProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        Ok(Box::pin(stream::pending()))
    }
}

#[async_trait]
impl Provider for PendingAfterRoundProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        let first = self.first.lock().unwrap().take();
        match first {
            Some(events) => Ok(Box::pin(stream::iter(events.into_iter().map(Ok)))),
            None => std::future::pending().await,
        }
    }
}

struct TerminalThenPendingProvider;

struct CompletedRoundThenPartialProvider {
    first: std::sync::Mutex<Option<Vec<StreamingEvent>>>,
}

#[async_trait]
impl Provider for CompletedRoundThenPartialProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        let first = self.first.lock().unwrap().take();
        match first {
            Some(events) => Ok(Box::pin(stream::iter(events.into_iter().map(Ok)))),
            None => Ok(Box::pin(
                stream::iter([Ok(StreamingEvent::TextDelta("partial answer".into()))])
                    .chain(stream::pending()),
            )),
        }
    }
}

#[async_trait]
impl Provider for TerminalThenPendingProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        Ok(Box::pin(
            stream::iter([
                Ok(StreamingEvent::TextDelta("done".to_string())),
                Ok(StreamingEvent::Done {
                    usage: Usage::default(),
                    stop_reason: None,
                }),
            ])
            .chain(stream::pending()),
        ))
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(
        &self,
        _model: &Model,
        messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        if let Some(requests) = &self.requests {
            requests.lock().unwrap().push(messages.to_vec());
        }
        let mut rounds = self.rounds.lock().unwrap();
        let evs = if rounds.is_empty() {
            Vec::new()
        } else {
            rounds.remove(0)
        };
        Ok(Box::pin(stream::iter(evs.into_iter().map(Ok))))
    }
}

fn model() -> Model {
    Model {
        id: "m".to_string(),
        name: "m".to_string(),
        provider: "p".to_string(),
        api: Api::OpenAiCompletions,
        reasoning: false,
        thinking: lofi_types::ThinkingLevel::Off,
        service_tier: lofi_types::ServiceTier::Auto,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    }
}

fn agent_with(rounds: Vec<Vec<StreamingEvent>>, root: &std::path::Path) -> Agent {
    Agent {
        exec_worker: std::sync::Arc::new(std::sync::OnceLock::new()),
        provider: Arc::new(MockProvider::new(rounds)),
        model: model(),
        root: root.to_path_buf(),
        tmp_dir: std::env::temp_dir().join("lofi-agent-test"),
        tmp_lease: None,
        retry: crate::retry::RetryPolicy::default(),
        system_prompt: "sys".to_string(),
        max_output_tokens: None,
        reserved_context_tokens: 0,
        bash_env: lofi_code::BashEnv::default(),
        shell_policy: lofi_code::policy::defaults::resolve(
            &lofi_types::ShellPolicyConfig::default(),
        ),
        confirm_tx: None,
        confirm_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        auto_mode: None,
        auto_mode_hints: super::auto_mode::hint_handle(),
        policy_override: lofi_code::policy::PolicyOverride::default(),
        skills_dir: None,
        jobs: lofi_code::tools::JobRegistry::new(),
        job_owner: None,
        mailbox: None,
        truncate: lofi_code::TruncatedCap::default(),
        image: lofi_types::ImageConfig::default(),
        auto_continue: lofi_types::AutoContinuePolicy::default(),
        subagent_support: None,
        subagents: SubagentRegistry::default(),
    }
}

fn user_msg(text: &str) -> Message {
    Message {
        origin: None,
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
    }
}

fn exec_round(code: &str) -> Vec<StreamingEvent> {
    vec![
        StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: serde_json::json!({ "code": code }).to_string(),
        },
        StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ]
}

fn system_msg(text: &str) -> Message {
    Message {
        origin: None,
        role: Role::System,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
    }
}

fn productive_tool_round(id: usize) -> Vec<StreamingEvent> {
    vec![
        StreamingEvent::ToolUseStart {
            id: format!("t{id}"),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: format!("t{id}"),
            delta: serde_json::json!({ "code": format!("return {id}") }).to_string(),
        },
        StreamingEvent::ToolUseEnd {
            id: format!("t{id}"),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::ToolUse),
        },
    ]
}

fn final_round() -> Vec<StreamingEvent> {
    vec![
        StreamingEvent::TextDelta("final answer".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ]
}

use indexmap::IndexMap;
use lofi_types::{
    ApiTypeMapping, Config, ModelConfig, PricingConvention, PricingFieldMappings, ProviderConfig,
    ThinkingLevel,
};

mod auto_mode;
mod continuation;
mod exec;
mod models;
mod persistence;
mod recovery;
mod retry;
mod round;
mod subagent;
