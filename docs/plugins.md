# Plugins

A plugin is a WebAssembly core module (`wasm32-unknown-unknown`) that ah runs
in the `wasmi` interpreter. Plugins can change any setting, rewrite the system
prompt and every request, allow, deny, replace or confirm tool calls, add
tools the model can call, add slash commands, draw the status line and bind
keys. They talk to the host with JSON in both directions.

Plugins are sandboxed: no filesystem, network or clock except through the
host calls below, a fuel budget per hook call (`plugins.fuel_per_call`) and a
memory cap (`plugins.max_memory_bytes`). A hook that traps or runs out of fuel
is disabled for the rest of the session; the harness keeps going.

Plugins run for the conversation you are in, not for subagents: the
interpreter is one, and a hook cannot be in two loops at once. So a plugin
that gates tool calls does not see what an agent does, which is why an agent
gets only the tools its type lists (`ah docs config`), and why the ones it
gets by default cannot write anything.

## Where plugins load from

1. `~/.config/ah/plugins/*.wasm`
2. `./.ah/plugins/*.wasm`
3. files and directories in `plugins.paths` and `--plugin PATH`

`plugins.disabled` lists names or file stems to skip; `plugins.enabled =
false` or `--no-plugins` loads nothing. `ah plugin list` shows what was found
and whether it loaded. `/plugins` inside the TUI lists the active ones and
`/reload` reloads them after a rebuild or install.

## Installing from git

Plugins are shared as git repositories: either the repository is the plugin,
or the plugin sits in a directory of a larger repository. `ah plugin install`
clones it, builds it when it ships as source, copies the module into
`~/.config/ah/plugins/` and records where it came from.

```
ah plugin install https://github.com/kasecrab/ah-agent plugins/themes   # directory inside a repo
ah plugin install https://github.com/kasecrab/ah-agent/tree/main/plugins/themes   # same, as a GitHub link
ah plugin install someone/ah-guard                 # GitHub shorthand, plugin at the repo root
ah plugin install git@example.com:team/plugin.git --ref v2   # any git URL, a branch, tag or commit
ah plugin install ../my-plugin                     # a local repository
ah plugin update [NAME]                            # reinstall from the recorded sources
ah plugin rm NAME
```

The source directory (the repository root, or the directory named after the
URL) is handled in this order:

1. If it holds `*.wasm` files, they are installed as they are. A repository
   can commit the built module so users need no Rust toolchain.
2. Otherwise it must hold a `Cargo.toml`; ah asks first, then runs
   `cargo build --release --target wasm32-unknown-unknown` there and installs
   every `cdylib` package it defines. Builds share
   `~/.local/share/ah/plugin-build/` so updates are incremental. The
   `wasm32-unknown-unknown` target must be installed (`rustup target add
   wasm32-unknown-unknown`).

It asks because building is the one part of this that is not sandboxed and
cannot be: `cargo build` runs the crate's `build.rs` and every proc-macro it
depends on as you, with your files and your network, before a byte of it
reaches the wasm interpreter. Installing a prebuilt `.wasm` is not asked
about — that one only ever runs inside the sandbox. Where there is nobody to
ask, the build is refused rather than assumed; `AH_PLUGIN_BUILD_YES=1` is how
a script says yes deliberately.

### Where plugins are loaded from

`~/.config/ah/plugins/` always. `.ah/plugins/` in the directory you are
working in only if your own config sets `plugins.trust_project = true`, which
a project cannot set for you — a `.wasm` that arrived with a repository is a
program somebody else wrote, and it loads before the first request of the
session.

### Updating

`~/.config/ah/plugins/sources.json` maps each installed file stem to its
`url`, `path` and `ref`. `ah plugin update` repeats the install for every
entry (or one, `ah plugin update themes`): a fresh clone of the recorded
ref, a build when the repository ships source, and the module replaced in
place. A branch ref, or none, follows the branch's newest commit; a tag or
commit ref reinstalls the same code and only changes anything when its
build inputs did. `ah plugin rm` deletes the module and its entry. Plugins
added by hand with `ah plugin add` or `ah plugin build` have no source and
are not touched by `update`.

