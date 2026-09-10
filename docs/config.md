# Configuration

All settings form one tree. Defaults are built in; layers on top are merged
as JSON merge patches (RFC 7386): objects merge key by key, anything else
replaces, and a removed key is spelled `"__unset__"` in TOML because TOML has
no null.

## Layers, lowest first

1. built-in defaults (this page)
2. `~/.config/ah/config.toml`
3. `~/.config/ah/favorites.toml` (written by `/favorite`, merged under `model.favorites`)
4. `./.ah/config.toml` in the working directory
5. `settings_patch` from each plugin manifest, then patches returned by plugin hooks
6. command line: `--model`, `--yolo`, `--ask`, `--max-tokens`, `--system`, `--set key=value`
7. runtime: `/set`, `/yolo`, `/ask`, `/effort`, `/model`, `/reasoning` (not saved)

`ah config show` prints the merged result as TOML; `--origins` also lists the
layers. `ah config path` prints file locations. `ah config init` writes a
commented file with every default to the user config dir (`--force`
overwrites). Inside the TUI `/config` shows the same paths and layer list, and
`/reload` re-reads the files and reloads plugins.

## Setting a key without editing files

```
ah --set theme.accent=magenta --set layout.input_height=5
/set theme.accent magenta
/set keys.submit ["enter","ctrl-s"]
```

Keys are dotted paths. The value is parsed as JSON first (`5`, `true`,
`["a","b"]`, `{"effort":"high"}`), otherwise taken as a string.

## Unknown keys

Top-level tables the harness does not know are kept and exposed to plugins
through `settings_get`, so a plugin can read its own section (`[guard]
deny = [...]`).

## [model]

| Key | Default | Meaning |
|---|---|---|
| `id` | `"deepseek/deepseek-v4-flash-0731"` | OpenRouter model id |
| `max_tokens` | `8192` | completion limit per request |
| `temperature` | unset | sampling temperature |
| `top_p` | unset | nucleus sampling |
| `reasoning` | unset | reasoning parameters, e.g. `{ effort = "high" }`; `off` disables. `/effort` sets it |
| `provider` | unset | OpenRouter provider routing object, passed through as is |
| `base_url` | `"https://openrouter.ai/api/v1"` | any OpenAI-compatible endpoint |
| `api_key` | unset | key override; prefer `OPENROUTER_API_KEY` or `ah login` |
| `favorites` | `{}` | named models cycled with `keys.cycle_model` and managed by `/favorite` |

Favorites are either a bare id or a table with an effort:

```toml
[model.favorites]
fast = "deepseek/deepseek-v4-flash-0731"
smart = { id = "anthropic/claude-sonnet-4.5", effort = "high" }
```

## [prompt]

| Key | Default | Meaning |
|---|---|---|
| `system` | the built-in prompt | full system prompt; `{cwd}`, `{os}`, `{shell}`, `{date}` are substituted |
| `append` | `""` | appended after `system`; `--system TEXT` fills it |
| `instructions` | `["AGENTS.md", "CLAUDE.md"]` | instruction file names, first found per directory wins (`ah docs instructions`) |
| `docs_hint` | `true` | tell the model that `ah docs` exists, so it can look up ah itself when asked |

Default `system`:

```
You are ah, a fast coding agent running in a terminal. Working directory: {cwd}. OS: {os}. Shell: {shell}. Date: {date}.
Use the provided tools to inspect and change files and run commands. Prefer reading before editing. Keep replies short; the user sees your text in a terminal. When a task is done, summarise what changed.
Work that takes more than a couple of steps goes in the plan tool first: set the tasks, mark one started before you work on it and done as soon as it is finished, and give a task `needs` when it cannot start until another one is done. Skip it for a single edit or question.
When the request leaves a choice open that would send the work one way or the other, ask with the ask_user tool before doing it, once, with the options you would otherwise pick between. Anything you can settle from the code or a sensible default, settle yourself and say what you assumed.
```

`window_title` names the terminal window after the work, so a row of ah
windows can be told apart. `{task}` is the session name when it has one
(`/rename`), otherwise the first message of the session, otherwise the
working directory. The previous title is put back when ah exits.

## [theme]

