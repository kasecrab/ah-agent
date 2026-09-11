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
| `ah models [FILTER] [--tools] [--modality KIND] [--refresh]` | list the cached OpenRouter catalogue, fuzzy filtered; `--tools` keeps tool-calling models; `--modality` keeps one kind of output (`text`, `image`, `video`, `speech`, `transcription`, `embeddings`, `rerank`); `--refresh` refetches |
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
| `ah remote pair [--url URL]` | make a pairing and show it as a QR code and a typed code. The URL is remembered, so it is needed once. Anyone holding the code can start a session on this machine |
| `ah remote serve [--detach]` | publish this machine's sessions with no window open, so a phone can start and resume them. `--detach` carries on in the background and writes to `<data dir>/remote.log` |
| `ah remote status` | whether this machine is paired, and to which relay |
| `ah remote forget` | forget the pairing, so no phone holding it can reach this machine |
| `ah docs [TOPIC]` | list these pages, or print one |

## Slash commands (TUI)

Type `/` in an empty input for a popup filtered as you type; Tab fills the
name in, Enter runs it. Plugins can add commands (`ah docs plugins`).

| Command | Effect |
|---|---|
| `/help`, `/?` | list commands and keys |
| `/model [CATEGORY] [ID\|refresh]` | fuzzy picker over the catalogue, in categories walked with ← →; a known id or favorite name switches directly; `refresh` refetches |
| `/effort [LEVEL]` | reasoning effort: `off`, `minimal`, `low`, `medium`, `high`, `xhigh` |
| `/favorite [NAME]`, `/fav` | favorites picker; a name switches to it or creates it |
| `/usage` | session cost per model, wall and API time, tool calls, lines changed, plus OpenRouter balance, limits and 30-day top models (`r` refreshes) |
| `/voice` | arm dictation: hold the talk key and speak, the words land in the input box in grey and turn white when the phrase is done. `/voice off` disarms, `/voice model [id]` picks the transcribing model from the models that take audio input, `/voice devices` picks the microphone. Nothing is sent until you press Enter |
| `/compact [FOCUS]` | summarise the conversation now, optionally around a focus; the summary folds behind a one-line header (Ctrl-T shows it) |
| `/clear` | start an empty conversation in the same session file |
| `/resume [ID\|NAME]` | switch to another session in place |
| `/rename [NAME]` | name the session; empty removes the name |
| `/session` | session id and file |
| `/skills [NAME] [ARGS]`, `/skill` | run a saved prompt (`ah docs skills`) |
| `/images` | pictures the model drew in this conversation; Enter opens one |
| `/init` | ask the model to write or refresh `AGENTS.md` (`ah docs instructions`) |
| `/set KEY VALUE` | override a setting for this run |
| `/config` | config paths and active layers |
| `/statusline`, `/status` | tick what the status line shows; Space toggles a row, Esc closes; lasts the session, `statusline.items` in config keeps it |
| `/reload` | re-read config files and reload plugins |
| `/plan` | show the task list |
| `/jobs` | the background shell commands; Enter follows one, `k` stops it |
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
| `ask_user` | `questions`: up to four, each with a `question`, an optional `header`, up to eight `options` (`label`, `description`) and `multi`; puts them to the user one at a time and returns what they said |
| `agent` | `tasks`: one brief per agent, each with `task` and optionally `agent` (the type), `cwd` and `model`; plus `background` and `timeout_ms`. Starts agents that run on their own and hands back what each reports |
| `agents` | `action` (`list`, `status`, `wait`, `kill`, `say`), `id`, `ids`, `all`, `timeout_ms`, `text`; looks after the agents this one started |

`tools.enabled` and `tools.disabled` choose which are offered.

### Asking the user

`ask_user` stops the turn and puts a box on screen: the question, its options
numbered, and a row for an answer of the user's own as the last of them. A
number picks, `space` picks the highlighted row, and typing anywhere starts a
written answer, which can stand alone or qualify a pick. Left and Right walk
several questions, each keeping its own answer; Enter sends, and while any
question is unanswered it goes to the first one and names it in the footer
instead. Sending and leaving both take a second key — Enter confirms, Esc goes
back — since neither can be undone once the model has the reply. The model gets
the questions and the answers back as text; a dismissed question and a run with
nobody at the keyboard both come back as errors telling it to decide for
itself. One-shot runs ask on the terminal instead; `--json` and piped runs
never ask.

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

## Subagents

Work that is read-heavy — map an unfamiliar codebase, chase a symbol through
fifty files, read a build log — costs the conversation more than it is worth:
everything that had to be read stays in the window afterwards. The model hands
that kind of job to an agent instead. An agent is a whole loop of its own, with
its own conversation, model and tools, running on its own thread. It reads what
it has to read and hands back one report; nothing else it did reaches the
conversation.

