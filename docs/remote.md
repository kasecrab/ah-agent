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

"Which device" needs one qualification. Every command a phone sends carries a
`plink` in the clear, beside the sealed part: sixteen random bytes the app
mints when it starts a run. The relay hands it to the desktop and keeps no
copy of it. It is a pseudonym rather than a name — it says that two commands
came from the same phone in the same run of the app, and nothing more, and
the next run picks a fresh one. The device rows the relay does keep are filed
under the nonce a socket signed with, which is new every time anything dials,
so those do not join two connections together either.

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
  The two directions are not derived alike, either: the desktop's key binds
  only its own connection, since every attached phone has to be able to open
  it, while a phone's key binds its own connection as well, so two phones
  never seal under one key. The wire protocol section below says what that
  buys and what it rests on.
- **The relay sees the shape of the traffic.** Not a word of what is in it, but
  how many frames, how big, and when — which is enough to know when you are
  working and roughly how much.
- **A command reaches the machine only while it is connected.** Nothing is
  queued for a desktop that is offline; the phone is told so instead.
- **Sessions the daemon runs share a process.** They have their own plans,
  their own background jobs and their own subagents, and cannot reach each
  other's — but they are one process with one set of environment variables and
  one API key, and a tool in one can read any file the others can.

The connect signature goes in a header rather than in the URL, so it is not
kept by whatever logs requests, and the relay refuses a second use of the same
one: a signature that did leak opens nothing, rather than opening a socket for
as long as the clocks allow. Be precise about whose property that is. Neither
`ah` nor the phone app ever puts a signature in a URL, but the relay still
reads one from an `h` query parameter when the header is absent, so that a
peer built before the header existed goes on connecting. It is a habit both
halves of this project keep, not a rule the relay enforces.

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
and its device list, closes both sockets, and marks the name revoked. From
that moment the old code opens nothing — the hub turns away every connection
and refuses to be paired again.

That mark is not quite permanent, and it is better to say so. A hub nobody has
connected to for thirty days deletes itself whole — log, devices, key and the
revoked mark in one go — because storage is the thing the free plan is
actually spent on and a pairing untouched for a month is one nobody is coming
back to. The name is free again afterwards. Nobody can do anything with it
who could not have done it anyway: making a pairing there takes the relay's
provisioning secret, and making that pairing be the old code again takes the
old code as well, which is two secrets that between them could always have
made a pairing from scratch. What is gone after thirty days is the relay's
memory of the revocation, not the protection the revocation gave.

A relay that will not confirm the revocation stops the command rather than
skipping it, because a pairing forgotten only on this side is one that still
answers whoever holds the code — `--local` forgets it here anyway and says
what that leaves behind.

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
| `trust_paired_device` | `false` | let sessions the daemon starts run tools without asking. Off does not mean the phone is distrusted: it means the phone is asked, and the phone answers |
| `max_sessions` | `4` | sessions the daemon holds open at once |
| `allow_sudo` | `false` | let sessions the daemon starts run `sudo`, with the password asked for once at startup. It is about the password prompt rather than the privilege: off, a command whose first word names one of `sudo`, `doas`, `pkexec` or `su` is refused, and a shell string that hides the word is not |

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
| `the relay refused the pairing` | the relay was redeployed and no longer holds this pairing, the code is from an older one, or the signature did not match for some other reason. The relay answers all three the same way on purpose, so that somebody guessing at hub names cannot learn which of them are real. Pair again. |
| `the relay has never heard of this pairing` | nothing answered on the hub route at all, so that URL is not a relay. `ah remote status` says which one is being dialled. |
| `a desktop is already connected` | another `ah` on this machine, or another machine, holds the pairing. It gives way once it goes quiet. |
| `the pairing was revoked` | `ah remote forget` was run somewhere. Pair again. |
| `the relay and this machine disagree about the time` | a clock is more than five minutes out. It is the only place a clock matters. |

## Costs

Nothing, within Cloudflare's free plan, and not close to the edges of it. Text
from the model is gathered for 200 ms at a time rather than sent token by
token, so a two-minute turn is a few hundred frames rather than tens of
thousands — around 30 billable requests against a daily 100,000.

## The wire protocol

Everything above is what the link is for. This is what it is made of, for
somebody writing a second phone against it, or checking that the one that
exists does what it says. The names used here are the ones in
`crates/ah-remote-proto`, which the harness and the relay both compile, so
every number below has exactly one place it comes from.

### Frames

