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
- **Subagents** via `lofi.agent(prompt, opts?)` — a nested agent loop with no
  iteration cap (bounded by per-call timeouts).
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
| `--print <PROMPT>` | Run one non-interactive turn and stream assistant text to stdout. |
| `--list-models` | Print `provider/id — name` lines and exit. |
| `--provider <NAME>` | Select the provider (overrides `default_provider`). |
| `--model <ID>` | Select a model by `provider/id`, raw id, name, or substring. |
| `--api-key <KEY>` | Override the selected provider's resolved `api_key` (taken literally). |

With no flags, `lofi` launches the interactive TUI against the current working
directory as the workspace root.

## Configuration

`lofi` reads its user config from `$XDG_CONFIG_HOME/lofi/config.toml` (default
`~/.config/lofi/config.toml`). This tree is treated as **read-only** — `lofi`
never writes there — so a config manager (Nix, stow, etc.) can own it
declaratively. Agent-owned, writable state (the remote-discovery cache, future
sessions/logs) lives under `$XDG_STATE_HOME/lofi/` (default
`~/.local/state/lofi/`). The split keeps a managed config tree pristine while
`lofi` still has a writable home for its cache.

### Value resolution

`api_key` and header values support value resolution:

- `!cmd` — run `sh -c cmd` and take the trimmed stdout;
- `$VAR` / `${VAR}` — read the env var (missing is an error);
- `$$` → a literal `$`, `$!` → a literal `!` (escapes);
- anything else is taken literally.

### Endpoint URLs

A provider's `base_url` is the **host root** (e.g. `https://api.openai.com`).
The full endpoint URL each model POSTs to is built by joining `base_url` with
the `path` of the selected api-type mapping (see below). The provider never
appends a hardcoded suffix — the path is explicit in the config, with sensible
defaults (`/v1/chat/completions`, `/v1/responses`, `/v1/messages`) when
omitted.

### `api_type` — protocol and endpoint routing

Each provider has an `api_type` table mapping a remote api-type string to the
lofi wire protocol and the endpoint path. The common case is a single-protocol
provider, which can be written as a scalar:

```toml
[providers.openai]
base_url = "https://api.openai.com"
api_type = "openai-completions"
```

This is shorthand for a one-entry table keyed by `chat_completions` (the
default api-type key). The full table form is for proxies that route one host
to several upstream APIs:

```toml
[providers.proxy]
base_url = "https://proxy.example.com"
default_api_type = "chat_completions"

[providers.proxy.api_type.chat_completions]
api = "openai-completions"
path = "/v1/chat/completions"      # optional; defaults to /v1/chat/completions

[providers.proxy.api_type.responses]
api = "openai-responses"
path = "/v1/responses"             # optional; defaults to /v1/responses

[providers.proxy.api_type.messages]
api = "anthropic-messages"
path = "/v1/messages"              # optional; defaults to /v1/messages
```

`default_api_type` names the key used when a model does not report its own
api-type (static models, or discovered models with no `preferred_api` field).
It defaults to `chat_completions`.

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
from the default api-type mapping's `path` (the version prefix plus `/models`,
e.g. `/v1/models`). Each discovered model's `api` and endpoint `base_url` are
resolved from the provider's `api_type` table — keyed by the `api_type_field`
value the endpoint reports (e.g. `preferred_api`), falling back to
`default_api_type`. Pricing is read via the mapping's `pricing_field_mappings`
(falling back to the provider-level default) and scaled by the provider's
`pricing_convention`.

```toml
[providers.openai]
base_url = "https://api.openai.com"
api_type = "openai-completions"
api_key = "$OPENAI_API_KEY"

[providers.openai.auto_models]
enabled = true
# models_url defaults to {base_url}/v1/models
api_type_field = "preferred_api"   # optional; field naming the per-model api-type

[providers.openai.pricing_field_mappings]   # optional; defaults shown
input = "pricing.prompt"
output = "pricing.completion"
cache_read = "pricing.input_cache_read"
cache_write = "pricing.input_cache_write"
```

### Config schema reference

```toml
default_provider = "openai"      # optional
default_model = "openai/gpt-4o"  # optional; "provider/id" or bare id

[providers.<name>]
base_url = "..."                 # host root; optional (defaults per api_type)
api_type = "..."                 # scalar or table; see above
default_api_type = "..."         # optional; defaults to "chat_completions"
api_key = "..."                   # optional; resolved per the rules above
env_name = "..."                  # optional; env var for the api key
headers = { "x-custom" = "..." }  # optional; values resolved
no_auth = false                   # optional; omit auth headers

[providers.<name>.api_type.<key>] # optional; per-endpoint override
api = "openai_completions" | "openai_responses" | "anthropic_messages"
path = "/v1/chat/completions"     # optional; defaults per api

[providers.<name>.pricing_field_mappings]  # optional; defaults shown above
input = "..."
output = "..."
cache_read = "..."
cache_write = "..."

# pricing_convention = "per_token" | "per_million"  # optional; default per_token

[providers.<name>.models]        # optional; static models, keyed by id
"<id>" = { name = "...", reasoning = bool, supports_image = bool,
            context_window = u64, max_tokens = u64,
            thinking_levels = ["low","medium","high","xhigh"],
            input_price = f64, output_price = f64,
            cache_read_price = f64, cache_write_price = f64 }

[providers.<name>.auto_models]   # optional; remote discovery
enabled = true
models_url = "..."               # optional; defaults to {base_url}/v1/models
api_type_field = "..."           # optional
```

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
- `lofi.bash({ cmd, timeoutMs? })` → `{ ok, output, code }`
- `lofi.agent(prompt, opts?)` → the subagent's final assistant text

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

## Lint strictness

The workspace denies `clippy::unwrap_used`, `clippy::expect_used`,
`clippy::todo`, `clippy::dbg_macro`, and `clippy::print_stdout` across all
crates. Non-test code propagates errors with `?` / `let-else` and writes to
stdout via `std::io::stdout().write_all` (never `println!`); the TUI renders
through ratatui, not stdout. Test modules `allow(clippy::unwrap_used)` so
assertions stay readable.