Colours accept names (`red`, `bright_blue`, `dark_gray`, `gray`, `white`,
`reset`), `#rrggbb`, or an ANSI index (`"235"`). `reset` means the terminal's
own colour. The `syn_*` keys may add `dim`, `bold`, `italic` or `underline`
after the colour (`"cyan dim"`). The highlighting defaults follow the ANSI
theme Claude Code uses, so both look the same in the same terminal.

| Key | Default | Used for |
|---|---|---|
| `fg` | `"reset"` | transcript text |
| `bg` | `"reset"` | background |
| `accent` | `"cyan"` | selections, titles, the model line |
| `user` | `"green"` | user messages |
| `assistant` | `"reset"` | assistant messages |
| `reasoning` | `"dark_gray"` | thinking block |
| `tool` | `"yellow"` | tool call headers |
| `tool_output` | `"dark_gray"` | tool output |
| `error` | `"red"` | errors |
| `dim` | `"dark_gray"` | notices, hints |
| `border` | `"gray"` | input box border |
| `border_focus` | `"gray"` | border while focused |
| `status_fg` | `"gray"` | status bar text, when the row is one colour |
| `status_bg` | `"reset"` | status bar background, when the row is one colour |
| `input_fg` | `"reset"` | input text |
| `input_bg` | `"reset"` | input background |
| `selection` | `"blue"` | mouse selection |
| `heading` | `"white"` | markdown headings |
| `link` | `"blue"` | markdown links |
| `quote` | `"dark_gray"` | block quotes |
| `code` | `"reset"` | inline and fenced code |
| `code_bg` | `"reset"` | code background; set one (`"235"`) to get a padded box |
| `rule` | `"dark_gray"` | horizontal rules, table borders |
| `syn_keyword` | `"blue"` | keywords, literals, class names |
| `syn_string` | `"red"` | strings |
| `syn_comment` | `"green"` | comments |
| `syn_number` | `"green"` | numbers |
| `syn_type` | `"cyan dim"` | type names (`i32`, `int`, `String`) |
| `syn_function` | `"yellow"` | function names before `(` |
| `syn_builtin` | `"cyan"` | built-in functions and primitive types (`str`, `string`, `print`) |
| `syn_attr` | `"cyan"` | keys in JSON, TOML and YAML |
| `job` | `"green"` | background of the running-jobs chip on the row above the input |
| `agent` | `"magenta"` | background of the running-agents chip beside it |
| `diff_add` | `"green"` | added lines in file diffs |
| `diff_del` | `"red"` | removed lines |
| `border_style` | `"lines"` | `none`, `lines` (rules above and below the input), `plain`, `rounded`, `double`, `thick` |
| `user_prefix` | `"> "` | glyph before user messages |
| `assistant_prefix` | `""` | glyph before assistant messages |
| `tool_prefix` | `"⚙ "` | glyph before tool calls |
| `input_prefix` | `"› "` | prompt glyph in the input line |

## [layout]

| Key | Default | Meaning |
|---|---|---|
| `input_height` | `1` | input rows; grows with content |
| `input_max_height` | `10` | growth limit |
| `transcript_max_width` | `0` | wrap width; 0 = full width |
| `show_status` | `true` | status bar |
| `show_tool_output` | `false` | tool output expanded by default (Ctrl-T toggles) |
| `tool_output_lines` | `20` | lines shown when expanded |
| `show_reasoning` | `true` | thinking block expanded (Ctrl-R toggles) |
| `wrap` | `true` | soft-wrap long lines |
| `stream_redraw_ms` | `33` | redraw throttle while streaming; 0 = every delta |
| `animation_ms` | `32` | redraw period for the working line; `0` leaves it still |
| `scroll_step` | `3` | rows per wheel or arrow step |
| `mouse` | `true` | capture the mouse: wheel scrolls, drag selects and copies via OSC 52; `false` leaves it to the terminal |
| `kitty_keyboard` | `true` | push kitty keyboard flags (needed for Shift-Enter) without querying the terminal |
| `paste_collapse_lines` | `3` | pastes longer than this become a `[Pasted #1: N lines]` chip |
| `queue_max` | `5` | messages that can wait while a turn runs; 0 disables queueing |
| `show_modalities` | `true` | `TI→T` modality tags in `/model`, `/favorite`, `/usage` and the status bar |
| `image_paste_cmd` | `""` | shell command printing the clipboard image as PNG; empty tries `wl-paste`, `xclip`, `pngpaste` |
| `markdown` | `true` | render assistant text as markdown |
| `code_highlight` | `true` | highlight fenced code |
| `show_plan` | `true` | plan summary on the row above the input while tasks are open |
| `window_title` | `"{task} · ah"` | terminal window title; `{task}`, `{cwd}`, `{model}`, `{session}`; empty leaves the title alone |