A plugin keeps working across ah releases as long as the ABI version
matches: new fields in hook payloads have defaults and unknown fields are
ignored on both sides. When ah bumps `abi_version`, `ah plugin list` reports
`abi version N != host M` for the module and `ah plugin update` rebuilds it
against the SDK the plugin's `Cargo.toml` points at (`branch = "main"` in
the examples, so the rebuild picks up the current SDK).

## Writing one in Rust

`Cargo.toml`:

```toml
[package]
name = "hello"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

# Empty table: build the same way alone and inside another repository.
[workspace]

[dependencies]
ah-plugin-sdk = { git = "https://github.com/kasecrab/ah-agent", branch = "main" }

[profile.release]
opt-level = "z"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```

`src/lib.rs`:

```rust
use ah_plugin_sdk::prelude::*;

fn manifest() -> Manifest {
    Manifest {
        name: "hello".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "Purple accent, blocks curl | sh".into(),
        hooks: vec![Hook::BeforeTool, Hook::SlashCommand],
        commands: vec![SlashCommandSpec {
            name: "hello".into(),
            description: "Say hello".into(),
            usage: "/hello [name]".into(),
        }],
        settings_patch: Some(json!({"theme": {"accent": "magenta"}})),
        ..Default::default()
    }
}

fn handle(hook: Hook, input: Value) -> Result<Value, String> {
    match hook {
        Hook::BeforeTool => {
            let inp: BeforeToolIn = serde_json::from_value(input).map_err(|e| e.to_string())?;
            if inp.call.function.name == "bash" && inp.call.function.arguments.contains("| sh") {
                return Ok(json!({"decision": "deny", "reason": "piping into sh is off"}));
            }
            Ok(json!({"decision": "allow"}))
        }
        Hook::SlashCommand => {
            let inp: SlashCommandIn = serde_json::from_value(input).map_err(|e| e.to_string())?;
            Ok(json!({"message": format!("hello {}", inp.args)}))
        }
        _ => Ok(Value::Null),
    }
}

ah_plugin_sdk::plugin!(manifest, handle);
```

Build and install:

```
rustup target add wasm32-unknown-unknown
ah plugin build ./hello            # cargo build --release --target wasm32-unknown-unknown, then copies the .wasm into ~/.config/ah/plugins
ah plugin build ./hello --no-install
ah plugin add path/to/hello.wasm   # install a prebuilt module
ah plugin install <git url> [DIR]  # clone, build and install (section above)
ah plugin rm hello
```

The SDK prelude re-exports every `ah_abi` type (`Manifest`, `Hook`,
`ToolSpec`, the `*In`/`*Out` payload structs, `Settings`, `Message`,
`ToolCall`, `ToolResult`), `serde_json` with `json!` and `Value`, and the
host helpers `settings_get`, `kv_get`, `kv_set`, `command_segments`,
`command_matches`, `host_call` and the `log!` macro. Returning `Value::Null`
from a hook means "no change". Returning `Err(String)` logs the error and is
treated as no change; a panic traps and disables that hook.

`before_tool` is the exception, because for that hook silence is an answer.
An `Err`, a trap, running out of fuel, or an output the host cannot read into
`BeforeToolOut` — a missing or misspelled `decision`, an empty object — is
treated as **deny**, with the plugin named in the reason, and the hook is *not*
disabled for the session. Otherwise the way to switch a policy off would be to
send it one command it cannot handle, which is the command you would most want
it kept on for. `decision` is required: there is no default.

### Reading a shell command

