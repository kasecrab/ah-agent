# Sessions

Every TUI run appends to a session file; one-shot runs keep the conversation
in memory. Files live in `~/.local/share/ah/sessions/<id>.jsonl` (`AH_DATA_DIR`
relocates them).

## File format

JSON Lines. The first line is the header, then one message per line, with
marker lines in between:

```
{"id":"3f9a1c2b7e4d5a60","started_ms":1757100000000,"cwd":"/home/me/proj","model":"deepseek/deepseek-v4-flash-0731"}
{"role":"user","content":"add a test for parse()"}
{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"src/lib.rs\"}"}}]}
{"role":"tool","content":"1\t...","tool_call_id":"call_1"}
{"role":"assistant","content":"Added `parses_empty` in src/lib.rs."}
{"_name":"tests"}
{"_compact":true}
{"role":"user","content":"[The conversation so far was compacted. Summary:] ... [End of summary. Continue from here.]"}
```

- Messages have the OpenAI chat shape; `reasoning` and `images` are present
  when used. The system prompt is not stored; it is rebuilt on every turn.
- An `images` entry is a `data:` URL for a picture attached to the prompt, and
  an `ah-image:<session>/<file>` reference for one the model drew. Generated
  images are written to `~/.local/share/ah/images/<session>/` and only named
  here: a base64 picture inlined into a line would slow every later `/resume`
  scan for the life of the install. The reference is relative to `AH_DATA_DIR`,
  so moving the data directory keeps it working. An older ah reading a file
  with one of these would hand the reference to the provider as an image and
  get an error back.
- Nothing deletes the images. `/clear` and compaction append a marker and stop
  loading what is above it, but the files stay; `rm -r
  ~/.local/share/ah/images/<id>` clears one session's pictures.
- An answer the model did not finish — Esc, or a connection dropped part way
  — is kept and marked `[cut short]`, without any tool call it had begun: those
  are truncated and have no result to pair with. The tokens were paid for, so
  the next turn carries on from what was said instead of asking for it again.
- `{"_name": "..."}` names the session (`/rename`).
- `{"_clear": true}` (`/clear`) and `{"_compact": true}` (compaction) mean
  "ignore every message above". Nothing is rewritten; the file keeps the full
  history and only the tail after the last such marker is loaded.

Session ids are hex, derived from the start time and the process id.

## Listing and resuming

| Command | Effect |
|---|---|
| `ah sessions` | id, age, name, message count, directory |
| `ah -r` | resume the latest session |
| `ah -r ID` or `ah -r NAME` | resume by id or by name (newest with that name) |
| `/resume` | picker inside the TUI, switches in place |
| `/rename NAME` | name the current session; `/rename` alone prompts, empty removes |
| `/session` | current id and path |

On quit the TUI prints the `ah -r ...` command that brings the conversation
back.

## Compaction

The status bar shows how full the context window is (`42%`). The window comes
from the model catalogue, or `context.window` when set. A model the catalogue
does not list has no window, and without one nothing is compacted on its own:
ah says so once at the start of a turn, and `context.window` sets a size by
hand. The catalogue is looked up again each turn until it answers, so one that
arrives mid-session is picked up.

When the conversation reaches `context.compact_at` percent (default 90) and
`context.auto_compact` is on, ah asks the model for a summary (at most
`context.summary_max_tokens` tokens, no tools), replaces the messages with one
user message containing it, and writes a `_compact` marker to the session file.
This happens before a new message is sent or between tool calls inside a turn,
never in the middle of a request. `/compact [focus]` does it on demand; a focus
is appended to the summary request.

While the summary is being written the working line reads `Compacting` and
carries a bar: `[━━━━━━━━━───────] 56%`. Two things move it and it takes
whichever is further along. The model spends the first stretch reading the
conversation and says nothing during it — on a long one that is a minute with
no signal at all — so the clock carries the bar there, quickly at first and
easing off. The same band that sweeps the word sweeps the bar throughout, and
the fill grows in eighths of a cell, so it creeps rather than jumping a whole
cell at a time.
The summary coming back moves it too, against `context.summary_max_tokens`, and
overtakes the clock when the model gets to the point quickly. It stops at 99: a
model stops when the summary is done, not when it runs out of room, so the last
step belongs to the end of the request. An automatic compaction is announced
with `context full; compacting`.
The summary itself
folds into one line in the transcript — `≡ context compacted: 48.0k → ~3.2k
tokens` — which Ctrl-T opens and closes, the same key as tool output; a session
replayed with `ah -r` folds its summary the same way. The event stream carries
`compacting` (with `auto`), `compact_progress` (`done` and `budget`, every
120 ms while the summary streams) and `compacted` (with the whole summary).

## Related files

| File | Contents |
|---|---|
| `~/.local/share/ah/history` | one JSON string per line, every prompt and slash command sent from any session; last 1000 kept |
| `~/.local/share/ah/models.json` | OpenRouter catalogue cache (ids, pricing, context length, tool and reasoning support, modalities); refreshed when older than a day or with `/model refresh`, `ah models --refresh` |
| `~/.local/share/ah/plugins/` | per-plugin key/value state (`kv_get`, `kv_set`) |