## [keys]

Every action is a list of key strings; see `ah docs keys` for the syntax and
the full default table.

## [voice]

Dictation. `/voice` arms it, the `talk` key (Space) is held to listen, and the
words land in the input box in grey and turn white when the phrase is done.
Nothing is ever sent on its own; Enter still sends.

Transcribing is a normal OpenRouter request against a model that takes audio
input, so there is no second key and no local model — and the audio does leave
the machine. `/voice model` lists the models that qualify with their audio
price.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | offer `/voice` at all; off never opens the microphone |
| `provider` | `""` | `deepgram` (own key, live socket, words while you speak) or `openrouter` (the key `ah` already has); empty asks on the first `/voice` |
| `model` | `""` | transcribing model; empty asks on first use. `nova-3` and friends on Deepgram, an audio-input model id on OpenRouter |
| `idle_secs` | `0` | seconds an unused Deepgram socket is held before it is dropped and reopened on demand; 0 holds it for as long as dictation is armed. An open socket sending no audio is not billed |
| `mode` | `"balanced"` | `fast`, `balanced` or `cheap`: one choice for the three dials below |
| `prompt_append` | `""` | words the model would get wrong: project names, identifiers, people |
| `language` | `""` | spoken language; empty lets the model decide |
| `hotkey_mode` | `"auto"` | `auto`, `push_to_talk` or `toggle`; `auto` uses what the terminal reports |
| `release_grace_ms` | `700` | silence that counts as letting the talk key go, before the keyboard's repeat rate has been seen; after that a much shorter gap is used |
| `max_listen_secs` | `300` | longest a toggled microphone stays on; holding a key needs no limit |
| `device` | `""` | input device name; empty takes the system default |
| `capture_cmd` | `""` | command printing raw s16le mono PCM on stdout; user config or environment only |
| `keep_open` | `false` | hold the microphone open for as long as dictation is armed, rather than opening it for each hold |
| `sample_rate` | `0` | capture rate; 0 asks for 16000 and resamples what the device gives |
| `ring_ms` | `2000` | audio kept in the capture buffer |
| `phrase_ms` | `400` | silence that ends a phrase |
| `max_chunk_ms` | `3500` | longest phrase before it is cut at the quietest moment near the end |
| `max_inflight` | `2` | phrases transcribed at the same time |
| `preroll_ms` | `300` | audio kept from before speech was detected |
| `speech_ratio` | `3.0` | how far above the noise floor counts as speech |
| `filter` | `true` | drop stock phrases and repeat loops models invent over silence |
| `budget_usd` | `0.0` | stop a dictation once it has cost this much; 0 does not watch |
| `show_cost` | `false` | show what the dictation has cost next to the timer |
| `meter` | `false` | draw the input level; the only part that redraws on a clock |

`mode` sets `phrase_ms`, `max_chunk_ms` and `max_inflight` together: `fast` is
`300 / 2000 / 3`, `balanced` `400 / 3500 / 2`, `cheap` `700 / 8000 / 1`. A dial
still sitting at its own default follows `mode`; one moved away from it keeps
the value it was given. Shorter phrases reach the screen sooner and cost more,
because each one is its own request.

`capture_cmd` is a shell command, so it is read from `~/.config/ah/config.toml`
or `AH_VOICE_CAPTURE_CMD` only. A project's `.ah/config.toml` cannot set it.

## [tools]

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `["ask_user", "bash", "read_file", "write_file", "edit_file", "jobs", "plan", "agent", "agents"]` | built-in tools offered to the model |
| `disabled` | `[]` | tools removed, including plugin tools by name |
| `max_output_bytes` | `32768` | larger tool output is truncated head and tail |
| `bash_timeout_ms` | `120000` | `bash` tool time limit |
| `shell` | `""` | shell binary for `bash`; empty = `$SHELL` or `sh` |
| `read_default_limit` | `2000` | lines `read_file` returns without an explicit limit |
| `parallel` | `true` | run consecutive read-only calls from one model message at the same time |
| `max_parallel` | `8` | most calls in flight at once |
| `parallel_bash` | `["ls", "cat", "rg", "git log", …]` | shell commands treated as read-only |
| `background_on_timeout` | `true` | a foreground command that outruns its timeout keeps running as a background job |
| `job_buffer_bytes` | `262144` | output kept per background job: the first third, then the newest lines |
| `job_kill_grace_ms` | `2000` | time between the polite stop and the hard kill |
| `job_wait_ms` | `60000` | default limit for a `jobs` wait |
| `job_wake` | `true` | when a job ends while the model is idle, start a turn so it can read the output and report |

