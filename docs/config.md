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
```

## [theme]

Colours accept names (`red`, `bright_blue`, `dark_gray`, `gray`, `white`,
`reset`), `#rrggbb`, or an ANSI index (`"235"`). `reset` means the terminal's
own colour.

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
| `status_fg` | `"gray"` | status bar text |
| `status_bg` | `"reset"` | status bar background |
| `input_fg` | `"reset"` | input text |
| `input_bg` | `"reset"` | input background |
| `selection` | `"blue"` | mouse selection |
| `heading` | `"white"` | markdown headings |
| `link` | `"blue"` | markdown links |
| `quote` | `"dark_gray"` | block quotes |
| `code` | `"white"` | inline and fenced code |
| `code_bg` | `"235"` | code background |
| `rule` | `"dark_gray"` | horizontal rules, table borders |
| `syn_keyword` | `"magenta"` | code highlighting |
| `syn_string` | `"green"` | |
| `syn_comment` | `"dark_gray"` | |
| `syn_number` | `"yellow"` | |
| `syn_type` | `"cyan"` | |
| `syn_function` | `"blue"` | |
| `diff_add` | `"green"` | added lines in file diffs |
| `diff_del` | `"red"` | removed lines |
| `border_style` | `"lines"` | `none`, `lines` (rules above and below the input), `plain`, `rounded`, `double`, `thick` |
| `user_prefix` | `"> "` | glyph before user messages |
| `assistant_prefix` | `""` | glyph before assistant messages |
| `tool_prefix` | `"⚙ "` | glyph before tool calls |
| `input_prefix` | `"› "` | prompt glyph in the input line |
| `spinner` | braille frames | frames cycled every `layout.spinner_ms` |

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
| `spinner_ms` | `100` | spinner frame time |
| `scroll_step` | `3` | rows per wheel or arrow step |
| `mouse` | `true` | capture the mouse: wheel scrolls, drag selects and copies via OSC 52; `false` leaves it to the terminal |
| `kitty_keyboard` | `true` | push kitty keyboard flags (needed for Shift-Enter) without querying the terminal |
| `paste_collapse_lines` | `3` | pastes longer than this become a `[Pasted #1: N lines]` chip |
| `queue_max` | `5` | messages that can wait while a turn runs; 0 disables queueing |
| `show_modalities` | `true` | `TI→T` modality tags in `/model`, `/favorite`, `/usage` and the status bar |
| `image_paste_cmd` | `""` | shell command printing the clipboard image as PNG; empty tries `wl-paste`, `xclip`, `pngpaste` |
| `markdown` | `true` | render assistant text as markdown |
| `code_highlight` | `true` | highlight fenced code |

## [keys]

Every action is a list of key strings; see `ah docs keys` for the syntax and
the full default table.

## [tools]

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `["bash", "read_file", "write_file", "edit_file"]` | built-in tools offered to the model |
| `disabled` | `[]` | tools removed, including plugin tools by name |
| `max_output_bytes` | `32768` | larger tool output is truncated head and tail |
| `bash_timeout_ms` | `120000` | `bash` tool time limit |
| `shell` | `""` | shell binary for `bash`; empty = `$SHELL` or `sh` |
| `read_default_limit` | `2000` | lines `read_file` returns without an explicit limit |

## [plugins]

| Key | Default | Meaning |
|---|---|---|
| `paths` | `[]` | extra `.wasm` files or directories, besides `~/.config/ah/plugins` and `.ah/plugins` |
| `disabled` | `[]` | plugin names or file stems to skip |
| `fuel_per_call` | `50000000` | interpreter fuel per hook call, roughly one unit per wasm instruction |
| `max_memory_bytes` | `67108864` | linear memory cap per plugin |
| `enabled` | `true` | `false` loads nothing (`--no-plugins` does the same) |

## [statusline]

| Key | Default |
|---|---|
| `format` | `" {favorite} {model} {effort} {modalities} │ ↑{tokens_in} ↓{tokens_out} ${cost} │ {context} │ {cwd} {git}"` |

Placeholders: `{model}`, `{favorite}`, `{effort}`, `{modalities}`,
`{tokens_in}`, `{tokens_out}`, `{cost}`, `{context}` (`42%`, or `12k ctx`
when the window is unknown), `{cwd}`, `{git}`, `{plugins}`, `{state}`,
`{session}`. A plugin with the `statusline` hook receives the rendered text
and can replace it.

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

See `ah docs sessions` for what compaction does to the transcript and the
session file.