One WebSocket per peer, to `/hub/<hub id>`, and text only: a binary frame is
closed on with 1003, and a frame over 128 KiB with 1009 before it is parsed,
since a Durable Object will store a two-megabyte row and no honest frame is
near that. Every frame is a JSON object with its tag in `t` and the protocol
version in `v`. The version is 1, and the relay refuses a frame naming
another whole, with a 1002 close, rather than reading part of it.

| `t` | Direction | Fields besides `v` |
|---|---|---|
| `pub` | desktop → relay | `link`, `seq`, `ct` |
| `cmd` | phone → relay → desktop | `link`, `plink`, `seq`, `ct` |
| `evt` | relay → phone | `link`, `seq`, `ct`, `n` |
| `sub` | phone → relay | `since`, `max` |
| `ka` | desktop → relay | — |
| `gap` | relay → phone | `from` |
| `ctl` | relay → either | `e`, and sometimes `server_ms` |

`ct` is the sealed payload and the only field with anything in it worth
reading; the rest is what a router needs. `n` is the number the relay stored
a frame under, and the only cursor a phone keeps. `sub` is the one control
the relay itself acts on, which is why it is the one thing not sealed.

`ctl` carries one of `offline`, `skew`, `replaced`, `revoked` or `quota`, and
two of those five never arrive. `replaced` is not sent: a desktop that has
been replaced is closed on with 4010 instead. `skew` is not sent either,
because a signature the two clocks disagree about is refused during the
handshake, before there is a socket to send anything on, with HTTP 401 and a
body of `{"e":"skew","server_ms":…}` — which is also the only place
`server_ms` ever appears, so that field never travels over the socket at all.
Both are kept in the list as reserved rather than deleted, so a reader written
against this page is not surprised if they ever begin to. One quirk worth
knowing: a frame the relay cannot parse is answered with `offline`, which is
the only way it has of saying that something went nowhere.

### The key ladder

HKDF-SHA256, as in RFC 5869, run twice. The salts and info strings are bytes
rather than words about bytes, because a stray space in one is a 401 or a
decrypt failure with nothing to debug.

The pairing code is twenty random bytes. Extract with the salt `ah-remote v1`
over those, then expand four times:

| Info | Bytes | What it becomes |
|---|---|---|
| `hub` | 16 | the hub id, written as 32 lowercase hex characters |
| `relay` | 32 | the only key the relay is ever given |
| `d2p` | 32 | the base for everything the desktop seals |
| `p2d` | 32 | the base for everything a phone seals |

HKDF expansions are independent, so the key the relay holds says nothing
about the two it does not. The hub id being derived rather than random is
what lets a phone find the hub from the code alone.

Each connection then draws a link id: sixteen random bytes, written as 32 hex
characters in the envelope. The desktop draws one every time its socket
opens, and a phone draws a `plink` of its own. Extract a second time, with
the salt `ah-remote link v1` over the direction's base, and expand once:

| Direction | Info, in this order | Direction byte |
|---|---|---|
| desktop to phone | `d2p`, link | 0 |
| phone to desktop | `p2d`, link, plink | 1 |

The asymmetry there is deliberate and load-bearing. The desktop's key binds
only its own link, because every phone attached to the pairing has to be able
to open what it sends; a phone's key binds both links, so two phones never
share a key, and neither do two runs of the same phone.

### The seal

AES-256-GCM with a 128-bit tag appended to the ciphertext, the two written
together as base64url with no padding. That is `ct`.

The nonce is the frame's sequence number: twelve bytes, four zeros and then
`seq` as a big-endian `u64`. The key is already per-connection, so a random
prefix would add nothing, and a nonce that can be recomputed from the
envelope is one fewer thing that can arrive wrong.

The additional data is 41 bytes, fixed widths and no separators:

| Bytes | What |
|---|---|
| 0–15 | the link id |
| 16–31 | the `plink`, or sixteen zeros |
| 32 | the direction byte |
| 33–40 | `seq`, big-endian `u64` |

A `pub` or an `evt` passes no `plink` on either side, so those sixteen bytes
are zeros when it is sealed and zeros when it is opened, whatever `plink` the
two ends happen to be holding. Between the direction byte and the link id, a
frame reflected back at its sender does not open, and neither does one
relabelled as coming from somewhere it did not.

### Sequence numbers

A `u64`, first frame numbered 1, going up by one, never restarted under a key
that has been used. A reader refuses a number it has already seen, and
refuses it before it tries the cipher, so a flood of replayed frames costs a
flood of integer comparisons rather than a flood of decryptions. A gap is
accepted, since the relay may have trimmed what was in between. The window
moves only once a frame has actually opened, so nobody can wedge a link shut
by injecting one with a high number.