### Parallel tool calls

Models ask for several tools in one message: read these four files, grep for
that symbol in two places. ah runs consecutive read-only calls together on
scoped threads and stitches the results back into call order, so the model
sees exactly what it would have seen one at a time.

A call joins a batch only when it reads: `read_file` always, `bash` when every
segment of the command matches `parallel_bash` and the command has no
redirection or command substitution. Everything else — `write_file`,
`edit_file`, any other shell command, every plugin tool — runs alone, in the
order the model asked for it, so a write never races a read of the same file.
A batch stops at the first call that writes and starts again after it.

Threads are only spawned for a batch of two or more, permission prompts and
plugin hooks still run one at a time on the main thread, and `max_parallel`
caps how many run together. Set `parallel = false` to go back to one at a time.

## [plugins]

| Key | Default | Meaning |
|---|---|---|
| `paths` | `[]` | extra `.wasm` files or directories, besides `~/.config/ah/plugins` and `.ah/plugins` |
| `disabled` | `[]` | plugin names or file stems to skip |
| `fuel_per_call` | `50000000` | interpreter fuel per hook call, roughly one unit per wasm instruction |
| `max_memory_bytes` | `67108864` | linear memory cap per plugin |
| `enabled` | `true` | `false` loads nothing (`--no-plugins` does the same) |

## [statusline]

| Key | Default | Meaning |
|---|---|---|
| `items` | `["favorite", "model", "effort", "modalities", "context", "tokens", "cost", "cwd", "git"]` | what the row shows, left to right, joined with ` · ` |
| `colors` | `true` | give each item a colour of its own instead of one flat row |
| `format` | `""` | a template that replaces `items` when set, drawn in one colour |

Items: `favorite`, `model`, `effort`, `modalities`, `context` (`42%`, or
`12k ctx` when the window is unknown), `tokens`, `cost`, `plan` (`2/7` while a
plan is unfinished), `cwd`, `git`, `plugins`, `state`, `session`. `tokens`,
`cost` and `plugins` stay out of the way while they are zero. `/statusline`
ticks them off a list for the session; this key keeps a choice for good.

`format` is the way out for a line the items cannot express. Placeholders:
`{model}`, `{favorite}`, `{effort}`, `{modalities}`, `{tokens_in}`,
`{tokens_out}`, `{cost}`, `{context}`, `{cwd}`, `{git}`, `{plan}`,
`{plugins}`, `{state}`, `{session}`.

`status_fg` and `status_bg` in `[theme]` paint the whole row in one colour,
which is only what `colors = false` wants; with `colors = true` each item
brings its own colour and the row keeps the terminal's background, so a theme's
bar colour cannot swallow them.

A plugin with the `statusline` hook receives the rendered text and can replace
it, either with one string drawn in the bar's colour or with coloured pieces of
its own (`ah docs plugins`).

## [images]

Pictures a model draws. Ask an image-capable model for one in plain language —
there is no separate command, and `ah models` marks them `T→TI`. Generated
images are written to disk; the conversation stores the path, never the bytes.

Two kinds of model draw. A chat model that also makes pictures
(`google/gemini-3-pro-image`, `openai/gpt-5-image`) answers with words and
images together, and `output` decides whether it is asked for them. A model
that *only* draws (`meta/muse-image`, `black-forest-labs/flux.2-pro` — the
`image` category in `/model`) is not on the chat endpoint at all: ah sends the
last thing you asked for, plus every picture already in the conversation as
something to work from, so "now make it night" edits what is there. Such a
model has no tools and cannot summarise, so compaction is off while one is
selected.

Inline drawing needs a terminal that speaks the kitty graphics protocol (kitty,
Ghostty, WezTerm, Warp) or the iTerm2 one. Anywhere else — tmux, screen, VS
Code, Alacritty — a picture shows as a one-line chip and Ctrl-O opens it.

