use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;

use crate::convert::JS_TO_JSON_MAX_BYTES;
use crate::tools::BashEnv;

pub const DEFAULT_GUEST_TIMEOUT: Duration = Duration::from_mins(2);

/// Optional `lofi.recall` implementation: a sync callback from the
/// `lofi.recall` native tool into the engine that owns the session
/// transcript. The callback reads the session file fresh and runs the
/// recall engine, so the sandbox never depends on `lofi-core`.
pub type RecallFn = Arc<
    dyn Fn(&lofi_types::recall::RecallRequest) -> lofi_types::recall::RecallOutcome + Send + Sync,
>;

pub type ResultFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Which `lofi.subagent*` function the guest invoked. One typed hook
/// serves the whole family so the crate boundary keeps a single plumbing
/// path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentOp {
    /// `lofi.subagent`: foreground run; dropping the future cancels it.
    Run,
    /// `lofi.subagentSpawn`: background run; the future returns at once.
    Spawn,
    /// `lofi.subagentStatus`.
    Status,
    /// `lofi.subagentList`.
    List,
    /// `lofi.subagentWait`: dropping the future aborts the wait, never the
    /// run.
    Wait,
    /// `lofi.subagentRead`.
    Read,
    /// `lofi.subagentKill`.
    Kill,
    /// `lofi.subagentNotify`.
    Notify,
    /// `lofi.subagentMailboxSend`.
    MailboxSend,
    /// `lofi.subagentMailboxReceive`: dropping the future aborts the wait,
    /// never the queued messages.
    MailboxReceive,
    /// `lofi.subagentMailboxList`.
    MailboxList,
}

/// `lofi.subagent*` native tool into the engine that owns agent runs. The
/// callback takes the operation and the raw argument object, and returns
/// the structured result (`{ ok, id, status, ... }`). The sandbox never
/// depends on `lofi-core`; the callback does.
pub type SubagentFn = Arc<
    dyn Fn(SubagentOp, Json) -> std::pin::Pin<Box<dyn std::future::Future<Output = Json> + Send>>
        + Send
        + Sync,
>;

/// Optional `lofi.models` implementation: lists the models available for
/// `lofi.subagent` overrides. `None` where the host has no registry (nested
/// subagent sandboxes, tests).
pub type ModelsFn = Arc<dyn Fn() -> Json + Send + Sync>;

// Must not block: called synchronously from the sandbox / job driver.
pub type JobReleaseFn = Arc<dyn Fn(u64) + Send + Sync>;
/// Acquire lifecycle ownership for a spawned job. The returned release hook
/// is retained by that job and called once when it reaches a terminal state.
pub type JobAcquireFn = Arc<dyn Fn(u64) -> Option<JobReleaseFn> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmReason {
    Policy,
    AutoEvaluating { started_at: std::time::Instant },
    AutoAsk { reason: String },
    AutoFailed { reason: String },
}

#[derive(Debug, Clone)]
pub struct ConfirmPrompt {
    pub command: String,
    pub reason: Arc<std::sync::Mutex<ConfirmReason>>,
    /// True while this prompt can still accept a user response. Auto-mode
    /// clears it when an approval or override wins the race, allowing the UI
    /// to dismiss a stale request whose response future was cancelled.
    pub active: Arc<std::sync::atomic::AtomicBool>,
}

pub type ConfirmFn = Arc<
    dyn Fn(
            ConfirmPrompt,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + Sync>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoModeOutcome {
    Allow { reason: String },
    Ask { reason: String },
    Failed { reason: String },
}

pub type AutoModeFn = Arc<
    dyn Fn(
            String,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = AutoModeOutcome> + Send + Sync>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone)]
pub enum ToolEvent {
    Start {
        id: u64,
        name: String,
        args: String,
    },
    End {
        id: u64,
        result: String,
        is_error: bool,
    },
}

#[derive(Clone)]
pub struct ExecCtx {
    pub root: PathBuf,
    pub tmp_dir: PathBuf,
    pub strings: HashMap<String, String>,
    pub recall: Option<RecallFn>,
    pub result: Option<ResultFn>,
    pub on_tool_event: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
    pub bash_env: BashEnv,
    pub shell_policy: crate::policy::ResolvedPolicy,
    pub confirm: Option<ConfirmFn>,
    pub auto_mode: Option<AutoModeFn>,
    /// Session-scoped `/policy` approval override shared with the owning
    /// agent.
    pub policy_override: crate::policy::PolicyOverride,
    pub skills_dir: Option<PathBuf>,
    /// Visible-output cap applied to file reads and bash output.
    pub truncate: crate::tools::truncate::TruncatedCap,
    /// Session-scoped background job registry shared with the owning
    /// agent.
    pub jobs: crate::tools::JobRegistry,
    /// Owning subagent run id when this exec belongs to one. Jobs spawned
    /// here are tagged with it so the subagent can be cancelled with its
    /// process tree.
    pub job_owner: Option<String>,
    /// Fired at successful `jobSpawn`. `None` keeps lifecycle in-memory
    /// only (pre-session resume, tests, `--no-session`). The callback
    /// returns the completion hook retained by that job.
    pub on_job_acquired: Option<crate::JobAcquireFn>,
    /// Runs a subagent turn. `None` where subagents are unsupported
    /// (nested subagent sandboxes, tests); `lofi.subagent` stays bound and
    /// reports the context as unavailable.
    pub subagent: Option<SubagentFn>,
    /// Lists available models. `None` where the host has no registry;
    /// `lofi.models` stays bound and reports the context as unavailable.
    pub models: Option<ModelsFn>,
}

impl std::fmt::Debug for ExecCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecCtx")
            .field("root", &self.root)
            .field("tmp_dir", &self.tmp_dir)
            .field("strings", &self.strings)
            .field("recall", &self.recall.is_some())
            .field("result", &self.result.is_some())
            .field("on_tool_event", &self.on_tool_event.is_some())
            .field("bash_env", &self.bash_env)
            .field("shell_policy", &"<resolved>")
            .field("confirm", &self.confirm.is_some())
            .field("auto_mode", &self.auto_mode.is_some())
            .field("subagent", &self.subagent.is_some())
            .field("models", &self.models.is_some())
            .field("skills_dir", &self.skills_dir)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub timeout: Duration,
    /// Maximum cumulative bytes copied from the guest result into host JSON.
    /// The agent sets this to its model-visible result cap so oversized values
    /// are truncated before a second full-sized representation is allocated.
    pub max_value_bytes: usize,
    /// Optional external cancellation flag. When set to `true`, the `QuickJS`
    /// interrupt handler breaks out of any running synchronous guest code and
    /// [`exec`](fn.exec.html) returns [`Error::Sandbox`] with a
    /// "cancelled" message. Without this, a tight `while (true) {}` loop
    /// blocks inside native `ctx.eval` and cannot be preempted by tokio's
    /// task-abort mechanism — the interrupt handler is the only way in.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_GUEST_TIMEOUT,
            max_value_bytes: JS_TO_JSON_MAX_BYTES,
            cancel: None,
        }
    }
}

#[derive(Debug)]
pub struct ExecImage {
    pub file: std::fs::File,
    pub bytes: usize,
}

#[derive(Debug)]
pub struct ExecResult {
    pub value: Json,
    pub(crate) has_value: bool,
    pub errors: Vec<Json>,
    pub logs: String,
    pub value_truncated: bool,
    pub image: Option<ExecImage>,
}

impl ExecResult {
    #[must_use]
    pub fn has_returned_value(&self) -> bool {
        self.has_value
    }
}
