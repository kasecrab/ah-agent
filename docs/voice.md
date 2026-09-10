# Dictation

`/voice` arms dictation. Hold the talk key — Space by default — and speak. The
words appear in the input box in grey as they come back, and turn white when
you let the key go. Press the key again and it carries on from there. Nothing
is ever sent on its own: Enter still sends, exactly as it always did.

```
/voice              arm or disarm
/voice off          disarm
/voice model        pick the model that transcribes
/voice model <id>   set it directly
/voice devices      pick the microphone
```

`alt-v` does the same as bare `/voice`. `Esc` drops a phrase that has not been
committed yet; `/voice` or `alt-v` disarms.

**The talk key still types.** A press and a quick release is an ordinary
keystroke, so the space bar goes on typing spaces while dictation is armed;
only holding it past `voice.dwell_ms` starts listening. When a hold is
recognised, the keystrokes it had already produced are taken back out of the
input box, so nothing is left behind.

## Who transcribes

The first `/voice` asks, and remembers the answer in `voice.provider`.

**Deepgram** opens a WebSocket and keeps it open while dictation is armed.
Audio goes up as it is recorded and words come back while you are still
speaking — a guess first, corrected in place, then settled. It needs a
Deepgram key of its own, which `ah` asks for at a prompt that shows dots
rather than characters and writes to `~/.config/ah/credentials.toml` at mode
600. `DEEPGRAM_API_KEY` in the environment works instead. Billed by the minute
of audio you actually send.

**OpenRouter** uses the key `ah` already has and any model that takes audio.
A phrase is sent when you pause, so words appear a second or so behind you.
Nothing new to sign up for.

`/voice` on its own switches between them by asking again. Whichever you pick
is remembered in `~/.config/ah/state.toml`, so a new window already knows —
which is also what lets it dial ahead of you.

So is the switch itself. Dictation left on is on again in the next window, with
no `/voice` to type; left off it stays off. Arming opens no microphone — that
waits for the talk key — so a window that comes up armed costs one thread and
the connection it would have made anyway. Bear in mind that while it is on the
talk key belongs to dictation and will not type a space, except on a line
beginning with `/`; the chip above the input says so for as long as it lasts.

## Where the audio goes

Off the machine, on either route: to Deepgram, or to OpenRouter and on to
whichever provider serves the model you picked. That is the trade for having
no local model and nothing extra resident. `ah` says so once, the first time
you use it.

What it does not do is keep the audio. A dictated phrase is built, sent and
dropped. It is never added to the conversation, never written to a session
file, and never logged — `AH_LOG` records the size of a clip and nothing else.

`voice.enabled = false` refuses `/voice` outright and never opens the device.

## Picking a model

On Deepgram, `/voice model` lists Deepgram's own: `nova-3` by default, with
the older and the specialised ones under it. They are billed by the minute of
audio rather than by the token.

On OpenRouter it lists two kinds of model, and `ah` sends a phrase to
whichever one the model itself calls for.

**Chat models that hear** — Gemini, Voxtral small and the rest. The phrase goes
to `/chat/completions` as an `input_audio` part. The reply streams, so the grey
text grows word by word, and the sentence so far rides along so the second half
knows about the first. The audio price is shown per million tokens; a spoken
minute is about 1,900 of them.

**Speech-to-text models**, marked `transcribes` — whisper, nova-3, gpt-4o-transcribe,
parakeet, qwen3-asr. These answer on `/audio/transcriptions`, which is what
they were built for: no instructions to pay for on every phrase and better
accuracy for the money. In exchange the endpoint has no stream and no prompt
field, so a phrase appears whole rather than a word at a time, and
`voice.prompt_append` and the sentence-so-far context do not reach it — `ah`
says so when you arm one.

Their price column reads `billed per audio` rather than a per-token figure,
because each provider means a different unit by it — per second, per minute,
per hour. `/usage` shows what a phrase actually cost, which the endpoint
reports itself.

OpenRouter leaves speech-to-text models out of its unfiltered model list, so
`ah` asks for them separately and merges the two. If none appear, Ctrl-R in
`/voice model` refetches.

## How it stays quick

On Deepgram there is nothing to arrange: the socket is already open, audio
flows as it is recorded, and the first words land in about a third of a
second. The local voice detector does not run at all — Deepgram finds the
phrase boundaries itself, so nothing is cut here and nothing waits for a
pause. Letting the talk key up asks for whatever is still held rather than
waiting out the silence.

The handshake takes well over a second, so it is never paid at the moment you
start speaking. The socket is dialled as soon as `ah` has a window up, before
`/voice` is even typed, whenever Deepgram is already the chosen provider and a
key is already stored — and it is held for as long as dictation is armed. An
open connection carrying only `KeepAlive` sends no audio and is not billed, so
holding it costs nothing; `voice.idle_secs` drops it after that many idle
seconds if you would rather it did not linger.

Dialling never touches startup: it happens after the first frame is drawn, and
on a thread, so a slow or missing network cannot delay the prompt appearing.

On OpenRouter there is no streaming transcription protocol behind a chat
model, so `ah` makes its own. A voice detector on this machine watches the level, cuts the
audio where you pause, and sends that phrase on its own while you carry on
talking — up to `voice.max_inflight` at a time, on one connection that is
opened when you arm and kept. Each reply streams, so the grey text grows word
by word rather than arriving in a lump.