ah asks the images endpoint for PNG, because the kitty protocol carries PNG
and raw pixels and nothing else. A provider that answers with a JPEG or a WebP
anyway is still saved and still measured, and shows as a chip in a kitty
terminal; iTerm2 hands those to the system decoder and draws them.

| Key | Default | Meaning |
|---|---|---|
| `output` | `"auto"` | `auto` asks only chat models the catalogue says draw; `always` asks anyway; `off` never asks. A model that only draws ignores this |
| `dir` | `""` | where images land; empty = `<data dir>/images/<session>/` |
| `history` | `1` | recent generated images resent with the next request; 0 never resends one |
| `echo` | `"assistant"` | shape of a resent image: `assistant`, or `user` for a provider that refuses assistant images |
| `inline` | `"auto"` | `auto` detects kitty/iTerm2; force `kitty`, `iterm2`, or `off` for chips only |
| `max_rows` | `20` | tallest inline picture, in rows; also capped at two thirds of the window |
| `max_cols` | `0` | widest inline picture, in columns; 0 = the transcript width |
| `cell_px` | `""` | cell size as `"9x18"` for terminals that will not report one; empty asks the terminal |
| `open_cmd` | `""` | shell command that opens a saved image; `{path}` is substituted, else appended; empty tries `xdg-open`, `open`, `start` |

`history` is the one knob that costs money. Providers cache a prompt by its
exact prefix, so when an image ages out of the window the bytes at that
position change once — from a picture to a line of text naming its file — and
the rest of that one request is priced as a miss. At `1` the change is a
message or two from the end; at `0` an image never enters a prompt at all and
the prefix is never disturbed. The placeholder text never changes afterwards,
so this is paid once per image, not once per turn.

## [permissions]

| Key | Default | Meaning |
|---|---|---|
| `mode` | `"auto"` | `auto` runs every tool call; `ask` prompts for tools in `ask_for` |
| `ask_for` | `["bash", "write_file", "edit_file"]` | tools that prompt in `ask` mode |
| `deny` | built-in list | shell commands refused in every mode |

`deny` rules apply to the `bash` tool only. The command is split into
segments on `;`, `|`, `&`, `&&`, `||` and newlines; a leading `sudo`, `env`,
`nohup`, `time` or `VAR=value` is skipped. A rule matches a segment that
equals it or starts with it followed by a space; a trailing `*` matches any
continuation. Setting `deny` replaces the built-in list, so copy the entries
you want to keep. The defaults:

```
rm -rf /   rm -rf ~   rm -rf ~/   rm -rf .   rm -rf ..   rm -fr /   rm -fr ~
rm -rf --no-preserve-root*   rm -rf $HOME   rm -rf $HOME/
git reset --hard   git push --force   git push -f
git clean -f*   git clean -x*   git clean -d*
git checkout -- .   git checkout .   git restore .
git branch -D   git stash drop   git stash clear   git filter-branch*
chmod -R 777 /   mkfs*   dd if=*   shutdown*   reboot   poweroff   :(){ :|:& };:
```

A denied call returns an error to the model asking it to let the user run
the command by hand. Plugins with the `before_tool` hook can deny, replace or
ask for any tool call on top of this.

## [context]

| Key | Default | Meaning |
|---|---|---|
| `auto_compact` | `true` | summarise the conversation when it fills `compact_at` percent of the window |
| `compact_at` | `90` | trigger percentage |
| `window` | `0` | context window in tokens; 0 = from the model catalogue |
| `summary_max_tokens` | `4096` | limit for the summary request |
| `cache` | `true` | ask the provider to cache the prompt prefix on models that need an explicit breakpoint |
| `cache_models` | `["anthropic/", "qwen/"]` | model id prefixes that need one; every other model caches on its own |
| `cache_ttl` | `"5m"` | how long the provider holds the cache: `5m` or `1h` |
| `cache_min_tokens` | `2048` | skip the cache below this estimated prompt size |
| `plan_reminder` | `true` | remind the model of a plan it has stopped updating |
| `plan_reminder_every` | `4` | requests without a plan change before the reminder is sent again |

See `ah docs sessions` for what compaction does to the transcript and the
session file.

## [agents]

