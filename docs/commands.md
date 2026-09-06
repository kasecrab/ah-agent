# Commands

## Modes

| Invocation | Mode |
|---|---|
| `ah` | TUI (needs a terminal on stdin and stdout) |
| `ah -p "fix the failing test"` or `ah fix the failing test` | one-shot: streams the reply to stdout, runs tools, exits |
| `echo "explain this" \| ah` | one-shot with the prompt from stdin |
| `ah --json -p "..."` | one-shot emitting JSONL events (below) |
| `ah --tui -p "..."` | TUI with the prompt submitted first |
| `ah -r` / `ah --resume work` | TUI resuming the latest session, or the newest named `work` |

One-shot mode keeps the session in memory only; the TUI writes a session file
(`ah docs sessions`).

## Flags

| Flag | Effect |
|---|---|
| `-p, --prompt TEXT` | one-shot prompt |
| `-m, --model ID` | model id for this run |
| `--yolo` | run every tool call without asking (default) |
| `--ask` | prompt before `bash`, `write_file`, `edit_file` (`permissions.ask_for`) |
| `--no-plugins` | load no plugins |
| `--plugin PATH` | extra plugin file or directory, repeatable |
| `--cwd DIR` | working directory for tools |
| `-s, --system TEXT` | appended to the system prompt (`prompt.append`) |
| `--max-tokens N` | completion limit |
| `--set KEY=VALUE` | override any setting, repeatable (`ah docs config`) |
| `--json` | JSONL events instead of text in one-shot mode |
| `-r, --resume [ID\|NAME]` | resume a session |
| `--tui` | force the TUI |
| `-V, --version` | print the version without loading anything |

## Subcommands

| Command | Effect |
|---|---|
| `ah login` | show auth status, then prompt for an OpenRouter key (hidden), verify it and store it with mode 600. `--key K` skips the prompt |
| `ah logout` | remove the stored key |
| `ah models [FILTER] [--tools] [--refresh]` | list the cached OpenRouter catalogue, fuzzy filtered; `--tools` keeps tool-calling models; `--refresh` refetches |
| `ah plugin list` | discovered plugins and their load status |
| `ah plugin add FILE.wasm` | copy into the user plugin dir |
| `ah plugin rm NAME` | remove from the user plugin dir |
| `ah plugin build [DIR] [--no-install]` | build a plugin crate for wasm32 and install it |
| `ah plugin install SOURCE [SUBDIR] [--ref REF]` | clone a git repository (URL, `owner/repo`, GitHub `tree` link or local path), build the plugin in it or in `SUBDIR`, install it and record the source |
| `ah plugin update [NAME]` | reinstall plugins that came from git |
| `ah config [show] [--origins]` | merged settings as TOML |
| `ah config path` | config, credentials, plugin and session locations |
| `ah config init [--force]` | write a commented default config |
| `ah sessions` | stored sessions |
| `ah docs [TOPIC]` | list these pages, or print one |

## Slash commands (TUI)

Type `/` in an empty input for a popup filtered as you type; Tab fills the
name in, Enter runs it. Plugins can add commands (`ah docs plugins`).

| Command | Effect |
|---|---|
| `/help`, `/?` | list commands and keys |
| `/model [ID\|refresh]` | fuzzy picker over the catalogue; a known id or favorite name switches directly; `refresh` refetches |
| `/effort [LEVEL]` | reasoning effort: `off`, `minimal`, `low`, `medium`, `high`, `xhigh` |
| `/favorite [NAME]`, `/fav` | favorites picker; a name switches to it or creates it |
| `/usage` | session cost per model, wall and API time, tool calls, lines changed, plus OpenRouter balance, limits and 30-day top models (`r` refreshes) |
| `/compact [FOCUS]` | summarise the conversation now, optionally around a focus |
| `/clear` | start an empty conversation in the same session file |
| `/resume [ID\|NAME]` | switch to another session in place |
| `/rename [NAME]` | name the session; empty removes the name |
| `/session` | session id and file |
| `/skills [NAME] [ARGS]`, `/skill` | run a saved prompt (`ah docs skills`) |
| `/init` | ask the model to write or refresh `AGENTS.md` (`ah docs instructions`) |
| `/set KEY VALUE` | override a setting for this run |
| `/config` | config paths and active layers |
| `/reload` | re-read config files and reload plugins |
| `/plugins` | active plugins |
| `/tools` | tools offered to the model |
| `/keys` | active key bindings |
| `/yolo`, `/ask` | permission mode |
| `/reasoning` | show or hide thinking blocks |
| `/quit`, `/exit`, `/q` | leave; prints the `ah -r ...` command that resumes |

## Built-in tools

| Tool | Arguments |
|---|---|
| `bash` | `command`; runs in the working directory with `tools.shell`, combined output plus exit code, `tools.bash_timeout_ms` limit, `permissions.deny` applies |
| `read_file` | `path`, optional `offset` and `limit`; numbered lines |
| `write_file` | `path`, `content`; creates parent directories |
| `edit_file` | `path`, `old_string`, `new_string`, optional `replace_all`; `old_string` must match exactly once unless `replace_all` |

`tools.enabled` and `tools.disabled` choose which are offered.

## JSONL events (`--json`)

One JSON object per line on stdout. `type` is one of:

| type | Fields |
|---|---|
| `request_start` | `turn` |
| `text` | `text` (streamed delta) |
| `reasoning` | `text` |
| `assistant` | `message` (complete assistant message) |
| `usage` | `usage` (`prompt_tokens`, `completion_tokens`, `cost`, ...) |
| `tool_start` | `call` |
| `tool_end` | `call`, `result`, `duration_ms` |
| `tool_denied` | `call`, `reason` |
| `tool_message` | `message` (the tool result as sent to the model) |
| `notice` | `text` |
| `settings_patch` | `patch` (from a plugin) |
| `retry` | `attempt`, `wait_ms`, `error` |
| `compacted` | `before`, `after`, `summary` |
| `error` | `error` |
| `turn_end` | `requests`, `tool_calls`, `usage`, `cancelled` |

Messages and calls use the OpenAI chat shape: `{"role": ..., "content":
..., "tool_calls": [...], "tool_call_id": ..., "reasoning": ..., "images":
[...]}`.
