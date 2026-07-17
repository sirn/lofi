# lofi

`lofi` is a minimal but real coding-agent harness written in Rust, built around
*code mode*: the model has exactly one tool — `exec` — which compiles
and runs a TypeScript program in an embedded QuickJS sandbox. All file, shell,
and search operations happen *inside that program* through in-sandbox bindings
on a global `lofi` object. The agent writes code to use tools, rather than
calling discrete tools one at a time.

v1 ships:

- **Code mode** with a TypeScript → JavaScript (swc strip) → QuickJS runtime.
- Built-in sandbox bindings: `lofi.read`, `lofi.write`, `lofi.edit`, `lofi.bash`,
  `lofi.ls`, `lofi.find`, `lofi.grep`.
- **Streaming provider transports** for OpenAI Chat Completions, OpenAI
  Responses, and Anthropic Messages (API-key auth only).
- **Remote model discovery** — a provider can fetch its model list from a
  remote endpoint and cache it for offline restarts.
- A flicker-free **TUI** built on ratatui + crossterm (plaintext rendering for
  v1), plus a non-interactive `--print` path for piping.

## Build & run

```sh
cargo build                  # debug build
cargo run --                 # interactive TUI (default)
cargo run -- --print "list the files in this directory"
cargo run -- --list-models
```

Set `RUST_LOG` (e.g. `RUST_LOG=debug`) to enable `tracing` logs on stderr;
the TUI owns the alternate screen, so logs are only emitted when you opt in.

### CLI flags

| Flag | Description |
| --- | --- |
| `-p`, `--print <PROMPT>` | Run one non-interactive turn and stream assistant text to stdout. |
| `--list-models` | Print `provider/id — name` lines (with a `·img` marker for image-capable models) and exit. |
| `--list-sessions` | Print saved sessions for this workspace and exit. |
| `--model <SPEC>` | Select the model as `provider/model[:level]`. |
| `-c`, `--continue` | Resume the most recent session for this workspace. |
| `--resume <ID>` | Resume a specific session by id prefix. |
| `--no-session` | Do not persist a transcript (ephemeral). |

With no flags, `lofi` launches the interactive TUI against the current working
directory as the workspace root.

## Configuration

`lofi` reads its user config from `$XDG_CONFIG_HOME/lofi/config.toml` (default
`~/.config/lofi/config.toml`). This tree is treated as **read-only** — `lofi`
never writes there — so a config manager (Nix, stow, etc.) can own it
declaratively. Agent-owned, writable state (the remote-discovery cache and
session transcripts) lives under `$XDG_STATE_HOME/lofi/` (default
`~/.local/state/lofi/`). The split keeps a managed config tree pristine while
`lofi` still has a writable home for its cache and history.

### Value resolution

`api_key` and header values support value resolution:

- `!cmd` — run `sh -c cmd` and take the trimmed stdout;
- `$VAR` / `${VAR}` — read the env var (missing is an error);
- `$$` → a literal `$`, `$!` → a literal `!` (escapes);
- anything else is taken literally.

### Endpoint URLs

A provider's `base_url` is the **host root** (e.g. `https://api.openai.com`).
The full endpoint URL each model POSTs to is built by joining `base_url` with
the `path` of the selected `api_types` entry (see below). The provider never
appends a hardcoded suffix — the path is explicit in the config, with sensible
defaults (`/v1/chat/completions`, `/v1/responses`, `/v1/messages`) when
omitted.

### `api_type` and `api_types` — protocol and endpoint routing