Subagents: child agent loops the model starts with the `agent` tool, each with
its own conversation, model and tools. See `ah docs commands` for the tools
themselves.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | offer the `agent` and `agents` tools |
| `max_concurrent` | `4` | children running at once; the rest queue |
| `max_total` | `32` | children a session may start |
| `max_depth` | `1` | 1 means children cannot start children of their own |
| `max_spawn` | `8` | tasks accepted in one `agent` call |
| `max_requests` | `40` | requests a child may make before it has to report |
| `max_context_bytes` | `262144` | conversation a child is allowed; children never compact |
| `timeout_ms` | `600000` | how long `agent` waits before leaving the children in the background |
| `kill_grace_ms` | `2000` | time a child gets to stop politely |
| `tools` | `["read_file", "bash", "jobs"]` | tools a child gets when its type names none |
| `model` | `""` | model for children whose type names none; empty inherits the session's |
| `effort` | `""` | reasoning effort for those children; empty inherits |
| `report_bytes` | `8192` | longest report a child hands back; the middle is dropped |
| `log_lines` | `200` | tool lines kept per child for the agent view |
| `keep` | `16` | finished children kept, with their conversation, for follow-ups |
| `wake` | `true` | when a background child ends while the model is idle, start a turn so it can read the report |
| `allow_model_arg` | `false` | let the model name a model per task instead of taking the type's |
| `stack_bytes` | `0` | thread stack per child; 0 uses the system default |
| `defs` | `{}` | the agent types, by name |

### Agent types

An agent type is a `[agents.defs.<name>]` table. The model reads the
`description` of every type and picks between them, so write it for the model.

```toml
[agents.defs.explorer]
description  = "Finds where something lives and reports paths with line numbers."
prompt       = "You are a code explorer. Report file:line, nothing else."
tools        = ["read_file", "bash"]
model        = "fast"        # a favorite name or a model id; empty inherits
effort       = "low"
max_requests = 20
timeout_ms   = 300000

[agents.defs.worker]
description = "Implements a bounded change and reports the files it touched."
tools       = ["read_file", "bash", "write_file", "edit_file", "jobs"]
```

`prompt` replaces the system prompt for that child; `settings` is a merge patch
applied over everything else, for anything without its own key.

A child gets only the tools its type lists, so the default types cannot change
a file: writing is something you grant in this file, not something the model
decides at run time. `permissions.deny` always applies to children too, and
`plan` and `ask_user` are always removed — the plan belongs to the session, and
a child has nobody to ask. Children also run without plugins: a plugin that
gates tools does not see a child's calls, which is the other reason the default
type list is read-only.

### Prompt caching

Most of what ah sends is the same on every request of a turn: the system
prompt, the tool declarations and the conversation so far. Providers can keep
that prefix in a cache and charge about a tenth of the input price for it.

OpenAI, Grok, Groq, DeepSeek, Z.AI and Gemini 2.5 do this on their own, so ah
sends nothing extra for them. Anthropic and Qwen need an explicit cache
breakpoint, and ah adds one as a top-level `cache_control` on the request: the
provider keeps the breakpoint at the end of the prompt and advances it as the
conversation grows, so every request after the first reads the whole prefix
from the cache.

A cache only helps if the next request reaches the machine holding it, so every
request carries the session id as OpenRouter's `session_id` and requests with
the same one are routed to the same provider endpoint. Without it the routing
key is guessed from the opening messages, and a turn that lands elsewhere pays
full input price for a prefix that was already cached. This matters just as
much for the providers that cache on their own as for the ones that need a
breakpoint.

The price of a cache write is 1.25x the input price (2x with `cache_ttl =
"1h"`), and a read is 0.1x. One re-read pays for the write several times over,
which a tool call already guarantees, but a prompt that is never re-sent would
cost 25 percent more. That is what `cache_min_tokens` is for: below it, and
below the provider's own minimum, nothing is cached. Set `cache = false` to
turn the whole thing off.

`/usage` shows `cached` tokens and what the cache saved, as reported by
OpenRouter.

### Plan reminders

The `plan` tool holds the task list for work that takes several steps, and
every call to it comes back with at least the summary, so the model normally
sees where the work has got to without help. When it stops calling the tool
while tasks are still open, ah appends one line — the plan summary — after
`plan_reminder_every` requests without a change. The line goes at the end of
the prompt, where it cannot disturb a cached prefix, and it belongs to that
one request: it is not kept in the conversation, so no later turn pays for it
or reads a count the plan has since moved past. It stops once every task is
done or dropped. See `ah docs commands` for the tool itself.