All of that rests on a precondition, and in practice it is the precondition
rather than the rule that gets broken: one sealer per key, counting once.
That is why the desktop draws a fresh link id every time its socket opens
instead of carrying on from where it was, and why a phone that is ever going
to start counting from 1 again has to draw a fresh `plink` first. A peer
that reconnects, keeps its key and restarts its count has used one AES-GCM
nonce twice, which is a break the relay can see and neither end can.

### Getting a socket open

An upgrade to `/hub/<hub id>?r=<desk|phone>&ts=<milliseconds>&n=<nonce>`,
with the signature in `x-ah-auth`. What is signed is one line:

```
ah/v1 connect|<hub id>|<role>|<ts>|<nonce>
```

HMAC-SHA256 under the relay key, base64url with no padding. The nonce is
twelve random bytes written the same way, sixteen characters; the relay takes
anything from 1 to 64 characters and remembers a spent one for ten minutes,
twice the skew window, so a signature is too old to use before the relay
forgets that it was used. `/hub/<hub id>/revoke` is signed the same way, with
the role `desk`, because it is the same proof: knowing the code.

The relay checks the signature first and the clock second. Answered the other
way round, the endpoint would tell anybody at all what the relay thinks the
time is and which timestamps it will accept. Five minutes either way is the
allowance, and it is the only place in the protocol where a clock matters.

What comes back when it does not open:

| Status | Meaning |
|---|---|
| 400 | the name in the path is not 32 lowercase hex characters |
| 401 | everything refused: a hub with no pairing on it, a signature that did not match, a nonce already spent, and — with `{"e":"skew","server_ms":…}` as the body — a clock too far out |
| 409 | a desktop is connected and has been heard from recently |
| 410 | the pairing was revoked |
| 429 | this hub's connections for the day are spent |

One 401 for three different refusals is the point of it, not an oversight: a
relay that answered "no such hub" to one and "no" to another would tell
somebody working through a list of guessed names which of them were real. A
clock out of step is the exception, and only because it is fixable, looks like
nothing else, and is told only to somebody who has already proved they hold
the key.

### Close codes

| Code | Who gets it |
|---|---|
| 4009 | a phone, closed to make room for a fifth |
| 4010 | a desktop, replaced after having gone silent |
| 4012 | either, on revocation |

4011 is defined and never sent. A second desktop arriving while the first is
still talking is turned away during the handshake with a 409, so there is
never a socket to close it on. The ordinary codes turn up as well: 1002 for a
version this relay does not speak, 1003 for a binary frame, 1009 for one too
big, and 1011 for a socket the relay has lost its own record of.

### What the relay keeps, and for how long

| Bound | Value |
|---|---|
| frames kept per hub | 20,000 |
| ciphertext kept per hub | 64 MiB |
| how long a frame is kept regardless of the other two | 7 days |
| frames one hub may take in a day | 40,000 |
| sockets one hub may open in a day | 2,000 |
| phones attached at once | 4 |
| how long a device row outlives the last sight of that device | 30 days |
| largest frame accepted at all | 128 KiB |

The three log bounds are enforced together, in one pass every 256 frames,
because a count on its own is not a bound: twenty thousand frames of the
largest size the relay will take would be two and a half gigabytes, and
storage is the thing being paid for. It is done in batches rather than on
every insert because a deleted row costs the same as a written one: trimming
per frame would double what a frame costs.

Past those, nothing fails quietly. A phone asking for something older than
the oldest kept frame is sent a `gap` naming the oldest number there still
is, draws a rule in its transcript and asks the desktop for a fresh snapshot.
A desktop past the day's frames is sent `ctl quota` rather than having its
frames dropped without a word. A fifth phone does not queue: the one heard
from longest ago is closed with 4009.

A phone asks for what it missed with `sub`, naming the last number it stored
and how many frames it wants back. The relay clamps the count to between 1
and 500 and answers at most one `sub` per second per socket, because a replay
is the one thing a phone can ask for that costs hundreds of row reads and
costs the asker nothing. A `sub` asking for something earlier than that same
socket has already been given is ignored.

The desktop sends `ka` every two minutes when it has nothing else to say, and
a frame it does send counts instead. Six minutes of silence — three
keepalives — is the point at which the relay will let a second desktop take
the pairing over; below that the newcomer is the one turned away, so a
working session is never knocked off the air by whoever connects next.
Protocol pings do not count towards it, because the runtime answers those
without the hub ever waking, which is the whole reason the keepalive is a
frame of its own.