Each provider has a scalar `api_type` naming its default protocol (an internal
api id, e.g. `openai-completions`) and an optional `api_types` table keyed by
the same internal ids, carrying the endpoint `path` and optional pricing-field
overrides. Static models, per-model `api_type` overrides, and auto-discovered
models all resolve through the same table: look up `api_types[key]`, take its
`path` (defaulting to the protocol's standard path), join onto `base_url`.

The common case is a single-protocol provider, which needs only the scalar:

```toml
[providers.openai]
base_url = "https://api.openai.com"
api_type = "openai-completions"
```

The full table is for proxies that route one host to several upstream APIs.
Keys are the internal api ids (kebab-case):

```toml
[providers.proxy]
base_url = "https://proxy.example.com"
api_type = "openai-completions"

[providers.proxy.api_types."openai-completions"]
path = "/v1/chat/completions"      # optional; defaults to /v1/chat/completions

[providers.proxy.api_types."openai-responses"]
path = "/v1/responses"             # optional; defaults to /v1/responses

[providers.proxy.api_types."anthropic-messages"]
path = "/v1/messages"              # optional; defaults to /v1/messages
```

A static model overrides its protocol with `api_type = "openai-responses"`;
the override is resolved through `api_types` exactly like a discovered
model's `preferred_api`.

### Example: OpenAI Chat Completions

```toml
default_provider = "openai"

[providers.openai]
base_url = "https://api.openai.com"
api_type = "openai-completions"
api_key = "$OPENAI_API_KEY"

[providers.openai.models]
gpt-4o = { name = "GPT-4o", supports_image = true, context_window = 128000 }
"gpt-4o-mini" = {}
```

### Example: OpenAI Responses

```toml
[providers.openai_resp]
base_url = "https://api.openai.com"
api_type = "openai-responses"
api_key = "$OPENAI_API_KEY"

[providers.openai_resp.models]
o1 = {}
```

### Example: Anthropic Messages

```toml
[providers.anthropic]
base_url = "https://api.anthropic.com"
api_type = "anthropic-messages"
api_key = "$ANTHROPIC_API_KEY"

[providers.anthropic.headers]
anthropic-beta = "prompt-caching-2024-07-31"

[providers.anthropic.models]
"claude-opus-4" = { name = "Claude Opus 4", reasoning = true, max_tokens = 4096 }
```

### Example: remote model discovery

A provider with an `auto_models` block fetches its model list from a
models endpoint at startup, maps each entry into a `ModelConfig`, and merges
the result with the provider's static `models` (**static wins** on id
collision, filling only fields the static entry left unset). Discovered models
are cached to `$XDG_STATE_HOME/lofi/discovery.json` and reused when the remote
fetch fails.

The endpoint URL is `auto_models.models_url` when set; otherwise it is derived
from the default `api_types` entry's `path` (the version prefix plus
`/models`, e.g. `/v1/models`). Each discovered model's `api_type` is read
from the field named by `api_type_field` (e.g. `preferred_api`), translated
through `auto_models.api_type_mappings` (the endpoint's own vocabulary → lofi
internal id), and resolved through the provider's `api_types` table like any
static model. When `api_type_field` is unset, discovered models inherit the
provider's default `api_type`. Pricing is read via the resolved api-type's
`pricing_field_mappings` (falling back to the provider-level default) and
scaled by the provider's `pricing_convention`.

```toml
[providers.openai]
base_url = "https://api.openai.com"
api_type = "openai-completions"
api_key = "$OPENAI_API_KEY"

[providers.openai.auto_models]
enabled = true
# models_url defaults to {base_url}/v1/models
api_type_field = "preferred_api"   # optional; field naming the remote api-type

# Remote vocabulary → internal api id. Only needed when the endpoint
# reports its own strings instead of lofi's internal ids.
[providers.openai.auto_models.api_type_mappings]
chat_completions = "openai-completions"
# messages = "anthropic-messages"
# responses = "openai-responses"

[providers.openai.pricing_field_mappings]   # optional; defaults shown
input = "pricing.prompt"
output = "pricing.completion"
cache_read = "pricing.input_cache_read"
cache_write = "pricing.input_cache_write"
# per_request = "pricing.request"   # optional; flat per-request cost path
```

### Config schema reference

