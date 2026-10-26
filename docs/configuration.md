# Configuration

Lofi reads `config.toml` from `$XDG_CONFIG_HOME/lofi/`.
The default path is `~/.config/lofi/config.toml`.
Shell approval settings are in `policy.toml` in the same directory.

Set `$LOFI_CONFIG` to use another configuration file.
Set `$LOFI_POLICY` to use another shell policy file.
Lofi stores sessions and model-discovery data in `$XDG_STATE_HOME/lofi/`,
which defaults to `~/.local/state/lofi/`.
The configuration directory remains read-only.

## Quick start

Configure a model provider or an agent backend.
Provider models use Lofi's agent loop.
Backend models use an external agent with Lofi's tools and shell policy.
Both appear in `lofi --list-models` and the `/model` command.

### Model provider

`config.toml`:

```toml
default_model = "openai/gpt-4o"

[providers.openai]
api_type = "openai-completions"
env_name = "OPENAI_API_KEY"

[providers.openai.models]
gpt-4o = { name = "GPT-4o", supports_image = true, context_window = 128000 }
```

Set `OPENAI_API_KEY` in the environment before starting Lofi.
The provider's host and endpoint path come from `api_type`.

When `config.toml` is absent, Lofi uses built-in OpenAI Responses,
Anthropic Messages, and Google Generative AI definitions.
Set `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY` to make
those providers available.
An explicit configuration file replaces the built-in provider definitions.

### Claude Code

Install Claude Code and run `claude auth login`.
Then add this configuration:

```toml
default_model = "claude/sonnet"

[backends.claude]
type = "claude-code"
model = "sonnet"
models = ["sonnet", "opus", "haiku"]
```

Lofi starts the installed `claude` command.
Claude Code uses its own login and credentials.
Lofi owns the conversation and the compaction policy.
Claude Code carries one turn at a time.
Lofi supplies the system prompt and the session's `exec` tool through MCP.
Shell approval uses Lofi's `policy.toml`.

### Run Lofi

```sh
lofi --list-models
lofi
lofi --model claude/sonnet --print "Describe this project"
```

## Model selection

Models use qualified names such as `openai/gpt-4o` or `claude/sonnet`.
The first part is the configured provider or backend name.

| Setting | Type | Description |
| --- | --- | --- |
| `default_model` | string | Model selected at startup. Takes precedence over `default_provider`. |
| `default_provider` | string | Select the first available model from this provider or backend. |

Place these settings at the top of `config.toml`, before any table:

```toml
default_model = "claude/sonnet"
```

`--model <name>/<id>` overrides the configuration for one invocation.
Use `/model` to select a model in the TUI.
Configured backend models are always included in the model list.
Provider models in the TUI require credentials or `no_auth = true`.

Without a default, startup selects the first available provider model in
configuration order, then the first configured backend model.
For a backend, `model` comes first, followed by its `models` list.
An empty backend model configuration supplies the choice `default`.

`--continue` and `--resume <id>` restore the session's selected model and
service tier, including `auto`.
An explicit `--model` overrides that selection.
Switching between provider and backend models keeps the conversation history,
tools, jobs, and shell policy.

