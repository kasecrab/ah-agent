# Key bindings

Bindings live under `[keys]` in the config. Each action takes a list of key
strings; the first entry is shown in help text.

## Key string syntax

`modifier-modifier-key`, case-insensitive, `+` also works as the separator.

- Modifiers: `ctrl` (`control`, `c`), `alt` (`meta`, `opt`, `option`, `m`),
  `shift` (`s`), `super` (`cmd`, `win`).
- Named keys: `enter` (`return`), `esc` (`escape`), `tab`, `backtab`,
  `backspace` (`bs`), `delete` (`del`), `insert` (`ins`), `home`, `end`,
  `pageup` (`pgup`), `pagedown` (`pgdn`), `up`, `down`, `left`, `right`,
  `space`, `minus`, `plus`, `f1` to `f12`.
- Any single character: `ctrl-j`, `alt-k`, `ctrl-shift-x`.
- `shift-tab` is the same as `backtab`.

Shift-Enter and other shifted specials need the kitty keyboard protocol
(`layout.kitty_keyboard = true` and a terminal that speaks it: kitty, WezTerm
with `enable_kitty_keyboard = true`, foot, ghostty); Alt-Enter and Ctrl-J work
everywhere.

## Defaults

| Action | Default | Effect |
|---|---|---|
| `submit` | `["enter"]` | send the message, or queue it while a turn runs |
| `newline` | `["shift-enter", "alt-enter", "ctrl-j"]` | insert a line break |
| `cancel` | `["esc"]` | cancel the running turn, stopping a command that is still running; keeps what the model had already written, marked `[cut short]`; returns queued messages to the input |
| `quit` | `["ctrl-c", "ctrl-d"]` | exit (Ctrl-C cancels first while busy) |
| `scroll_up` | `["ctrl-up", "alt-k"]` | scroll transcript one step |
| `scroll_down` | `["ctrl-down", "alt-j"]` | |
| `page_up` | `["pageup"]` | scroll a page |
| `page_down` | `["pagedown"]` | |
| `scroll_top` | `["ctrl-home"]` | jump to the start |
| `scroll_bottom` | `["ctrl-end"]` | jump to the end |
| `clear` | `["ctrl-l"]` | clear the conversation (same as `/clear`) |
| `toggle_tools` | `["ctrl-t"]` | expand or collapse tool output and compaction summaries |
| `toggle_reasoning` | `["ctrl-r"]` | expand or collapse thinking blocks |
| `cycle_model` | `["shift-tab"]` | switch to the next favorite model |
| `prev_agent` | `["left"]` | with an empty input, look at the previous agent; from the first one back to the main conversation |
| `next_agent` | `["right"]` | with an empty input, look at the next running agent |
| `history_prev` | `["up", "ctrl-p"]` | earlier prompt; inside a multi-line draft moves the cursor first; with an empty input pulls back the last queued message |
| `history_next` | `["down", "ctrl-n"]` | later prompt; with an empty input opens the background job list |
| `delete_word` | `["ctrl-w", "ctrl-backspace", "ctrl-h", "alt-backspace"]` | delete the word before the cursor, or a whole chip |
| `delete_line` | `["ctrl-u"]` | clear the input |
| `yank` | `["ctrl-y"]` | put back what the last delete took |
| `line_start` | `["ctrl-a", "home"]` | |
| `line_end` | `["ctrl-e", "end"]` | |
| `paste_image` | `["ctrl-v"]` | attach the clipboard image as an `[Image #1: 120 KB]` chip |
| `open_image` | `["ctrl-o"]` | open the newest picture the model drew; with several, opens the `/images` list |
| `toggle_plan` | `["alt-p"]` | show or hide the plan summary above the input |
| `voice` | `["alt-v"]` | arm or disarm dictation (same as `/voice`) |
| `talk` | `["space"]` | held to listen while dictation is armed |

Example:

```toml
[keys]
submit = ["enter", "ctrl-s"]
quit = ["ctrl-q"]
```

`/keys` in the TUI prints the active bindings.

## Fixed keys

- `/` at the start of an empty input opens the command popup; Tab completes,
  Enter runs, arrows move.
- Backspace on a `[Pasted #1: N lines]` or `[Image #1: …]` chip removes the
  whole chip.
- Mouse wheel scrolls; drag selects and copies through OSC 52 when
  `layout.mouse` is on. A double click takes the word under the pointer
  (paths like `crates/ah/src/tui/mod.rs:106` count as one word), a triple
  click the whole row, and a fourth goes back to dragging cells. Shift-drag
  keeps the terminal's own selection.
- The question box (`ask_user`): `1`-`9` goes to the row it numbers and picks
  it, Up/Down move between rows, `space` picks the highlighted one, and typing
  anything else starts an answer of your own. Left/Right (and Tab/Shift-Tab)
  walk the questions when there are several; on a row with text in it, Left
  and Right move through the text instead. Enter takes the highlighted option
  when the question is unanswered and takes one answer, then sends — or goes
  to the first question still without an answer and names it. Sending and
  leaving both ask first: Enter (or `y`) goes through with it, Esc goes back.
  Ctrl-C interrupts the turn without asking.
- Pickers (`/model`, `/resume`, `/favorite`, `/skills`): type to filter,
  Up/Down or Tab/Shift-Tab move, PgUp/PgDn page, Enter accepts, Esc closes,
  Ctrl-U clears the query, Ctrl-R refreshes the model list. In `/model`,
  Left/Right walk the categories along the top and the query carries over;
  the category you leave it on is the one it opens on next. `/favorite` uses
  bare letters: `n` new, `m` model, `e` effort, `r` rename, `d` remove,
  `j`/`k` move, `q` close.

## Plugin key binds

The plugin ABI defines a `keybinds` hook returning `[(key, action)]` pairs,
where the action is a `[keys]` field name (`"toggle_tools"`) or a slash
command (`"/guard"`). The TUI does not apply plugin binds yet; bind keys in
`[keys]` for now.