```toml
default_provider = "openai"      # optional
default_model = "openai/gpt-4o"  # optional; "provider/id" or bare id

[agent]                          # optional; global defaults
thinking_level = "medium"        # optional
thinking_levels = ["low", "medium", "high", "xhigh"]  # optional

[providers.<name>]
base_url = "..."                 # host root; optional (defaults per api_type)
api_type = "openai-completions"  # default protocol (internal api id)
api_key = "..."                  # optional; resolved per the rules above
env_name = "..."                 # optional; env var for the api key
headers = { "x-custom" = "..." } # optional; values resolved
no_auth = false                  # optional; omit auth headers
thinking_level = "medium"        # optional; per-provider default
thinking_levels = [...]          # optional; per-provider
pricing_convention = "per_token" # optional; per_token | per_million

[providers.<name>.api_types."<api-id>"]  # optional; per-endpoint override
path = "/v1/chat/completions"    # optional; defaults per api-id

[providers.<name>.api_types."<api-id>".pricing_field_mappings]  # optional
input = "..."; output = "..."; cache_read = "..."; cache_write = "..."; per_request = "..."

[providers.<name>.pricing_field_mappings]  # optional; defaults shown above
input = "..."; output = "..."; cache_read = "..."; cache_write = "..."; per_request = "..."

[providers.<name>.models]        # optional; static models, keyed by id
"<id>" = { name = "...", api_type = "openai-responses", reasoning = bool,
            supports_image = bool, context_window = u64, max_tokens = u64,
            thinking_level = "medium", thinking_levels = [...],
            base_url = "...", input_price = f64, output_price = f64,
            cache_read_price = f64, cache_write_price = f64,
            per_request_price = f64 }

[providers.<name>.auto_models]   # optional; remote discovery
enabled = true
models_url = "..."               # optional; defaults to {base_url}/v1/models
path = "data"                   # optional; default "data"
api_type_field = "preferred_api" # optional; field naming the remote api-type
auth = true                     # optional; default true
thinking_levels = [...]          # optional; inherited by discovered models
thinking_level = "medium"        # optional
ttl_seconds = 300               # optional; default 300

[providers.<name>.auto_models.api_type_mappings]  # optional
"<remote-value>" = "<internal-api-id>"

[providers.<name>.auto_models.field_mappings]  # optional; defaults shown
name = "name"
context_window = "context_length"
max_tokens = "top_provider.max_completion_tokens"

[compaction]                    # optional; compaction settings
reserved_context_tokens = 20000  # hard cap: a run crossing `window - reserved`
                               #   mid-turn is force-stopped, compacted, and
                               #   silently continued.
min_messages_between_hard_compacts = 6  # min agent messages between two
                               #   force-compacts; a re-cross within this
                               #   window errors out instead of looping.

[compaction.auto]              # optional; speculative soft caps
enable = true                  # optional; default true
max_context_tokens = 150000    # optional; absolute cap, lowers the threshold
context_ratio = 0.75           # optional; fraction of window, lowers the threshold

[compaction.edit]              # optional; tiered-retention context editing
enabled = true                 #   applied to the kept tail at each compact
keep_results = 6               #   tool-result blocks kept verbatim (recent)
keep_thinking = 2             #   thinking blocks kept verbatim (recent)
keep_calls = 6                #   tool-call `code` blocks kept verbatim

[bash]                        # optional; child-process environment for lofi.bash
strip_env = true              # default. Strip the inherited env down to a
                               #   minimal baseline (PATH, HOME, locale) so
                               #   inherited credentials never reach a
                               #   model-run shell. `false` inherits the full
                               #   parent env (trust opt-out).
pass_env = ["GITHUB_TOKEN"]   # optional; names copied from the parent env into
                               #   the child. Their VALUES are redacted
                               #   (`[redacted]`) from command output so a
                               #   command may use a secret without it leaking
                               #   into the transcript. Empty by default.
env_file = "~/.config/lofi/secrets.env"  # optional; KEY=VALUE file loaded into
                               #   the child env (overrides pass_env). `~` is
                               #   expanded; keep it outside the workspace so
                               #   lofi.read/edit/write cannot reach it. Values
                               #   are redacted from output. None by default.

### Shell policy (`policy.toml`)

Shell policy is configured in a separate `policy.toml` file (alongside
`config.toml`; overridable via `$LOFI_POLICY`). Every `lofi.bash` command
is parsed structurally (not string-matched) and evaluated against an
allow / ask / deny policy table. Commands that don't match any rule fail
closed to `ask` (require confirmation).

Three built-in modes are available:

- `read_only` — only read-only commands (ls, cat, grep, git status, …).
- `workspace_write` (default) — read-only plus workspace mutations (cargo,
  make, mkdir, …); destructive ops (rm, git push, …) require confirmation.
- `unrestricted` — everything allowed unless explicitly denied.

```toml
mode = "workspace_write"       # read_only | workspace_write | unrestricted
yolo = false                   # allow-unless-deny: skip confirmation for ask/unmatched