The model starts them with `agent`, one call for as many briefs as it wants to
run at once:

```json
{"tasks": [
  {"agent": "explorer", "task": "Find where SSE frames are parsed. Report file:line."},
  {"agent": "explorer", "task": "Find where plugins are instantiated. Report file:line."}
]}
```

An agent starts fresh and sees only its brief, so the brief has to stand on its
own. It cannot ask anybody anything: `ask_user` is not among its tools, and a
tool call that would need the user's approval is refused rather than put on
screen. What an agent may do is settled in `config.toml`, by the type it is
started as — `ah docs config` covers `[agents]` and the types.

By default `agent` waits and returns each report. With `background: true` it
returns the ids at once, and a wait that runs out leaves the agents running
rather than stopping them — the same bargain `bash` makes with a slow command.
Either way the model hears `[agent] agent 3 (explorer) done …` on its next
request, and if nothing is running at that moment ah starts a turn so it can
read the report and say what happened (`agents.wake = false` turns that off).

The `agents` tool looks after them: `list`, `status` for how far one has got,
`wait`, `kill`, and `say` to give a running agent something more to go on.
Waiting costs nothing until something happens.

In the TUI everything running sits in a strip under the status bar, one row
each: the background jobs when there are any, then `main`, then one row per
agent with what it is doing, how long it has been at it and what it has
written. `●` marks the one the input is bound to. The strip is only there while
something is running, and an agent's row goes as soon as it reports.

With an empty input, Down moves the keys from the input into the strip and
along it, Up moves back out of the top, Enter binds the input and the
transcript to the highlighted row, Ctrl-K stops the agent it is on, and Esc
leaves the strip alone. An arrow marks the row the keys are on; `●` the one the
input belongs to.

Bound to an agent, the screen is that agent's conversation, drawn exactly like
this one: its brief as the first message, its replies as they arrive, its tool
calls with the same headers and the same Ctrl-T to open their output. The
status bar reports that agent — its model, its context, its spend — and the
prompt reads `a3 ›`. What you type goes to it and it reads the message before
its next step. Going back to `main` leaves the agent running; the conversation
is untouched, because stepping into an agent watches it rather than
interrupting anything. `agents.view_bytes` caps what an agent keeps of its own
work for this.

An agent's own background commands never appear in the conversation. They are
in that agent's view, with the rest of what it did.

Agents cost more tokens than doing the work in one conversation, not fewer.
What they buy is a conversation that stays about the work instead of filling
with what had to be read, several jobs running at once, and cheap models on the
jobs that do not need an expensive one.

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
task whose subtasks are still open — though naming a parent and its subtasks
in the same call is fine, since that finishes them together. Dropping a task
drops the work below it however deep it goes, and a dropped task no longer
blocks whatever waited for it.

`set` and `list` answer with the whole plan:

```
plan · 1/3 done · doing: 2 read the [tools] table
  1 [ ] parse the config file
  2   [>] read the [tools] table
  3 [ ] tests for the parser · waits for 1
```

The other actions answer with the summary and only the tasks they touched,
which keeps a plan worked through over twenty calls from being written out
twenty times:

```
done 2
plan · 2/3 done · next: 3 tests for the parser
  2   [x] read the [tools] table · 40 lines
```

A move that finds the task already where it was asked to go says so instead,
so a model that has lost track stops repeating itself:

```
no change: 2 is already done
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
| `image` | `path`, `mime`, `width`, `height`, `bytes` (a picture the model drew, already written) |
| `notice` | `text` |
| `settings_patch` | `patch` (from a plugin) |
| `retry` | `attempt`, `wait_ms`, `error` |
| `compacting` | `auto` (true when the window filled up, false for `/compact`) |
| `compact_progress` | `done`, `budget` (summary tokens written, of the budget allowed) |
| `compacted` | `before`, `after`, `summary` |
| `error` | `error` |
| `turn_end` | `requests`, `tool_calls`, `usage`, `cancelled` |

Messages and calls use the OpenAI chat shape: `{"role": ..., "content":
..., "tool_calls": [...], "tool_call_id": ..., "reasoning": ..., "images":
[...]}`. An entry in `images` is either a `data:` URL, for a picture attached
to the prompt, or an `ah-image:<session>/<file>` reference to one the model
drew — the bytes of a generated image are never in the stream, only its path,
which the `image` event gives in full. In one-shot mode that path is also
printed on stdout on its own line, so `ah -p "draw a cat" | xargs feh` works.