Provider models accept optional thinking and service-tier suffixes:
`--model provider/id[:level][@tier]`.
See [Thinking levels](#thinking-levels) and [Service tiers](#service-tiers).

## Agent backends

Each `[backends.<name>]` table defines an agent and its model choices.
The supported backend type is `claude-code`.

### Backend fields

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `type` | string | required | Agent type: `claude-code`. |
| `command` | string | `claude` | Executable name on PATH or path to the executable. |
| `model` | string | none | Initial model. Also included in the model list. |
| `models` | string array | empty | Additional model choices. |
| `context_window` | integer | `200000` | Context window Lofi budgets before the child reports its own. |

For a specific installation, set `command`:

```toml
[backends.claude]
type = "claude-code"
command = "/absolute/path/to/claude"
models = ["sonnet", "opus", "haiku"]
```

Model selection applies to the next turn.
Switching models within a backend keeps its current session.
Selected aliases, such as `sonnet`, are preserved when a session resumes.

### Claude Code sessions

Lofi runs Claude Code in streaming print mode with the Lofi MCP server.
Claude Code's built-in tools, slash commands, hooks, Chrome integration, and
user/project/local settings are disabled for this process.
Managed Claude Code policy can still apply.

Claude Code is a transport, not a second agent.
Lofi tracks the conversation, so the child holds no decision Lofi cannot see.
Claude Code saves its conversation alongside Lofi's transcript.
Lofi reuses the Claude Code session ID when its context matches the conversation.
Messages that the CLI did not see, such as user shell output, travel as a delta.
A cancelled or failed turn closes the child.
The next turn reopens the CLI session from its checkpoint.
After a restart, use `--continue` or `--resume <id>`.
A `/compact` or a changed system prompt rewrites the context.
Lofi then starts a new Claude Code session from the compacted history.

Compaction belongs to Lofi on this backend.
`[compaction]` decides when the context is full and what the summary holds.
A turn stops at `context_window - reserved_context_tokens` and reports
`ContextPressure` like a provider turn, so the same policy applies.
The child is started with `--autocompact 1000000`, so it never rewrites a
transcript Lofi tracks.
Cancelling a turn stops the process group.
A missing Claude Code session produces an error.
`--no-session` disables the Lofi transcript; Claude Code still saves its session.

### Model and usage display

The header uses the model reported when Claude Code starts, including suffixes
such as `[1m]`.
If that report omits the model, the first response supplies the label.
The turn indicator and saved turn records use the model reported for that turn.
Resume restores the header label and the selected model alias.

Claude Code reports cumulative USD cost for its child process.
Lofi stores the spend of each turn as the delta since the previous turn.
A turn record therefore holds its own cost.
A cancelled turn has no reported cost, so it records none.
It still records the usage of the rounds it ran, from the last response the
child reported.
The context gauge uses the context window the child reports.
It falls back to the configured window until the first turn ends.
Claude Code accepts `low`, `medium`, `high`, `xhigh`, and `max` effort levels.
Use `/thinking` to select `--effort` for the next turn; `off` passes no flag.

Automatic shell approval uses the `[auto_mode]` model query in `policy.toml`.
Only an `allow` result approves a command.
Evaluator errors and requests for confirmation keep the normal approval prompt.

`lofi.subagent` works on this backend.
A child is an ordinary backend from the same factory.
A Claude Code session can spawn a Claude Code child or an HTTP child.
An HTTP session can spawn a Claude Code child.
A child that names another model with `lofi.subagent` opens that model's transport.
A child shares the session's job registry, policy, and truncation limits.
It has its own sandbox thread, its own auto-mode context, and its own inbox.

## Value resolution

Provider credentials, headers, and endpoint values support these forms:

| Form | Meaning |
| --- | --- |
| `!command` | Run `sh -c command` and use its trimmed stdout. |
| `$VAR` or `${VAR}` | Read an environment variable. |
| `$$` | A literal `$`. |
| `$!` | A literal `!`. |
| Any other value | Use the value as written. |

```toml
[providers.example]
api_type = "openai-completions"
api_key = "!pass show services/example/api-key"

[providers.example.headers]
x-organization = "$EXAMPLE_ORGANIZATION"
x-literal = "$$5"
```

`env_name` names the environment variable that supplies the API key.
An unset or empty variable leaves the provider unavailable.
Explicit `api_key`, provider or model `base_url`, and header values are strict.
A missing variable or failed command produces a configuration error that names
the provider and field.
A value that resolves to an empty string is treated as unset.

### Credential command timeout

```toml
[credential]
timeout_ms = 30000
```

`timeout_ms` limits how long a `!command` helper can run.
The default is 30000 milliseconds.
Helper output is bounded.
A timeout or failure stops the helper's process group.

### Environment overrides

Use `LOFI__SECTION__KEY=VALUE` to override a setting for one invocation.
Double underscores select nested tables.
Key names are converted to lowercase.
Values are parsed as TOML when possible and as strings otherwise.

```sh
LOFI__DEFAULT_MODEL=claude/opus lofi
LOFI__RETRY__MAX_RETRIES=0 lofi
```

Use `-e` or `--env` to supply environment variables.
A bare name forwards the parent shell's value.
`NAME=VALUE` sets a value explicitly.
Repeat the flag to supply more variables.
These values are available during configuration resolution and to
`lofi.bash` child processes.

```sh
lofi -e EXAMPLE_API_KEY -e EXAMPLE_BASE_URL="https://api.example.com"
```

## UI theme

```toml
[ui]
theme = "auto"
```

`theme` accepts `auto`, `light`, or `dark`.
The default is `auto`.
Lofi queries the terminal background with OSC 11 to select a theme.
It uses the dark theme if the terminal gives no reply.
Use `/theme` to change the theme for the current session.

## Thinking levels

For provider models, select the default thinking effort globally, per provider,
or per model.
Supported values are `off`, `low`, `medium`, `high`, and `xhigh`.

```toml
[agent]
thinking_level = "medium"

[providers.openai]
api_type = "openai-responses"
api_key = "$OPENAI_API_KEY"
thinking_level = "high"

[providers.openai.models]
"o3" = { reasoning = true, thinking_level = "medium", thinking_levels = ["low", "medium", "high"] }
```

A command-line suffix such as `--model openai/o3:high` selects the level for that run. Default precedence is model, provider, then `[agent]`; the command line overrides all three. Non-`off` levels must appear in the model `thinking_levels` list.

## Automatic turn continuation

`auto_continue` controls recovery in Lofi's agent loop for provider models.
`lost_tool_call` is enabled by default.
It recovers when a provider reports a tool-call stop without a structured tool call.
`intent` is disabled by default.
It uses the final assistant paragraph to detect unfinished tool work.

```toml
[agent.auto_continue]
lost_tool_call = true
intent = false

[providers.example.auto_continue]
intent = true

[providers.example.models.broken-template.auto_continue]
lost_tool_call = false
```

The settings are resolved field by field in this order:

1. Built-in defaults.
2. `[agent.auto_continue]`.
3. `[providers.<name>.auto_continue]`.
4. The static model override, when present.

Auto-discovered models use the provider policy.

`lost_tool_call` recovers a protocol contradiction: the provider reports a
tool-use stop but sends no tool call. Its built-in default is `true`.

`intent` recovers a clean end-of-turn whose final short paragraph announces an
immediate tool-related action, such as “I’ll run the tests next.” Its built-in
default is `false`. Enable it only for providers or models whose templates are
known to lose tool calls.

All automatic recovery paths share a budget of one continuation per user turn.
The recovery prompt is stored as a notice-kind message, not as user-authored
input.

## Service tiers

Service tiers select the request service class for provider models.
Supported values are `auto`, `flex`, `priority`, and provider-defined values.
`auto` uses the provider's default.
Provider-defined values are sent as written.

The default tier can be selected globally, per provider, or per model, mirroring thinking levels:

```toml
[agent]
service_tier = "flex"

[providers.openai]
api_type = "openai-responses"
api_key = "$OPENAI_API_KEY"
service_tier = "priority"

[providers.openai.models]
o3 = { reasoning = true, thinking_levels = ["low", "medium", "high"], service_tier = "flex", service_tiers = ["flex", "priority"] }
```

A command-line suffix such as `--model openai/o3:high@flex` selects the tier for that run (the `@tier` suffix is optional and combines with the `:level` suffix). Default precedence is model, provider, then `[agent]`; the command line overrides all three. Non-`auto` tiers must appear in the model `service_tiers` list.

## Providers

Each `[providers.<name>]` table defines authentication, protocol routing, and its available models.

### Provider fields

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `api_type` | string | `openai-completions` | Default wire protocol. |
| `base_url` | string | Protocol host | Host root; the endpoint path is joined to it. |
| `api_key` | string | none | Explicit key or value-resolution expression. Overrides `env_name`. |
| `env_name` | string | none | Environment variable containing the API key. |
| `headers` | table | none | Additional HTTP headers; values support value resolution. |
| `no_auth` | boolean | `false` | Make the provider available without authentication headers. |
| `response_start_timeout_ms` | positive integer | `90000` | Maximum wait for connection and response headers. Successful streaming response bodies have no fixed deadline. |
| `thinking_level` | string | inherited | Provider-level thinking default. |
| `thinking_levels` | string array | empty | Provider capability metadata. Declare supported levels on each static model. |
| `service_tier` | string | inherited | Provider-level service-tier default. |
| `service_tiers` | string array | empty | Provider capability metadata. Declare supported tiers on each static model. |
| `pricing_convention` | string | `per_token` | Remote pricing is `per_token` or `per_million`. |
| `pricing_field_mappings` | table | standard `pricing.*` paths | Dot paths for discovered input, output, cache, and per-request prices. |
| `api_types` | table | empty | Per-protocol endpoint paths and optional pricing mappings for a shared host. |
| `auto_models` | table | none | Remote model discovery configuration. |
| `auto_continue` | table | inherited | Provider-level `lost_tool_call` and `intent` overrides. |

Supported protocols are:

| `api_type` | Default host | Default path |
| --- | --- | --- |
| `openai-completions` | `https://api.openai.com` | `/v1/chat/completions` |
| `openai-responses` | `https://api.openai.com` | `/v1/responses` |
| `anthropic-messages` | `https://api.anthropic.com` | `/v1/messages` |
| `google-generative-ai` | `https://generativelanguage.googleapis.com` | `/v1beta` |

### Endpoint routing with `api_types`

A provider's `base_url` is a host root.
Lofi joins the selected protocol's endpoint path to it.
A model-level `base_url` is a full endpoint URL.

A single-protocol provider normally only needs `api_type`:

```toml
[providers.openai]
base_url = "https://api.openai.com"
api_type = "openai-completions"
api_key = "$OPENAI_API_KEY"
```

Use `api_types` when one host routes multiple protocols:

```toml
[providers.proxy]
base_url = "https://proxy.example.com"
api_type = "openai-completions"
api_key = "$PROXY_API_KEY"

[providers.proxy.api_types."openai-completions"]
path = "/v1/chat/completions"

[providers.proxy.api_types."openai-responses"]
path = "/v1/responses"

[providers.proxy.api_types."anthropic-messages"]
path = "/v1/messages"

[providers.proxy.api_types."google-generative-ai"]
path = "/v1beta"

[providers.proxy.models]
chat = { api_type = "openai-completions" }
reasoner = { api_type = "openai-responses", reasoning = true }
claude = { api_type = "anthropic-messages" }
```

A model's `api_type` override and a discovered model's mapped protocol both resolve through this same table. Omitting `path` uses the protocol's default path.

## Static models

Static models are keys in `[providers.<name>.models]`. An empty inline table is valid when no metadata is needed.

```toml
[providers.openai.models]
"gpt-4o-mini" = {}

[providers.openai.models.gpt-4o]
name = "GPT-4o"
supports_image = true
context_window = 128000
max_tokens = 16384
input_price = 2.50
output_price = 10.00
```

### Model fields

| Field | Type | Description |
| --- | --- | --- |
| `name` | string | Display name; defaults to the model id. |
| `api_type` | string | Per-model protocol override. |
| `reasoning` | boolean | Whether the model exposes reasoning. |
| `supports_image` | boolean | Whether the model accepts image input. |
| `context_window` | integer | Context-window size in tokens. |
| `max_tokens` | integer | Maximum output tokens. |
| `thinking_level` | string | Default thinking level. |
| `thinking_levels` | string array | Supported thinking levels. |
| `service_tier` | string | Default service tier. |
| `service_tiers` | string array | Supported service tiers. |
| `base_url` | string | Full per-model endpoint URL override. |
| `input_price` | number | USD per million input tokens. |
| `output_price` | number | USD per million output tokens. |
| `cache_read_price` | number | USD per million cache-read tokens. |
| `cache_write_price` | number | USD per million cache-write tokens. |
| `per_request_price` | number | Flat USD cost per request. |
| `auto_continue` | table | Model-level `lost_tool_call` and `intent` overrides. |

### OpenAI Chat Completions example

```toml
[providers.openai]
api_type = "openai-completions"
api_key = "$OPENAI_API_KEY"

[providers.openai.models]
gpt-4o = { name = "GPT-4o", supports_image = true, context_window = 128000 }
"gpt-4o-mini" = {}
```

### OpenAI Responses example

```toml
[providers.openai]
api_type = "openai-responses"
api_key = "$OPENAI_API_KEY"

[providers.openai.models]
o1 = { reasoning = true, thinking_levels = ["low", "medium", "high"] }
```

### Anthropic Messages example

```toml
[providers.anthropic]
api_type = "anthropic-messages"
api_key = "$ANTHROPIC_API_KEY"

[providers.anthropic.models]
"claude-opus-4" = { name = "Claude Opus 4", reasoning = true, max_tokens = 4096 }
```

Lofi automatically adds ephemeral prompt-cache breakpoints to Anthropic
requests at the tool definitions, system prompt, and latest user message. No
cache header or per-message configuration is required.

### Google Generative AI example

```toml
[providers.google]
api_type = "google-generative-ai"
api_key = "$GEMINI_API_KEY"

[providers.google.models."gemini-3.7-flash"]
reasoning = true
supports_image = true
thinking_levels = ["low", "medium", "high"]
```

The native transport preserves Gemini thought signatures on model parts and
replays them only with the same provider and model. The OpenAI Chat Completions
transport also preserves Gemini signatures returned in tool-call
`extra_content.google` or `extra_content.vertex` fields.

### Unauthenticated local provider example

```toml
[providers.local]
base_url = "http://127.0.0.1:8000"
api_type = "openai-completions"
no_auth = true

[providers.local.models]
"local-model" = {}
```

## Remote model discovery

An `auto_models` block fetches an OpenAI-style model list at startup. Remote models are merged with static models. On an id collision, static values override discovered values; fields omitted by the static entry remain discovered.

Discovered models are cached in `$XDG_STATE_HOME/lofi/discovery.json` and the cache is used when a remote fetch fails. `models_url` defaults to a `/models` endpoint derived from the provider's default API path (for example, `https://api.openai.com/v1/models`).

```toml
[providers.proxy]
base_url = "https://proxy.example.com"
api_type = "openai-completions"
api_key = "$PROXY_API_KEY"

[providers.proxy.auto_models]
enabled = true
# models_url = "https://proxy.example.com/v1/models"
path = "data"
auth = true
ttl_seconds = 300
api_type_field = "preferred_api"
thinking_levels = ["low", "medium", "high"]
thinking_level = "medium"

[providers.proxy.auto_models.api_type_mappings]
chat_completions = "openai-completions"
responses = "openai-responses"
messages = "anthropic-messages"

[providers.proxy.auto_models.field_mappings]
name = "name"
context_window = "context_length"
max_tokens = "top_provider.max_completion_tokens"
```

### Discovery fields

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | boolean | `false` | Enable discovery. |
| `models_url` | string | derived | Full model-list URL. |
| `path` | string | `data` | Dot path to the model array in the response. |
| `auth` | boolean | `true` | Send provider credentials while fetching models. |
| `ttl_seconds` | integer | `300` | Discovery-cache freshness. |
| `api_type_field` | string | none | Field containing the remote protocol name. |
| `thinking_level` | string | none | Default for discovered models. |
| `thinking_levels` | string array | empty | Levels exposed by discovered models. |
| `service_tier` | string | none | Default for discovered models. |
| `service_tiers` | string array | empty | Tiers exposed by discovered models. |

When `api_type_field` is set, its remote value is translated through `api_type_mappings`. An absent mapping falls back to the provider's default `api_type`.

## Discovery pricing

Pricing is read from discovered model entries through dot-path mappings. The provider-level mappings apply to every protocol unless an `api_types` entry overrides them.

```toml
[providers.proxy]
pricing_convention = "per_token" # per_token | per_million

[providers.proxy.pricing_field_mappings]
input = "pricing.prompt"
output = "pricing.completion"
cache_read = "pricing.input_cache_read"
cache_write = "pricing.input_cache_write"
per_request = "pricing.request"

[providers.proxy.api_types."openai-responses".pricing_field_mappings]
input = "responses_pricing.input"
output = "responses_pricing.output"
```

All mappings are optional. The defaults are the four `pricing.*` token paths shown above, with no per-request path.

## Compaction

`[compaction]` controls context compaction for every transport.
The soft and hard thresholds read the same fill on either transport.
Use `/compact` to compact the context manually.
Claude Code keeps its own context inside the child process.
A compaction rewrites Lofi's transcript, and the next turn starts a new child.
The child never resumes the discarded session.

```toml
[compaction]
reserved_context_tokens = 20000
min_messages_between_hard_compacts = 6

[compaction.auto]
enable = true
max_context_tokens = 150000
context_ratio = 0.75

[compaction.edit]
enabled = true
keep_results = 6
keep_thinking = 2
keep_calls = 6
```

### Hard compaction

`reserved_context_tokens` (default `20000`) reserves room for model output. When a run crosses `context_window - reserved_context_tokens`, lofi stops the run, compacts, and silently continues it.

`min_messages_between_hard_compacts` (default `6`) prevents a loop when the retained tail is itself too large. Crossing the hard limit again too soon produces an error instead of another compaction.

### Soft compaction with `[compaction.auto]`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | boolean | `true` | Master switch for soft compaction. |
| `max_context_tokens` | integer | none | Absolute soft threshold. |
| `context_ratio` | number | none | Fraction of the model's context window used as a soft threshold. |

When both thresholds are present, the lower threshold wins. Soft compaction is checked after the agent settles; it does not interrupt a run. Thresholds and hard limits compare against the round's full context fill: input, output, cache-read, and cache-write tokens, the same total the footer gauge shows. With neither threshold set, compaction is hard-limit-only.

### Tiered retention with `[compaction.edit]`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | boolean | `true` | Edit the retained tail during compaction. |
| `keep_results` | integer | `6` | Recent tool-result blocks kept verbatim. |
| `keep_thinking` | integer | `2` | Recent thinking blocks kept verbatim. |
| `keep_calls` | integer | `6` | Recent tool-call code blocks kept verbatim. |

Older tool results and tool-call code become event-id stubs; the compaction handoff tells the model to recover them with `lofi.result(eventId)`. Older thinking is dropped, while assistant prose is retained.

## Bash environment

`[bash]` controls the child-process environment of `lofi.bash`.

```toml
[bash]
strip_env = true
pass_env = ["GITHUB_TOKEN"]
env_file = "~/.config/lofi/secrets.env"
```

### `strip_env`

- Type: boolean
- Default: `true`

Strips the inherited environment to a minimal baseline containing values such as `PATH`, `HOME`, and locale settings. Setting this to `false` explicitly trusts model-run shell commands with the full parent environment.

### `pass_env`

- Type: array of strings
- Default: empty

Copies named variables from the parent environment into the child. Their values are replaced with `[redacted]` in captured command output.

### `env_file`

- Type: path
- Default: none

Loads `KEY=VALUE` entries into the child environment, overriding values from `pass_env`. `~` is expanded. Keep this file outside the workspace so the agent's file tools cannot read it. Values are redacted from command output.

## Truncation

`[truncate]` sets the visible-output cap that both `lofi.read` and `lofi.bash` apply to their results.

```toml
[truncate]
max_lines = 2000
max_bytes = 51200
```

### `max_lines`

- Type: integer
- Default: `2000`

Maximum number of lines a tool returns before truncating to the first (`read`) or last (`bash`) lines and linking a full log.

### `max_bytes`

- Type: integer (bytes)
- Default: `51200` (50 KiB)

Maximum number of bytes a tool returns before truncating and linking a full log. Both limits default to the same 2000 lines / 50 KiB cap.

## Images

`[image]` sets the limits applied when `lofi.read` reads an image for the model. The image is downscaled to fit `max_width`×`max_height` (preserving aspect ratio) and re-encoded as JPEG, sweeping quality down until the payload fits `max_bytes`. These bounds keep the image payload sent to the provider small. Pasted file paths remain text so the model can choose whether to read them.

A model receives images only when its static or discovered metadata sets `supports_image = true` (see [Model fields](#model-fields)). When the active model does not support images, the engine omits image results from the request with a warning rather than failing.

```toml
[image]
max_width = 2000
max_height = 2000
max_bytes = 1048576
```

### `max_width`

- Type: integer (pixels)
- Default: `2000`

Maximum image width after downscaling.

### `max_height`

- Type: integer (pixels)
- Default: `2000`

Maximum image height after downscaling.

### `max_bytes`

- Type: integer (bytes)
- Default: `1048576` (1 MiB)

Maximum size of the re-encoded JPEG payload.

## Retries

`[retry]` controls retries for provider models.
Claude Code backend turns require explicit resume after a disconnect.

```toml
[retry]
max_retries = 10
base_delay_ms = 2000
max_delay_ms = 60000
```

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `max_retries` | integer | `10` | Attempts after the initial request; `0` disables retries. |
| `base_delay_ms` | integer | `2000` | Delay before the first retry. |
| `max_delay_ms` | integer | `60000` | Per-retry delay ceiling. |

Later delays use exponential backoff. Overload responses, HTTP 429/5xx, network drops, and truncated streams are retried. Authentication, quota/billing, and context-overflow failures are not.

## Shell policy

Shell policy lives in `policy.toml`, alongside `config.toml` by default. Every `lofi.bash` command is parsed structurally and evaluated against allow, ask, and deny rules. Unmatched commands fail closed to `ask`.

`lofi.bash` runs through host `sh -c`. The policy controls approval; it is not a filesystem or network sandbox. A command approved by the user can read or modify anything available to that user. The default therefore requires confirmation for every model-generated shell command. Filesystem tools such as `lofi.read`, `lofi.write`, and `lofi.edit` remain confined to their registered roots.

### Modes

| Mode | Behaviour |
| --- | --- |
| `confirm` | Requires confirmation for every command except universal denials. This is the default. |
| `read_only` | Automatically approves a command-classification allowlist such as `ls`, `cat`, `grep`, and `git status`. It does not confine paths. |
| `workspace_write` | Adds builds, interpreters, package managers, and mutation commands to the automatic-approval allowlist. It does not confine writes to the workspace. |
| `unrestricted` | Allows everything unless an explicit rule denies it. |

```toml
mode = "confirm"
yolo = false
```

Network clients are not automatically approved by the built-in restricted modes. Output redirects require confirmation by default. Both can be opted into with custom rules and redirect settings.

`yolo = true` skips confirmation for `ask` and unmatched commands, but does not bypass explicit deny rules.

### Custom rules

Custom rules are merged on top of the selected mode's defaults:

```toml
[[allow]]
match = "my-tool"
mode = "prefix"

[[ask]]
match = "git commit"
mode = "prefix"

[[deny]]
match = "curl"
mode = "prefix"
```

Each rule has a required `match` string and one of these matching modes:

| Mode | Behaviour |
| --- | --- |
| `exact` | Match the entire command. |
| `prefix` | Match the beginning of the command at a word boundary. |
| `substring` | Match a contiguous token sequence anywhere in the command. |
| `args` | Match a command prefix and required argument tokens. |

### Command wrappers

The evaluator extracts commands nested inside common wrappers such as `sh -c`, `env`, `xargs`, and `podman run`. Add a wrapper when a project-specific command carries another command as an argument:

```toml
[[wrappers]]
name = "project-shell"
kind = "shell_c"
```

Supported `kind` values are:

| Kind | Inner command |
| --- | --- |
| `shell_c` | The argument after `-c`. |
| `utility_operand` | The first non-option operand and all following arguments. |
| `env` | The command after options and environment assignments. |
| `xargs` | The command operand and all following arguments. |
| `docker_run` | The container command after `run`, `exec`, or `create`; also applies to compatible tools such as Podman. |

Built-in wrappers already cover common shells, privilege and utility wrappers, `env`, `xargs`, Docker, and Podman. Custom entries extend that set.

### Redirects and heredocs

Redirect and heredoc decisions are independent from command matching. Their default action is `ask`:

```toml
[redirects]
action = "ask"
safe_targets = ["/dev/null"]
allow_fd_dup = false

[heredocs]
action = "ask"
```

`action` is `allow`, `ask`, or `deny`. A redirect to a string in `safe_targets` bypasses the redirect action. Set `allow_fd_dup = true` to permit descriptor duplication such as `2>&1`. Heredoc bodies use the separate `heredocs.action` value. Bare `&`, `nohup`, `setsid`, and `disown` are always denied; use `lofi.jobSpawn` for background work.

### Automatic approval

Automatic mode asks a small model to pre-approve commands that would otherwise require confirmation. The evaluator answers with one risk level:
- `low` runs the command without a prompt.
- `confirm` shows the normal confirmation prompt.
- `block` shows the normal confirmation prompt with the evaluator's reason.

Auto mode never denies a command. An unparseable answer, a provider error, or a timeout shows the normal prompt too.
A read outside the workspace, a public read, and a read-only `ssh` command stay `low`.
A public read is a `GET`, a `HEAD`, or a GraphQL query, with or without a token.
A write outside the workspace, a saved download, an upload, and a read of a credential file show the prompt.

The evaluator sees the command, the workspace root, and the request that started the turn. For a request of three words or fewer, it also sees the previous request and the agent's last message, so a terse "yes" is judged against the question it answers. The evaluator treats that context as untrusted data: it can raise the risk, never lower it. The context comes from the live history, so compaction, rewind, and a new session drop it. A subagent never inherits the context of its parent.

```toml
[auto_mode]
enable = true
model = "openai/gpt-4o-mini"
# max_tokens = 1024
# timeout_ms = 10000
```

`model` is a model query: a provider key or a backend key, then a model, as everywhere else in the configuration. A config that still names `provider` and a bare `model` keeps working; the two fields join into one query. A backend name runs the evaluation on that CLI backend, so a session on a CLI backend evaluates commands exactly like an HTTP one. When the evaluator runs on a provider, it uses that provider's `response_start_timeout_ms` as a hard ceiling. `max_tokens` defaults to the selected model's output limit. `timeout_ms` defaults to `10000` and caps one evaluation, because a stalled evaluator holds the command and the turn with it. A timeout is a failure, so the command falls back to the normal prompt.
