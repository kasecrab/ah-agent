# Remote

Watching a session from a phone, and answering it.

A run started at the desk keeps going whether or not anyone is watching. `ah
remote` lets a phone watch it — without opening a port on this machine, and
without anyone else being able to read a word of it.

Watching is all it does so far. Saying something back, answering a tool
prompt, and starting a session from the phone are being built on top of the
same link.

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

Treat it like a key. Whoever holds it sees everything the session says: what
you asked, what the model answered, which files it read and what was in them.
It cannot yet make the session do anything — but reading is not nothing, and
the code is the only thing standing in the way of it.

Publishing is off until you ask for it. Pairing a machine and publishing from
it are separate decisions: `ah remote pair` does the first, `remote.enabled`
the second.

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

## Files and variables

| Where | What |
|---|---|
| `~/.config/ah/credentials.toml` | the pairing code and relay URL, owner-readable only |
| `AH_REMOTE_CODE` | the pairing code, ahead of the file |
| `AH_REMOTE_URL` | the relay, ahead of the file |

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