# Custom rules merged on top of the mode's defaults.
[[allow]]
match = "my-tool"
mode = "prefix"               # exact | prefix | substring | args

[[deny]]
match = "curl"
mode = "prefix"

[[ask]]
match = "git commit"
mode = "prefix"

# Auto-mode: LLM-based pre-approval of ask commands.
# When enabled, commands that would normally require confirmation are first
# evaluated by a small LLM. If the model returns "allow", the command runs
# silently. Otherwise the normal confirmation flow applies.
[auto_mode]
enable = true
provider = "openai"           # provider key from config.toml
model = "gpt-4o-mini"         # model id within that provider
timeout_ms = 30000            # optional; default 30s
# max_tokens = 1024           # optional; defaults to the model's max_tokens
```

[retry]                         # optional; transient-error retries
max_retries = 10                # default. Max retry attempts after the initial
                                #   try, for transient provider/transport
                                #   errors (overloaded, 429/5xx, network drops,
                                #   stream truncation). Non-transient errors
                                #   (auth, quota/billing, context overflow)
                                #   are never retried. 0 disables retries.
base_delay_ms = 2000            # default. Base delay (ms) for the first retry;
                                #   later retries double it.
max_delay_ms = 60000            # default. Per-retry delay ceiling (ms); the
                                #   exponential backoff clamps here.
```

## Session transcripts

`lofi` persists each interactive session as a JSON-lines transcript under
`$XDG_STATE_HOME/lofi/sessions/<workspace-slug>/`, one `.jsonl` file per
session. The first line is a `meta` header (`version`, `created`, `cwd`,
`model`); every subsequent line is one session event. `--no-session` skips
persistence entirely (an ephemeral session); `--continue` resumes the most
recent session for the workspace and `--resume <id>` resumes a specific one.

Events form a **tree** via `id` and `parent_id`: each event has a stable id,
its `parent_id` chains to the previous event, and the **active path** is the
walk from the file's leaf (the last-appended event) back to the root. Resuming
rebuilds the agent's in-memory history from the active path alone, so sibling
branches (alternate turns, failed attempts) do not contaminate the model's
context.

The event kinds are:

| `type` | Fields | Meaning |
| --- | --- | --- |
| `message` | `role`, `blocks` | A user/assistant/tool-result message. |
| `tool_timing` | `tool_call_id`, `elapsed_ms` | Wall-clock duration of a completed tool call (so `took Ns` survives resume). |
| `thinking_timing` | `elapsed_ms` | Wall-clock duration of a thinking block. |
| `native_tool` | `parent`, `call_id`, `name`, `args`, `result`, `is_error` | A nested `lofi.<tool>` call inside an `exec` block. |
| `turn_end` | `label`, `elapsed_ms`, `cost`, `usage` | A completed turn — rendered as `◇ label done in Ns`. |
| `turn_failed` | `label`, `elapsed_ms`, `error`, `cost`, `usage` | A turn that errored or was cancelled — rendered as a red `◇ label failed in Ns · <error>`. |

Failed and cancelled turns are **recorded, not dropped**: the rounds that ran
still consumed tokens, so the turn's partial messages, timings, and a
`turn_failed` marker are written with their accumulated cost/usage folded into
the status-bar total. The failed turn's content stays on the active path so it
remains visible on resume (the user can see what was attempted), but
`messages_from_events` skips it via the `turn_failed` boundary when building
the agent's history — the model resumes from the prior `turn_end` checkpoint,
never fed partial or errored content.

Older (v1) transcripts stored bare `Message` JSON with no `id`/`parent_id`;
`lofi` migrates them in-memory on load by chaining each event to the previous
one, so existing transcripts continue to resume without conversion.

## Code mode

The single LLM-facing tool is `exec`:

```jsonc
{
  "code": "<TypeScript source>",
  "strings": { "key": "value" }, // optional; exposed as `lofi_strings`
  "display": { ... }              // optional metadata, ignored by the runtime
}
```

The `code` is type-stripped with swc, wrapped as `(async () => { ... })()` so
top-level `await` and `return` work, then run in a QuickJS context with a
global `lofi` object:

