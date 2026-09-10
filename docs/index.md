# ah documentation

ah is a minimal coding agent for the terminal: one static binary, OpenRouter
as the model provider, built-in tools (`bash`, `read_file`, `write_file`,
`edit_file`, `jobs`, `plan`, `ask_user`, `agent`, `agents`) and wasm plugins for
everything else. These pages are embedded in the binary. `ah docs` lists them and `ah docs <topic>` prints one in full.

## Topics

| Topic | Command | Covers |
|---|---|---|
| config | `ah docs config` | every setting with its default, config files, layering, `--set` and `/set` |
| keys | `ah docs keys` | key bindings, the key string syntax, plugin key binds |
| commands | `ah docs commands` | CLI flags and subcommands, slash commands, pickers, the `--json` event stream |
| instructions | `ah docs instructions` | AGENTS.md and CLAUDE.md project instruction files, `/init` |
| skills | `ah docs skills` | saved prompts in skills directories, `/skills` |
| plugins | `ah docs plugins` | writing, building and installing wasm plugins: manifest, hooks, payloads, host calls |
| sessions | `ah docs sessions` | session files, resume, naming, `/clear`, compaction, history and model cache |
| voice | `ah docs voice` | dictation: the talk key, the model that transcribes, what it costs, where the audio goes |

## Where things live

| Path | Purpose |
|---|---|
| `~/.config/ah/config.toml` | user configuration (`ah config init` writes the defaults with comments) |
| `~/.config/ah/credentials.toml` | OpenRouter key stored by `ah login`, mode 600 |
| `~/.config/ah/favorites.toml` | favorites managed by `/favorite` |
| `~/.config/ah/state.toml` | choices a picker settled: the voice provider, model and microphone |
| `~/.config/ah/plugins/*.wasm` | user plugins (`sources.json` beside them records git origins) |
| `~/.config/ah/skills/` | user skills |
| `./.ah/config.toml` | project configuration, merged over the user file |
| `./.ah/plugins/*.wasm` | project plugins |
| `./.ah/skills/` | project skills |
| `~/.local/share/ah/sessions/<id>.jsonl` | session logs |
| `~/.local/share/ah/history` | prompt history shared by every session |
| `~/.local/share/ah/models.json` | cached OpenRouter model catalogue |
| `~/.local/share/ah/plugins/` | plugin key/value state |
| `~/.local/share/ah/ah.log` | debug log when `AH_LOG=1` |

`AH_CONFIG_DIR` and `AH_DATA_DIR` relocate the two trees. `ah config path`
prints the resolved locations.

## Environment variables

| Variable | Effect |
|---|---|
| `OPENROUTER_API_KEY` | API key; used as is, and stored on the first `ah login` |
| `AH_CONFIG_DIR` | replaces `~/.config/ah` |
| `AH_DATA_DIR` | replaces `~/.local/share/ah` |
| `AH_LOG` | `1` logs to `<data dir>/ah.log`; a path logs there instead |
| `AH_LOG_LEVEL` | `error`, `warn`, `info` or `debug` |
| `SHELL` | shell used by the `bash` tool when `tools.shell` is empty |

## Customisation map

| Want to | See |
|---|---|
| change colours, borders, prefixes | `[theme]` in config, or the `themes` plugin (`/theme nord`) |
| change layout, mouse, markdown, queue size | `[layout]` in config |
| rebind a key | `[keys]` in config, and the keys page |
| change model, reasoning effort, favorites | `[model]` in config, `/model`, `/effort`, `/favorite` |
| change the system prompt | `prompt.system` and `prompt.append` in config |
| give the model project rules | instructions page (AGENTS.md) |
| approve tool calls by hand | `permissions.mode = "ask"`, `--ask`, `/ask` |
| block shell commands | `permissions.deny` in config |
| run read-only tool calls at once | `parallel` in `[tools]`, config page |
| keep a server or long build running | `bash` with `background`, commands page |
| speak a prompt instead of typing it | `/voice`, voice page |
| tune compaction | `[context]` in config |
| cut input cost on long sessions | `cache` in `[context]`, config page |
| change the status bar | `/statusline`, `statusline.items` in config, or a `statusline` plugin hook |
| add a tool, slash command, policy or theme | plugins page |
| install a plugin from a git repository | `ah plugin install URL [DIR]`, plugins page |
| save a prompt for reuse | skills page |
| script ah from another program | `ah --json -p ...`, commands page |