`command_segments(cmd)` and `command_matches(cmd, rules)` hand a plugin the
harness's own reading of a command — split on `;`, `|`, `&` and newlines,
whitespace-normalised, with a leading `sudo`, `env`, `nohup`, `time` or
`VAR=value` dropped — and the same rule syntax `permissions.deny` uses: a rule
matches a segment that equals it or starts with it followed by a space, and a
trailing `*` matches any continuation.

Use these rather than `cmd.contains(pattern)`. A substring match over the raw
text refuses `git commit -m "the rm -rf / case"` and lets `rm -fr /` past; it
matches spelling, not meaning. Neither reading can see through a shell: what
`sh -c "$(…)"` or a script written and then run will do is not in the text at
all, so a policy should ask about those rather than guess.

Any language that can produce a core wasm module works. The module must
export `ah_alloc(len) -> ptr`, `ah_free(ptr, len)`, `ah_manifest() -> i64` and
`ah_call(hook_ptr, hook_len, in_ptr, in_len) -> i64`, where an `i64` result
packs `(ptr << 32) | len` of a buffer the host reads then frees with
`ah_free`. `ah_call` receives the hook name and the input JSON and returns
`{"ok": <output>}` or `{"err": "<message>"}`. Imports come from module
`"ah"`: `log(level, ptr, len)`, `host_call(name_ptr, name_len, in_ptr,
in_len) -> i32` (result length, negative on error) and `host_read(dst, cap)
-> i32` which copies the pending result.

## Manifest

| Field | Meaning |
|---|---|
| `name` | unique name; used by `plugins.disabled`, `ah plugin rm`, log lines |
| `version` | free text |
| `abi_version` | must equal the host's (currently 1); the SDK fills it in |
| `description` | shown by `ah plugin list` and `/plugins` |
| `hooks` | hooks the plugin handles; others are never called |
| `tools` | `ToolSpec` entries (OpenAI function shape: name, description, JSON schema) added to the model's tool list; calls arrive through the `tool_call` hook |
| `commands` | slash commands (`name`, `description`, `usage`) routed to `slash_command` |
| `settings_patch` | merge patch applied at load, before `on_load` |
| `order` | where in the hook chain this plugin runs; lower first, default `0` |
| `env` | environment variable names `env_get` may read; empty means none |

## Hooks

