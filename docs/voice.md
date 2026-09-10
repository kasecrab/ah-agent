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

`alt-v` does the same as bare `/voice`. While dictation is armed the talk key
belongs to it, so it does not type a space — except on a line that starts with
`/`, where it still does, or `/set voice.mode cheap` could not be typed.
`Esc` drops a phrase that has not been committed yet; `/voice` or `alt-v`
disarms and gives the key back for good.

## Where the audio goes

To OpenRouter, and on to whichever provider serves the model you picked. This
is the trade: there is no second account, no API key beyond the one `ah`
already has, no model to download and nothing extra resident — and the
recording leaves the machine. `ah` says so once, the first time you use it.

What it does not do is keep the audio. A dictated phrase is built, sent and
dropped. It is never added to the conversation, never written to a session
file, and never logged — `AH_LOG` records the size of a clip and nothing else.

`voice.enabled = false` refuses `/voice` outright and never opens the device.

## Picking a model

`/voice model` lists two kinds of model, and `ah` sends a phrase to whichever
one the model itself calls for.

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

There is no streaming transcription protocol behind a chat model, so `ah`
makes its own. A voice detector on this machine watches the level, cuts the
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

Every phrase reports its real cost, so nothing has to be guessed:

- `voice.show_cost = true` puts a running figure next to the recording mark.
- `/usage` gains a **voice** row: what dictation has cost this session and how
  many phrases it took.
- `voice.budget_usd` stops a dictation once it crosses that much.

Silence never becomes a request. A phrase has to hold at least a quarter of a
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

The device is opened when you arm, not when you press the talk key: opening it
takes long enough to swallow the first syllable of every phrase.
`voice.keep_open = false` closes it between phrases and accepts that.

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

Holding a key is only visible if the terminal reports the release, which needs
the kitty keyboard protocol. `ah` works out what it has on the first press:

1. A release arrives — hold to talk, as intended.
2. No release, but repeats while held — a gap in the repeats ends the phrase.
3. Neither — the key toggles instead, and `ah` says so once.

`voice.hotkey_mode` forces one of `push_to_talk` or `toggle` instead of
working it out. In toggle mode `voice.max_listen_secs` stops a microphone left
on by accident.

## What it costs when it is off

Nothing. No thread, no buffer, no open device — `/voice` is the first thing
that touches any of it. Armed and quiet, one thread wakes five times a second
to throw away audio nobody asked for. Every setting lives under `[voice]`; see
`ah docs config`.
