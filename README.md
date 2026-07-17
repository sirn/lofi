# lofi

`lofi` is a minimal but real coding-agent harness written in Rust, modeled on
Pi's *full code mode*: the model has exactly one tool — `exec` — which compiles
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

`api_key` and header values support Pi-style resolution:

- `!cmd` — run `sh -c cmd` and take the trimmed stdout;
- `$VAR` / `${VAR}` — read the env var (missing is an error);
- `$$` → a literal `$`, `$!` → a literal `!` (escapes);
- anything else is taken literally.

### Example: OpenAI Chat Completions

```toml
default_provider = "openai"

[providers.openai]
base_url = "https://api.openai.com/v1"
api = "openai_completions"
api_key = "$OPENAI_API_KEY"
# `models` is a list of inline tables:
models = [
  { id = "gpt-4o", name = "GPT-4o", supports_image = true, context_window = 128000 },
  { id = "gpt-4o-mini" },
]
```

### Example: OpenAI Responses

```toml
[providers.openai_resp]
base_url = "https://api.openai.com/v1"
api = "openai_responses"
api_key = "$OPENAI_API_KEY"
models = [{ id = "o1" }]
```

### Example: Anthropic Messages

```toml
[providers.anthropic]
base_url = "https://api.anthropic.com"
api = "anthropic_messages"
api_key = "$ANTHROPIC_API_KEY"

[providers.anthropic.headers]
anthropic-beta = "prompt-caching-2024-07-31"

[[providers.anthropic.models]]
id = "claude-opus-4"
name = "Claude Opus 4"
reasoning = true
max_tokens = 4096
```

### Example: remote model discovery

A provider with a `discover` block fetches its model list from
`{base_url}{discover.url}` using the provider's resolved auth, navigates the
JSON response at `discover.path` (dot-separated, default `data`), and merges
the result with its static `models` (**static wins** on `provider/id`
collision). Discovered models are cached to
`$XDG_STATE_HOME/lofi/discovery.json` and reused when the remote fetch fails.

```toml
[providers.openai]
base_url = "https://api.openai.com/v1"
api = "openai_completions"
api_key = "$OPENAI_API_KEY"
# Optional per-entry override of the wire protocol:
discover = { url = "/v1/models", path = "data", api_field = "api" }
```

### Config schema reference

```toml
default_provider = "openai"      # optional
default_model = "openai/gpt-4o"  # optional; "provider/id" or bare id

[providers.<name>]
base_url = "..."
api = "openai_completions" | "openai_responses" | "anthropic_messages"
api_key = "..."                   # optional; resolved per the rules above
headers = { "x-custom" = "..." }  # optional; values resolved
models = [{ id = "...", name = "...", reasoning = bool,
            supports_image = bool, context_window = u64, max_tokens = u64 }]
discover = { url = "/...", path = "data", api_field = "api" }  # optional
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