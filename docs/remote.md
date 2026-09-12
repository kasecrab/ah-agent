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
ah remote pair --url https://ah-relay.<you>.workers.dev --token <the secret>
```

That prints a QR code and, under it, the same pairing code as text. Point the
phone's camera at the QR: it holds a `razorback://pair` link, so the phone's own
camera app reads it and hands it to the app. If the camera will not cooperate,
type the code instead — it is grouped in fours to make that bearable, and `0`
and `1` are read as `O` and `I`, since neither digit appears in the alphabet it
uses.

The token is the relay's own provisioning secret, set when it was deployed —
see [relay/README.md](../relay/README.md). It is asked for only when a pairing
is made, it never reaches the phone, and it is no part of the key ladder; it is
there so that knowing the relay's URL is not enough to make pairings on it.
`AH_PROVISION_TOKEN` in the environment does instead of `--token`.

The URL is remembered, so later pairings need only `ah remote pair --token …`.

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

`remote.roots` is the one real fence, and its default — your home directory —
is not much of one. Narrow it to the directories you actually want reachable
from a phone, and it becomes worth something.

What the link does do is show its working. Anything a phone sends appears in
the transcript in front of you — the message, an interruption, an answer to a
tool prompt — so a session being driven from elsewhere never looks like a
session doing things on its own. `remote.notice = false` turns that off, which
is a strange thing to want.

Publishing is off until you ask for it. Pairing a machine and publishing from
it are separate decisions: `ah remote pair` does the first, `remote.enabled`
the second.

### What it does not do

Worth knowing before you rely on any of it:

- **One code, for the life of the pairing.** Every key comes from it, so there
  is no forward secrecy: somebody who records the traffic today and learns the
  code later can read what they recorded. Rotating means `ah remote forget` and
  pairing again, which takes a few seconds and is worth doing if a code has
  ever been somewhere it should not.
- **Paired phones are not isolated from one another.** The key a phone seals
  under is derived from the pairing code, so any phone holding the code can
  compute any other's. Two phones are two devices with the same key, not two
  accounts, and the `phone <hex>` a notice names is a label, not an identity.
- **The relay sees the shape of the traffic.** Not a word of what is in it, but
  how many frames, how big, and when — which is enough to know when you are
  working and roughly how much.
- **A command reaches the machine only while it is connected.** Nothing is
  queued for a desktop that is offline; the phone is told so instead.

The connect signature goes in a header rather than in the URL, so it is not
kept by whatever logs requests, and the relay refuses a second use of the same
one: a signature that did leak opens nothing, rather than opening a socket for
as long as the clocks allow.

## With nobody at the keyboard

```
ah remote serve             # publishes until stopped
ah remote serve --detach    # carries on in the background
ah remote serve --sudo      # and may run sudo, asking for the password now
```

A window publishes the one session it has. The daemon publishes the machine:
a phone can list what is on disk, bring one back, or start a new one. Each
session gets an engine of its own, with settings read from the directory it
runs in rather than from wherever the daemon happens to be standing.

Three things it does that a window does not, because nobody is watching it:

- **Sessions it starts ask before running tools**, however this machine is
  otherwise configured. `remote.trust_paired_device = true` turns that off.
  Read what this is and is not, below.
- **It will only run an agent where you said it may.** `remote.roots` lists
  those directories and defaults to your home directory, which is wide. A
  session on disk is checked against the same list before it is resumed, so
  the rule is about where an agent runs and not only about where a new session
  starts.
- **They may not run `sudo`.** See below. All three settings are read from the
  environment or your own config and nowhere else — a repository you cloned
  cannot widen any of them by being cloned.

**What "asks before running tools" is worth.** It is a guard against the model,
not against whoever holds the code: the prompt goes to the phone, and the phone
is the same party that asked for the tool in the first place. It means a
session driven from a phone cannot run a command nobody approved — it does not
mean a code holder is limited to what you would have approved. Against a code
holder, the things that actually hold are `remote.roots`, the `sudo` refusal,
and `permissions.deny`.

### sudo

`sudo` reads a password from the terminal its command was started from. For a
session the daemon started, that terminal is the one the daemon was launched
in, which nobody is reading — so a command needing a password does not fail,
it waits, and the phone is shown nothing until the tool gives up. So a session
driven from a phone is refused `sudo`, `doas`, `pkexec` and `su` before they
run, with a tool error saying why.

`ah remote serve --sudo` allows them instead. It asks for your password once,
when it starts, and refreshes that every minute for as long as it serves, so
no later command ever has a prompt to wait on. Started without the flag in a
terminal, it asks the question; started from `systemd` or with `--detach`, it
does not, and `remote.allow_sudo = true` is how you say yes there — though
with `--detach` the password cannot be asked for at all, since there is no
terminal left to ask at, and it stays refused with a line saying so.

Be clear about what allowing it means: for as long as the daemon serves,
anything a paired phone starts can become root without anybody being asked
again. It is off unless you ask for it, and it lasts only as long as that
`ah remote serve` does.

The check reads the command without running a shell, so it is a guard against
the ordinary case rather than a sandbox — `sh -c` with the name in a string
gets past it. It is there to turn a silent hang into a clear refusal, not to
contain a program that is trying to get around it.

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

`ah remote forget` ends it at the relay and then here: the hub drops its log
and its device list, closes both sockets, and refuses that name for good, so
the old code opens nothing. A relay that cannot be reached stops the command
rather than being skipped, because a pairing forgotten only on this side is one
that still answers whoever holds the code — `--local` forgets it here anyway
and says what that leaves behind.

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
| `allow_sudo` | `false` | let sessions the daemon starts run `sudo`, with the password asked for once at startup |

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
