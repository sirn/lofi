# Lofi

Lofi is a minimal coding-agent harness written in Rust.

## Building

```sh
cargo build --release
target/release/lofi
```

## Usage

### CLI flags

| Flag | Description |
| --- | --- |
| `-p`, `--print <PROMPT>` | Run one non-interactive turn and stream assistant text to stdout. |
| `--list-models` | Print `provider/id — name` lines and exit. |
| `--list-sessions` | Print saved sessions for this workspace and exit. |
| `--model <SPEC>` | Select the model as `provider/model[:level]`. |
| `-c`, `--continue` | Resume the most recent session for this workspace. |
| `--resume <ID>` | Resume a specific session by id prefix. |
| `--no-session` | Do not persist a transcript. |
| `--docs` | Print the embedded API reference index. |
| `--docs-search` | Search the embedded API reference. |
| `--policy-explain` | Evaluate a command against the shell policy (dry-run). |

With no flags, `lofi` launches the interactive TUI against the current working directory as the workspace root.

Model-generated shell commands run on the host through `sh -c`; QuickJS isolation does not sandbox native commands. The default policy asks for confirmation before every such command. See [Shell policy](docs/configuration.md#shell-policy) before selecting a more permissive approval mode.

### TUI keybindings

| Key | Action |
| --- | --- |
| `Enter` | Submit the input box as a prompt. |
| `Ctrl+C` | Cancel the in-flight run. |
| `Ctrl+D` | Quit. |
| `q` | Quit (only when the input box is empty and no run is active). |
| `Esc` | Clear the input box. |
| `Up` / `Down` | Scroll the message log. |

### Shell commands

Prefix input with `!` to run it directly through `sh -c` in the workspace. The captured output is included in subsequent model context; use `!!` instead to run it without adding it to context. Direct shell commands have no wall-clock timeout, are persisted in session transcripts, and can be cancelled with `Ctrl+C`.

### Slash commands

| Command | Action |
| --- | --- |
| `/help` | Show the keybinding and command reference. |
| `/clear` | Drop all turns from the log (retain the transcript). |
| `/compact` | Fold the older history into a structured summary. |
| `/model` | Open a model picker. |
| `/resume` | Open a sessions picker. |
| `/tree` | Open a rollback picker. |
| `/session` | Print the session path, message count, and model. |
| `/quit` | Exit. |

## Configuration

See **[Configuration](docs/configuration.md)**.

## Paths

Lofi makes a distinction between user configuration and harness-managed state:

- `${XDG_CONFIG_DIR}/lofi` (usually `~/.config/lofi`) for user-edited configuration (`config.toml`, `AGENTS.md`, `skills/`).
- `${XDG_STATE_HOME}/lofi` (usually `~/.local/state/lofi`) for other agent state.

## Acknowledgements

Lofi builds on ideas from several earlier projects:

- **[pi](https://github.com/earendil-works/pi)**
- **[pi-fabric](https://github.com/monotykamary/pi-fabric)**
- **[pi-vcc](https://github.com/monotykamary/pi-vcc)**
