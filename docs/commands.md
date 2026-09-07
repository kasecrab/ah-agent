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
| `/plan` | show the task list |
| `/plugins` | active plugins |
| `/tools` | tools offered to the model |
| `/keys` | active key bindings |
| `/yolo`, `/ask` | permission mode |
| `/reasoning` | show or hide thinking blocks |
| `/quit`, `/exit`, `/q` | leave; prints the `ah -r ...` command that resumes |

## Built-in tools

| Tool | Arguments |
|---|---|
| `bash` | `command`, optional `timeout_ms` and `background`; runs in the working directory with `tools.shell`, combined output plus exit code, `tools.bash_timeout_ms` limit, `permissions.deny` applies |
| `read_file` | `path`, optional `offset` and `limit`; numbered lines |
| `write_file` | `path`, `content`; creates parent directories |
| `edit_file` | `path` and either `old_string`/`new_string`/`replace_all` or `edits` (a list of those, applied in order); each `old_string` must match one place unless `replace_all`; nothing is written unless every edit applies |
| `jobs` | `action` (`list`, `output`, `wait`, `kill`), `id`, optional `from_line`, `tail`, `timeout_ms`; background commands |
| `plan` | `action` (`set`, `add`, `start`, `done`, `drop`, `update`, `list`), `tasks`, `ids`, `id`, `title`, `parent`, `needs`, `note`; the task list |

`tools.enabled` and `tools.disabled` choose which are offered.

### Matching in edit_file

`old_string` is looked for exactly first. If that finds nothing, the same text
is tried ignoring trailing whitespace and carriage returns, then ignoring how
far the block is indented (the replacement is re-indented to the file), then
ignoring how much whitespace sits between words. Each step is only accepted
when it finds a single place, so leniency never edits the wrong one, and the
result says which step was needed.

When several places match, the error lists their line numbers. When nothing
matches, the error points at the closest lines in the file and the first line
that differs, which is usually enough to fix the call without reading the file
again.

## Background jobs

A command that should keep running — a dev server, a long build, a big
download — is started with `bash` and `background: true`. It returns a job id
straight away. A foreground command that outruns `tools.bash_timeout_ms` is not
killed either: it becomes a background job and the model is told its id, so a
slow download costs one timeout instead of the whole command.

The model looks after a job with the `jobs` tool: `list` shows every job with
its state, `output` reads it (`tail` for the last lines, `from_line` with the
`next_line` from the previous read for only what is new), `wait` blocks until
the job ends or `timeout_ms` passes, and `kill` stops it. Waiting is cheaper
than polling: it costs no request until something actually happens.

When a job ends, the next request carries one `[background] job 2 exited 0
after 12.4s · 340 lines` line, so the model finds out without asking. If
nothing is running at that moment, ah starts a turn of its own so the model
reads the output and says what happened instead of leaving the news on the
screen; `tools.job_wake = false` turns that off and the model waits for your
next message.

In the TUI, a green `2 Bash` chip on the row above the input counts the running
jobs and disappears when the last one ends. `Down` on an empty input opens the job
list; `Enter` on a job follows its output live, `k` stops the job, `Esc`
closes the view. Jobs are children of the ah process: leaving ah stops them.

## The plan

Work that takes several steps gets a task list. The model writes it with
`plan` and `action: "set"`, giving one object per task:

```json
{"action": "set", "tasks": [
  {"title": "parse the config file"},
  {"title": "read the [tools] table", "parent": 1},
  {"title": "tests for the parser", "needs": [1]}
]}
```

Ids are handed out in the order the tasks are listed, so `parent` and `needs`
of 1 mean the first task in the call. `parent` makes a task a subtask;
`needs` lists the tasks that must finish before this one may start. `add`
appends tasks without disturbing the ids already given out.

`start`, `done` and `drop` take `ids` (or a single `id`) and an optional
`note`, which is kept beside the task as its outcome or blocker. `update`
changes one task's `title`, `parent`, `needs` or `note`. `list` shows the plan
without changing it.

Two moves are refused, because they are the ones that quietly put a plan out
of order: starting a task whose dependencies are unfinished, and finishing a
task whose subtasks are still open. Dropping a task drops its subtasks, and a
dropped task no longer blocks whatever waited for it. Every call returns the
whole plan:

```
plan · 1/3 done · doing: 2 read the [tools] table
  1 [ ] parse the config file
  2   [>] read the [tools] table
  3 [ ] tests for the parser · waits for 1
```

The plan is stored with the session, so `ah -r` resumes it. In the TUI, the
summary sits at the right of the row above the input while tasks are open, and
each update prints the task list in the transcript in place of the one before
it (`Alt-P` hides the summary,
`layout.show_plan` sets the default), `/plan` opens the whole list, and
`{plan}` is a statusline placeholder. `context.plan_reminder` controls the
reminder sent when the model leaves an unfinished plan alone.

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
