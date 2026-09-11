# Remote

Watching a session from a phone, and answering it.

A run started at the desk keeps going whether or not anyone is watching. `ah
remote` lets a phone watch it, say something into it, and answer the questions
it asks — without opening a port on this machine, and without anyone else
being able to read a word of it.

With `ah remote serve` running, a phone can also start sessions and bring
old ones back with no window open at all.

## How it fits together

Three parts. The harness publishes; a relay stores and forwards; the phone
reads.

The relay is a small Cloudflare Worker that runs on **your own** account, on the
free plan. It is the only part that is reachable from the internet, and it is
deliberately the part that knows nothing: every payload is sealed before it
leaves this machine, with keys derived from a pairing code the relay is never
given.

| The relay can see | The relay cannot see |
|---|---|
| how many frames, how big, and when | anything you or the model said |
| which pairing they belong to | which session, directory or model |
| the times a device connected | which device, or whose |

## Pairing

Deploy a relay once (see `relay/README.md` in the source tree), then:

```
ah remote pair --url https://ah-relay.<you>.workers.dev
```

That prints a QR code and, under it, the same pairing code as text. Point the
phone's camera at the QR: it holds a `razorback://pair` link, so the phone's own
camera app reads it and hands it to the app. If the camera will not cooperate,
type the code instead — it is grouped in fours to make that bearable, and `0`
and `1` are read as `O` and `I`, since neither digit appears in the alphabet it
uses.

The URL is remembered, so later pairings need only `ah remote pair`.

```
ah remote status     # whether this machine is paired, and to what
ah remote forget     # make the code useless
```

## What the code is worth

Treat it like a key — the same key as the keyboard.

Whoever holds it sees everything the session says, and can say things into it.
A prompt sent from a phone runs in the session you have open, under the
permissions you gave it: if the window is in `auto`, that prompt runs tools
without asking, exactly as if you had typed it yourself. Nothing about the
link makes it safer than sitting down at the machine, and it is not meant to
be: the pairing code is the whole of the boundary.

What the link does do is show its working. Anything a phone sends appears in
the transcript in front of you — the message, an interruption, an answer to a
tool prompt — so a session being driven from elsewhere never looks like a
session doing things on its own. `remote.notice = false` turns that off, which
is a strange thing to want.

Publishing is off until you ask for it. Pairing a machine and publishing from
it are separate decisions: `ah remote pair` does the first, `remote.enabled`
the second.

## With nobody at the keyboard

```
ah remote serve             # publishes until stopped
ah remote serve --detach    # carries on in the background
```

A window publishes the one session it has. The daemon publishes the machine:
a phone can list what is on disk, bring one back, or start a new one. Each
session gets an engine of its own, with settings read from the directory it
runs in rather than from wherever the daemon happens to be standing.

Two things it does that a window does not, because nobody is watching it:

- **Sessions it starts ask before running tools**, however this machine is
  otherwise configured. `remote.trust_paired_device = true` turns that off.
- **It will only start a session where you said it may.** `remote.roots`
  lists those directories and defaults to your home directory. Both settings
  are read from the environment or your own config and nowhere else — a
  repository you cloned cannot widen either by being cloned.

Only one of them publishes at a time. Opening a window takes the pairing from
the daemon, which stands down within a few seconds and picks it up again when
the window closes; the phone sees the link change and carries on.

## Answering from either end

A tool prompt goes to both screens at once and the first answer wins; the
other is dropped rather than applied to whatever question came next, and the
box on the losing screen comes down by itself saying who answered it.

A message sent while a turn is running does not queue behind it. It goes into
the same mailbox a subagent's messages use, which the loop reads between
requests — so it reaches the model in the middle of the work rather than after
it. Sent while nothing is running, it simply starts a turn.

`ah remote forget` ends it from this side, and the relay keeps nothing
readable either way.

## Settings

`[remote]` in `~/.config/ah/config.toml`.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | publish while the window is open |
| `flush_ms` | `200` | how long text is gathered before a frame goes out |
| `max_frame_bytes` | `16384` | largest frame built before sending early |
| `max_event_bytes` | `65536` | longest single event; the rest is a note saying how much was left |
| `outbox_bytes` | `262144` | frames held while the relay is unreachable |
| `snapshot_messages` | `20` | messages of scrollback a phone gets on attaching |
| `notice` | `true` | say in the transcript when a phone attaches |
| `roots` | `[]` | directories `ah remote serve` may start a session in; empty means `$HOME` |
| `trust_paired_device` | `false` | let sessions the daemon starts run tools without asking |
| `max_sessions` | `4` | sessions the daemon holds open at once |

## Files and variables

| Where | What |
|---|---|
| `~/.config/ah/credentials.toml` | the pairing code and relay URL, owner-readable only |
| `AH_REMOTE_CODE` | the pairing code, ahead of the file |
| `AH_REMOTE_URL` | the relay, ahead of the file |
| `AH_REMOTE_ROOTS` | directories the daemon may start a session in, colon separated, ahead of the config |
| `<data dir>/remote.log` | what a detached `ah remote serve` has to say |

## When it will not connect

| What it says | What it means |
|---|---|
| `the relay has never heard of this pairing` | the relay was redeployed, or the code is from an older pairing. Pair again. |
| `a desktop is already connected` | another `ah` on this machine, or another machine, holds the pairing. It gives way once it goes quiet. |
| `the pairing was revoked` | `ah remote forget` was run somewhere. Pair again. |
| `the relay and this machine disagree about the time` | a clock is more than five minutes out. It is the only place a clock matters. |

## Costs

Nothing, within Cloudflare's free plan, and not close to the edges of it. Text
from the model is gathered for 200 ms at a time rather than sent token by
token, so a two-minute turn is a few hundred frames rather than tens of
thousands — around 30 billable requests against a daily 100,000.
