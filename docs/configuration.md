# Configuration

Lofi reads its main configuration from `$XDG_CONFIG_HOME/lofi/config.toml` (default: `~/.config/lofi/config.toml`). Shell policy is configured separately in `policy.toml` in the same directory.

The configuration directory is read-only from lofi's perspective, so it can be managed declaratively with tools such as Nix or stow. Writable data—including model-discovery cache files and session transcripts—is stored under `$XDG_STATE_HOME/lofi/` (default: `~/.local/state/lofi/`).

Set `$LOFI_CONFIG` to use a different main configuration file, or `$LOFI_POLICY` to use a different shell-policy file.

Any configuration value can be overridden per invocation with an environment variable of the form `LOFI__SECTION__KEY=VALUE`. Use `__` to walk into nested tables; segment names are lowercased before lookup. The value parses as TOML when possible (`200`, `true`, `["a", "b"]`, `'quoted'`), otherwise it is used as a plain string.

```sh
LOFI__RETRY__MAX_RETRIES=0 LOFI__CREDENTIAL__TIMEOUT_MS=1000 lofi
```

The `[credential] timeout_ms` setting controls how long a `!command` credential helper may run before it is rejected (default: 30000). Credential helper output is bounded and a timeout or failure kills the helper's process group.

When `config.toml` is absent, lofi falls back to built-in OpenAI Responses, Anthropic Messages, and Google Generative AI definitions. They become available when `$OPENAI_API_KEY`, `$ANTHROPIC_API_KEY`, or `$GEMINI_API_KEY` is set. An explicit config file replaces that built-in provider tree.

## UI theme

Set `[ui] theme` to `auto`, `light`, or `dark`:

```toml
[ui]
theme = "auto"
```

The default is `auto`. It queries the terminal background with OSC 11 and selects a matching palette. If the terminal does not reply, Lofi uses the dark palette. The `/theme` command changes the palette only for the current session.

## Minimal configuration

A provider and at least one model are enough to get started:

```toml
default_model = "openai/gpt-4o"

[providers.openai]
api_type = "openai-completions"
api_key = "$OPENAI_API_KEY"

[providers.openai.models]
gpt-4o = { name = "GPT-4o", supports_image = true, context_window = 128000 }
```

The default base URL and endpoint path are inferred from `api_type`. Run `lofi --list-models` to verify the resulting model registry.

## Value resolution

`api_key` and custom header values support the following forms:

| Form | Meaning |
| --- | --- |
| `!command` | Run `sh -c command` and use its trimmed stdout. |
| `$VAR` or `${VAR}` | Read an environment variable; a missing variable is an error. |
| `$$` | A literal `$`. |
| `$!` | A literal `!`. |
| Any other value | Use it literally. |

For example:

```toml
[providers.example]
api_type = "openai-completions"
api_key = "!pass show services/example/api-key"

[providers.example.headers]
x-organization = "$EXAMPLE_ORGANIZATION"
x-literal = "$$5"
```

Only `env_name` discovery is lenient: an `env_name` variable that is missing or empty leaves the provider keyless and unavailable. Every explicitly configured value is strict. An `api_key`, provider or model `base_url`, or header value that fails to resolve (missing variable, failed or timed-out command) is a startup error that names the provider and field. A value that resolves to an empty string is treated as unset.

`base_url` (per provider or per model) and `env_name` also resolve the same `$VAR` / `!command` forms, so an endpoint or key can be supplied by an environment variable rather than edited into the config.

To inject values for a single invocation without touching the config, use `-e`/`--env` (repeatable). A bare `NAME` forwards the parent shell's value; `NAME=VALUE` sets it explicitly. These variables are available to `$VAR`/env_name resolution and to `lofi.bash` child processes during that run. For example:

```sh
lofi -e EXAMPLE_API_KEY -e EXAMPLE_BASE_URL="https://api.example.com"
```

## Model selection

### `default_provider`

- Type: string
- Optional.

Provider used when no model is selected on the command line. Its first available model is selected unless `default_model` is also set.

### `default_model`

- Type: string
- Optional.

Default model as `provider/id`. This takes precedence over `default_provider`.

`--model provider/id[:level][@tier]` overrides both values for one invocation. The provider qualifier is required. The thinking and service-tier suffixes are optional.

```toml
default_provider = "anthropic"
default_model = "anthropic/claude-opus-4"
```

## Thinking levels

The default thinking effort can be selected globally, per provider, or per model. Supported values are `off`, `low`, `medium`, `high`, and `xhigh`.

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

Lofi can recover when a provider reports that it stopped for a tool call but
its template parser emits no structured tool call. This recovery is enabled by
default. A separate clean-stop intent detector is disabled by default because
it uses the assistant text as a bounded heuristic.

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

Auto-discovered models use the provider policy. Remote model metadata does not
currently set per-model continuation policy.

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

Service tiers are per-request routing hints forwarded to the provider. They are useful with OpenAI's own plans and with proxies that expose multiple service classes. Supported values are `auto`, `flex`, `priority`, and any provider-defined value (custom values serialize verbatim). `auto` omits the field so the provider uses its default.

The default tier can be selected globally, per provider, or per model, mirroring thinking levels:

```toml
[agent]
service_tier = "flex"

[providers.openai]
api_type = "openai-responses"
api_key = "$OPENAI_API_KEY"
service_tier = "priority"

[providers.openai.models]
"gpt-5.6-sol" = { service_tier = "flex", service_tiers = ["flex", "priority"] }
```

A command-line suffix such as `--model openai/gpt-5.6-sol:high@flex` selects the tier for that run (the `@tier` suffix is optional and combines with the `:level` suffix). Default precedence is model, provider, then `[agent]`; the command line overrides all three. Non-`auto` tiers must appear in the model `service_tiers` list.

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
| `stream_idle_timeout_ms` | positive integer | `90000` | Maximum gap between response-body chunks before the stream fails. |
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

`base_url` is always a host root. The selected protocol's endpoint path is joined to it; lofi does not append another hardcoded suffix.

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

`[compaction]` controls automatic context compaction. The `/compact` command remains available regardless of these settings.

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

A model receives images only when its static or discovered metadata sets `supports_image = true` (see [Model fields](#static-models)). When the active model does not support images, the engine omits image results from the request with a warning rather than failing.

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

`[retry]` controls retries for transient provider and transport failures.

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

`action` is `allow`, `ask`, or `deny`. A redirect to a string in `safe_targets` bypasses the redirect action. Set `allow_fd_dup = true` to permit descriptor duplication such as `2>&1`. Heredoc bodies use the separate `heredocs.action` value. Background operators remain subject to policy evaluation.

### Automatic approval

Automatic mode asks a small model to pre-approve commands that would otherwise require confirmation. Only an `allow` answer skips the normal prompt.

```toml
[auto_mode]
enable = true
provider = "openai"
model = "gpt-4o-mini"
# max_tokens = 1024
```

`provider` is a provider key from `config.toml`; `model` is one of that provider's model ids. Evaluations use that provider's `stream_idle_timeout_ms`, and `max_tokens` defaults to the selected model's output limit.