- `lofi.read(path)` → `string`
- `lofi.ls(dir)` → `string` (newline-joined)
- `lofi.find(glob, dir?)` → `string`
- `lofi.grep(pattern | { regex, ic, ctx, max }, path?)` → `string`
- `lofi.write({ path, text })` → `{ ok: true }`
- `lofi.edit({ path, old, new })` → `{ ok: true }` (errors if `old` is missing
  or appears more than once)
- `lofi.bash({ cmd, timeoutMs? })` → `{ ok, output, code, command, directory, signal, duration_ms, status }` (`status` is `"exited"`, `"signaled"`, or `"timeout"`); output keeps the last 20 lines / 4 KB and links to the full pageable log when truncated

File tools canonicalize paths against the workspace root and reject escapes.
`bash` runs with `cwd` = workspace root. `print(...)` buffers into the tool
result's `logs` field (it does not write to the host stdout). The program's
returned value is sent back to the model as the tool result; keep it compact
and final, and keep intermediates in-sandbox.

## TUI keybindings

| Key | Action |
| --- | --- |
| `Enter` | Submit the input box as a prompt. |
| `Ctrl+C` | Cancel the in-flight run. |
| `Ctrl+D` | Quit. |
| `q` | Quit (only when the input box is empty and no run is active). |
| `Esc` | Clear the input box. |
| `Up` / `Down` | Scroll the message log. |

### Slash commands

| Command | Action |
| --- | --- |
| `/help` | Show the keybindings + commands reference. |
| `/clear` | Drop all turns from the log (the transcript file is untouched). |
| `/compact` | Fold the older history into a structured summary, keeping the most recent turn. The summary is injected into the agent's context in place of the folded messages; the full transcript stays on disk. Two auto-triggers: a **hard cap** force-stops a run mid-turn when context crosses `window - reserved_context_tokens`, compacts, and silently continues (gated by `min_messages_between_hard_compacts` so a re-cross too soon after a compact errors out instead of looping); a **soft cap** compacts at `agent_settled` when context is above the optional `[compaction.auto]` threshold. Refused when there is too little to fold. **Tiered retention** (`[compaction.edit]`, default on): at each compact the kept tail is elided — old tool results and tool-call code become `lofi.result`-recoverable stubs, old thinking is dropped, assistant prose is always kept — so the new prefix is ~75–86% lighter and the next compact fires far later. Cache-safe: it rides the prefix rebuild compaction already pays; between compacts the tail is append-only and caches normally. |
| `/model` | Open a picker of the available models and switch the active one mid-session. Entries are listed as `provider/id` (sorted), with a `·thinks` marker for reasoning models and `·img` for image-capable ones; the current model is highlighted. The agent is rebuilt from the retained registry (no remote re-discovery), reusing the per-session tmp dir so `lofi.bash` log state stays reachable; the new model runs with its default thinking level. |
| `/resume` | Open a picker of past sessions for this workspace and resume one. |
| `/tree` | Open a rollback picker over this session's turns. `✎` entries roll back to before a user prompt and prefill the input (edit and resend); `↳` entries roll back to after a turn and leave the input empty (continue from here). The next run branches off the chosen point. |
| `/session` | Print the session path, message count, and model. |
| `/recall [query]` | Search the full session transcript — including messages a `/compact` folded away — and render the matches inline in the log. With no query, browse the most recent entries. Scopes: `scope:lineage` (default, active branch), `scope:all` (whole session), `scope:compaction:N` / `scope:compaction:latest` (within one compaction's summarized range). Search supports regex (`hook|inject`) and multi-word BM25 ranking; `page:N` pages results. The model reaches the same engine via the `lofi.recall` native tool. |
| `/quit` | Exit. |

## Lint strictness

The workspace denies `clippy::unwrap_used`, `clippy::expect_used`,
`clippy::todo`, `clippy::dbg_macro`, and `clippy::print_stdout` across all
crates. Non-test code propagates errors with `?` / `let-else` and writes to
stdout via `std::io::stdout().write_all` (never `println!`); the TUI renders
through ratatui, not stdout. Test modules `allow(clippy::unwrap_used)` so
assertions stay readable.

## Acknowledgements

lofi builds on ideas from several earlier projects:

- **[pi-fabric](https://github.com/monotykamary/pi-fabric)** — for the full code mode concept that inspired lofi's single-tool `exec` approach.
- **[pi](https://github.com/earendil-works/pi)** — for the agent loop architecture that shaped lofi's run loop design.