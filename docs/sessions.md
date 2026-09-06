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

- Messages have the OpenAI chat shape; `reasoning` and `images` (data URLs)
  are present when used. The system prompt is not stored; it is rebuilt on
  every turn.
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
from the model catalogue, or `context.window` when set. When the conversation
reaches `context.compact_at` percent (default 90) and `context.auto_compact`
is on, ah asks the model for a summary (at most `context.summary_max_tokens`
tokens, no tools), replaces the messages with one user message containing it,
and writes a `_compact` marker to the session file. This happens before a
new message is sent or between tool calls inside a turn, never in the middle
of a request. `/compact [focus]` does it on demand; a focus is appended to the
summary request. The summary appears in the transcript and the event stream
(`compacted`).

## Related files

| File | Contents |
|---|---|
| `~/.local/share/ah/history` | one JSON string per line, every prompt and slash command sent from any session; last 1000 kept |
| `~/.local/share/ah/models.json` | OpenRouter catalogue cache (ids, pricing, context length, tool and reasoning support, modalities); refreshed when older than a day or with `/model refresh`, `ah models --refresh` |
| `~/.local/share/ah/plugins/` | per-plugin key/value state (`kv_get`, `kv_set`) |