From the end of a phrase to the first grey character is roughly a second: the
pause `ah` waits out (`voice.phrase_ms`), the upload, and the model's own time
to first token. A monologue with no pauses in it is cut anyway every
`voice.max_chunk_ms`.

`voice.mode` trades the two against each other:

| mode | pause | forced cut | at once |
|---|---|---|---|
| `fast` | 300 ms | 2 s | 3 |
| `balanced` | 400 ms | 3.5 s | 2 |
| `cheap` | 700 ms | 8 s | 1 |

Shorter phrases reach the screen sooner and cost more, because each one is a
request carrying its own instructions. The audio itself costs the same either
way.

## What it costs, and how to see it

Deepgram bills by the minute of audio sent, and audio is only sent while the
talk key is down — a socket sitting open with nobody talking into it costs
nothing. `/usage` shows how much audio a session has sent.

On OpenRouter every phrase reports its real cost, so nothing has to be
guessed:

- `voice.show_cost = true` puts a running figure next to the recording mark.
- `/usage` gains a **voice** row: what dictation has cost this session and how
  many phrases it took.
- `voice.budget_usd` stops a dictation once it crosses that much.

Silence never becomes a request on the OpenRouter route. A phrase has to hold at least a quarter of a
second of speech above the room's own noise floor before it is sent, so a
cough, a door, or a held key in a quiet room costs nothing at all. When a model
fills silence with a stock phrase anyway — "thanks for watching" and its
relatives — `voice.filter` drops it, along with the repeat loops models fall
into when they lose their place.

A phrase that fails is retried once, immediately, and only if nothing had come
back yet. Then it is given up on: a retry costs the whole phrase again, and a
slow one costs the moment it was for. The words around it still arrive in the
order you said them.

## Accuracy

`voice.prompt_append` is the one setting worth filling in. Put the words a
model would otherwise get wrong — project names, identifiers, the people you
work with — and they are sent with every phrase:

```toml
[voice]
prompt_append = "ah, ratatui, wasmi, OpenRouter, kasecrab"
```

The last 120 characters already in the input box go with each phrase too, so
the second half of a sentence knows about the first.

## The microphone

By default `ah` opens the system input itself and asks for 16 kHz mono, which
is what a transcriber wants and a sixth of the bytes of 48 kHz stereo. If the
device refuses, `ah` takes what it offers and converts.

The microphone is opened when the talk key goes down and closed when it comes
up — never merely because dictation is armed. Opening it was measured at 23 ms,
with the first sample 37 ms later, which is far too little to be worth holding
a microphone open for. On a Bluetooth headset it matters more than the
milliseconds: an open microphone pins the earpieces in their call profile, and
that costs you playback quality for as long as it is held.

`voice.keep_open = true` holds it open for as long as dictation is armed, if
you would rather have those 60 ms back.

Because nothing is opened until you speak, a missing or refused microphone is
reported on the first hold rather than when you arm.

Over ssh, in a container, or in a build without the `mic` feature, there is no
device to open. `ah` then looks for `pw-record`, `parec`, `arecord`, `ffmpeg`
and `sox`, in that order, and says what it looked for if it finds none.
`voice.capture_cmd` names one directly:

```toml
[voice]
capture_cmd = "pw-record --rate 16000 --channels 1 --format s16 -"
```

It has to print raw signed 16-bit little-endian mono PCM on stdout. Because it
is a shell command, it is read from `~/.config/ah/config.toml` or
`AH_VOICE_CAPTURE_CMD` only — a project's `.ah/config.toml` cannot set it.

## The talk key on your terminal

Holding a key looks different depending on what the terminal reports, so `ah`
works it out rather than assuming.

- **A release is reported.** Holding past `voice.dwell_ms` (180 ms) starts
  listening; the release stops it. A press and release inside that window is a
  keystroke and types.
- **No release.** A held key arrives as the same key pressed again and again,
  at whatever rate the keyboard repeats, and nothing at all arrives between
  the press and the first repeat. So a hold cannot be recognised on a clock
  here: the giveaway is two key events closer together than a person could
  type them, which is the second repeat. That lands about half a second in,
  and the one or two spaces typed before it are taken back out. Once a hold is
  under way a silence ends it — starting at `voice.release_grace_ms`, then
  narrowing to a fraction of the observed repeat rate.

  This is the case where `enable_kitty_keyboard = true` is worth having: with
  releases reported, a hold is recognised in 180 ms and nothing is ever typed
  and taken back.

Either way a held key is one phrase. It is never treated as a series of
presses that turn dictation on and off.

Key releases need the kitty keyboard protocol. Most terminals that support it
have it on; **WezTerm does not**, so add this to `wezterm.lua` if you want the
exact version rather than the gap-timed one:

```lua
enable_kitty_keyboard = true,
```

`voice.hotkey_mode` overrides the lot: `push_to_talk` insists on releases (and
still falls back to the gap if none arrive, rather than never stopping), and
`toggle` makes the key start and stop dictation on alternate presses, bounded
by `voice.max_listen_secs`.

## What it costs when it is off

Nothing. No thread, no buffer, no open device — `/voice` is the first thing
that touches any of it. Armed and quiet, one thread wakes five times a second
to throw away audio nobody asked for. Every setting lives under `[voice]`; see
`ah docs config`.