Inputs and outputs are JSON objects with these fields. Any `settings_patch`
in an output is merged into the live settings (and shown as a
`settings_patch` event in `--json` mode, carrying the plugin's name).

A patch is merged on the plugin's own footing wherever it came from — the
manifest, `on_load`, or any later hook. The guarded keys in
[config](config.md#what-a-projects-config-may-not-set) are refused from it and
the refusal is said out loud, so a plugin that behaves at load and misbehaves
at `before_tool` gets no further than one that misbehaves at load. A one-shot
run (`-p`) has no stack to fold a mid-turn patch into and prints a line saying
so instead of applying it.

| Hook | When | Input | Output |
|---|---|---|---|
| `on_load` | after loading, and on `/reload` | `settings` (merged tree), `cwd` | `settings_patch` |
| `system_prompt` | before each request | `prompt` (assembled text, instructions included), `cwd`, `os`, `shell` | `prompt` |
| `before_request` | before each request | `request` (`model`, `messages`, `tools`, sampling fields), `turn` | `request` |
| `before_tool` | before a tool runs | `call`, `cwd` | `decision` (below) plus `settings_patch` |
| `after_tool` | after a tool ran | `call`, `result`, `duration_ms` | `result` (replacement, optional), `settings_patch` |
| `tool_call` | the model called a tool this plugin declared | `call`, `cwd` | `result` (`output`, `is_error`, optional `diff`) |
| `statusline` | every status bar redraw | `StatusContext` (below) | `text`, or `spans` (below) |
| `slash_command` | user ran a command from `commands`, or used a picker it opened | `name`, `args`, `cwd`, `stage` (`run`, `preview`, `pick`) | `message` (notice), `send_to_model` (submitted as a user message), `settings_patch`, `picker` (below) |
| `on_turn_end` | after the assistant's final message | `message`, `usage`, `total_usage`, `tool_calls` | `message` (notice), `settings_patch` |
| `keybinds` | reserved | none | `binds`: `[[key, action]]`, action = a `[keys]` field name or a slash command. Declared in the ABI; the TUI does not apply plugin binds yet |

`before_tool` decisions:

```json
{"decision": "allow"}
{"decision": "deny", "reason": "why"}
{"decision": "replace", "arguments": "{\"command\":\"ls -la\"}"}
{"decision": "ask", "reason": "confirm this one even in auto mode"}
```

Every plugin with the hook is consulted in `order`, lowest first, and plugins
that name the same order run in the order they were found. Set it in the
manifest: rewriters negative, policies positive, everything else zero. Before
`order` existed the sequence was the alphabet of the file names, so renaming
`policy.wasm` to `zz-policy.wasm` changed what the policy saw.

A deny stops the chain at once. A replace feeds the new arguments to the next
plugin. An ask is sticky: once any plugin has asked, only a deny changes that
— a replace cannot erase it, and an ask raised after a replace is still an ask.

A replace that lands after another plugin has already answered turns the call
into an ask, naming both plugins. What that earlier plugin approved is not what
would run, and nothing downstream could tell the difference. Give your
rewriters a negative `order` and they run before anything has judged, so this
does not come up.

Built-in `permissions.deny` rules are checked after the plugins, on the final
arguments, and a plugin cannot lift them.

### Pickers

A `slash_command` answer may carry a `picker` to let the user choose from a
list instead of typing an argument:

```json
{"picker": {"title": "theme", "selected": 1, "preview": true,
            "items": [{"value": "nord", "label": "nord", "detail": ""},
                      {"value": "off", "label": "off", "detail": "colours from config"}]}}
```

The TUI opens the list with the `selected` item under the cursor; typing
filters it. Enter calls the command again with `stage = "pick"` and the
item's `value` as `args`; the plugin then does what it would do for a typed
argument. With `preview = true` every cursor move calls the command with
`stage = "preview"` and the item's `value`; return only a `settings_patch`
so the user sees the effect at once, and change no state, because the host
undoes all preview patches when the picker is cancelled with Esc, or after
the `pick` answer was applied. Pickers are ignored in one-shot mode, so a
command should also accept a typed argument. Answering `null` to any hook
means "no change" and is never an error.

`StatusContext` fields: `model`, `usage` (`prompt_tokens`,
`completion_tokens`, `cost`), `cwd`, `git_branch`, `plugins` (count), `state`
(`idle`, `thinking`, `streaming`, `compacting`, `tool:<name>`), `session_id`, `width`,
`favorite`, `effort`, `context_tokens`, `context_window`, `modalities`
(`TI→T`), and `rendered`, the text the built-in `statusline.items` produced,
so a plugin can decorate instead of replace.

A `text` answer is drawn in the status bar's own colour (`status_fg` and
`status_bg`). To colour the row piece by piece, answer with `spans` instead:

```json
{"spans": [{"text": " gpt-5", "style": "heading bold"},
           {"text": " · ", "style": "dim"},
           {"text": "89%", "style": "error"}]}
```

`style` is a `[theme]` key (`accent`, `heading`, `tool`, `dim`, `link`,
`user`, `error`, `fg`, ...), a colour name, a `#rrggbb` colour or a 0-255
index, followed by any of `dim`, `bold`, `italic` and `underline`. Naming a
theme key keeps the row in step with whatever theme is on. An empty `style`
leaves the piece in the row's colour. A row given as spans is drawn over the
terminal's background rather than `status_bg`, so the colours stay readable.

## Host calls

Available through `host_call(name, &json)` or the typed helpers.

| Name | Input | Output |
|---|---|---|
| `settings_get` | JSON pointer string (`"/theme/accent"`, `""` for all) | the value; unknown top-level tables from config are visible here |
| `kv_get` | key | string or null; persisted per plugin under `~/.local/share/ah/plugins/<stem>.json` |
| `kv_set` | `{"key": k, "value": v}` (`null` removes) | null |
| `cwd` | none | working directory |
| `now_ms` | none | unix time in milliseconds |
| `env_get` | variable name | string, or null unless the manifest's `env` names it |
| `read_file` | path inside the working directory | file contents, up to 8 MiB of a regular file |
| `command_segments` | a shell command | the segments the harness reads it as |
| `command_matches` | `{"command": c, "rules": [r]}` | the first rule that matches, or null |
| `git_branch` | none | current branch, read from `.git/HEAD` |

`log!(LogLevel::Info, "...")` writes to the host log, visible with `AH_LOG=1`
in `~/.local/share/ah/ah.log`.

### What a plugin cannot reach

The sandbox is the host calls, so this is where its edges are.

`settings_get` and the `on_load` input both have `model.api_key` taken out.
`env_get` reads only the variables the manifest's `env` list names, and never
`OPENROUTER_API_KEY`, `AH_API_KEY`, `DEEPGRAM_API_KEY`, `AH_REMOTE_CODE`,
`AH_PROVISION_TOKEN` or `AH_VOICE_CAPTURE_CMD` whatever it names — those are
where the documentation tells people to keep their keys, and `before_request`
would carry one out in the next request. A `bash` tool call has the same
variables removed from its environment.

`read_file` resolves the path and refuses anything outside the working
directory, refuses anything that is not a regular file, and stops at 8 MiB.
The credentials file, the ssh keys and the browser's cookie jar are not the
business of a program that was installed for a project.

## Publishing a plugin

A plugin repository needs nothing beyond the crate: `Cargo.toml` with the
`[workspace]` table and the git dependency shown above, and `src/lib.rs`. The
same layout works in a subdirectory of any repository; users then pass the
directory as the second argument of `ah plugin install`, or paste the
GitHub link to it. Commit the built `.wasm` next to `Cargo.toml` when users
should be able to install without a Rust toolchain; ah prefers a prebuilt
module over building.

Read config from your own top-level table (`[my_plugin] key = ...`) through
`settings_get("/my_plugin")`; unknown tables are kept in the settings tree
for that purpose. Keep state in `kv_set`, which persists per plugin. Return
messages, not `Err`, for user mistakes such as a bad argument, so the user
sees them in the transcript.

## Examples in the repository

`plugins/themes/` is the reference example: a complete, installable plugin
with a `/theme` slash command and live-preview picker, `on_load`, `settings_get`, `kv_get`/`kv_set`,
`log!` and a `README.md` describing the layout.

```
ah plugin install https://github.com/kasecrab/ah-agent plugins/themes
```

Inside ah, `/theme` opens a picker over the bundled palettes (dracula, nord,
gruvbox, gruvbox-light, catppuccin-mocha, catppuccin-latte, tokyo-night,
solarized-dark, solarized-light, one-dark, monokai) that previews each one
as the cursor moves; Enter keeps it, Esc goes back. `/theme nord` switches
directly and `/theme off` restores the colours from the config file, both
also in one-shot mode; `/theme list` prints the names. `[themes] name = "nord"` sets a default, `[themes] background = true`
also paints the background keys, and `[themes.palettes.<name>]` adds a
palette (`base = "nord"` starts from a bundled one).

The unpublished `plugins/` workspace in a source checkout has three more
small examples built with `just plugins`:

| Plugin | Shows |
|---|---|
| `statusline` | the `statusline` hook and `kv_set` for a persisted peak cost |
| `guard` | `before_tool` deny and ask decisions, config read through `settings_get` (`[guard] deny = [...]`), a `/guard` slash command |
| `tool-wordcount` | a `tools` entry and the `tool_call` hook using `read_file` |
