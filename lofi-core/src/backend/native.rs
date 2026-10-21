use std::sync::Arc;

use async_trait::async_trait;
use lofi_error::{Error, Result};
use lofi_types::{ContentBlock, Message, ModelChoice, PromptKind, Role, ThinkingLevel};
use tokio::sync::mpsc;

use super::contract::{Backend, BackendRef, Completion, CompletionRequest, ModelSelection};
use crate::agent::native::NativeEngine;
use crate::agent::turn_session::{ExchangeDriver, TurnPolicy};
use crate::agent::{collect_text, usage_cost, Agent, ToolExecutor};
use crate::models::ModelRegistry;
use crate::session::store::SessionCursor;

/// What a native backend needs to open another transport: the registry that
/// resolves a query and the config that credentials were resolved into.
pub struct NativeSource {
    pub registry: ModelRegistry,
    pub config: lofi_types::Config,
}

impl NativeSource {
    #[must_use]
    pub fn new(registry: ModelRegistry, config: lofi_types::Config) -> Arc<Self> {
        Arc::new(Self { registry, config })
    }
}

#[async_trait]
impl Backend for Agent {
    #[allow(clippy::wrong_self_convention, clippy::misnamed_getters)]
    fn name(&self) -> &str {
        &self.model.provider
    }

    fn selection(&self) -> ModelSelection {
        ModelSelection {
            model: self.model.id.clone(),
            thinking: self.model.thinking.clone(),
            service_tier: self.model.service_tier.clone(),
        }
    }

    fn choice(&self) -> ModelChoice {
        if let Some(source) = &self.source {
            if let Some(choice) = source
                .registry
                .choices()
                .into_iter()
                .find(|c| c.provider == self.model.provider && c.id == self.model.id)
            {
                return choice;
            }
        }
        ModelChoice {
            provider: self.model.provider.clone(),
            id: self.model.id.clone(),
            name: self.model.name.clone(),
            thinking_levels: Vec::new(),
            service_tiers: Vec::new(),
            supports_image: self.model.supports_image,
            context_window: self.model.context_window,
        }
    }

    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn tool_executor(&self) -> ToolExecutor {
        Agent::tool_executor(self)
    }

    fn with_selection(&self, query: &str) -> Result<BackendRef> {
        let source = self.source.as_ref().ok_or_else(|| {
            Error::Config("this backend was opened without its model registry".into())
        })?;
        let (mut model, level) =
            crate::agent::select_model(&source.registry, &source.config, Some(query))?;
        model.thinking = level;
        let provider_cfg = source
            .config
            .providers
            .get(&model.provider)
            .ok_or_else(|| Error::Config(format!("provider not found: {}", model.provider)))?;
        let provider = lofi_providers::open(model.api, provider_cfg)?;
        let auto_continue = source
            .registry
            .auto_continue_policy(&source.config.agent, &model);
        Ok(Arc::new(
            self.with_model(provider, model)
                .with_auto_continue(auto_continue),
        ))
    }

    fn with_tool_executor(&self, executor: ToolExecutor) -> BackendRef {
        Arc::new(self.with_executor(&executor))
    }

    fn with_system_prompt(&self, prompt: String) -> BackendRef {
        Arc::new(Agent::with_system_prompt(self, prompt))
    }

    fn subagents(&self) -> crate::SubagentRegistry {
        Agent::subagents(self)
    }

    fn subagent_hook(&self) -> Option<lofi_code::SubagentFn> {
        Agent::subagent_hook(self)
    }

    fn fork(&self, executor: ToolExecutor) -> BackendRef {
        Arc::new(self.forked(&executor))
    }

    async fn run_turn(
        &self,
        messages: &mut Vec<Message>,
        request: crate::agent::TurnRequest,
        events: mpsc::Sender<crate::agent::AgentEvent>,
        cursor: Option<&SessionCursor>,
        controls: crate::agent::RunControls,
    ) -> Result<crate::session::recorder::TurnOutcome> {
        crate::agent::turn_session::run(self, messages, request, events, cursor, controls).await
    }

    fn open_turn(&self) -> Result<Box<dyn ExchangeDriver>> {
        Ok(Box::new(NativeEngine::new(self.clone())))
    }

    fn turn_policy(&self) -> TurnPolicy {
        Agent::turn_policy(self)
    }

    fn context_window(&self) -> Option<u64> {
        self.model.context_window
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let mut model = self.model.clone();
        model.thinking = ThinkingLevel::Off;
        if let Some(max) = request.max_tokens {
            model.max_tokens = Some(max);
        }
        let messages = vec![
            Message {
                origin: None,
                role: Role::System,
                blocks: vec![ContentBlock::Text {
                    text: request.system,
                }],
                kind: PromptKind::User,
            },
            Message {
                origin: None,
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: request.prompt,
                }],
                kind: PromptKind::User,
            },
        ];
        let stream = self.provider.stream(&model, &messages, &[]).await?;
        let (text, usage) = collect_text(stream).await?;
        Ok(Completion {
            text,
            usage,
            cost: usage_cost(&model, usage),
        })
    }
}
