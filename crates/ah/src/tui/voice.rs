//! Dictation, from the TUI's side: what is on screen, what is in flight, and
//! what it has cost.
//!
//! The crate below turns a held key into WAV clips. This turns each clip into
//! an ordinary streaming request against a model that takes audio input, and
//! puts the words in the input box — grey while the key is down, white when it
//! comes up. Nothing is ever sent on its own.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ah_abi::{ChatRequest, Message, VoiceSettings};
use ah_core::provider::openrouter::OpenRouter;
use ah_core::provider::{Provider, StreamEvent};
use ah_voice::deepgram;

use super::Msg;

/// Everything opening a dictation session needs that is not a setting.
pub struct Arm {
    pub model: String,
    /// The model answers on the transcription endpoint rather than as a chat.
    pub stt: bool,
    pub provider: Arc<OpenRouter>,
    pub capture_cmd: String,
    /// Sticky-routing key, so every phrase lands on one upstream.
    pub route: String,
    /// A socket dialled ahead of time, if there is one that fits.
    pub warm: Option<Warm>,
}

/// A Deepgram socket dialled before anybody asked to dictate, so the first
/// phrase does not pay for the handshake. It carries the ring the microphone
/// will eventually fill, and the settings it was opened with, because a
/// socket opened for one model is no use for another.
pub struct Warm {
    pub live: deepgram::Live,
    producer: ah_voice::ring::Producer,
    model: String,
    language: String,
    keyterms: Vec<String>,
}

impl Warm {
    /// Open the socket. Returns as soon as the thread is spawned: the dialling
    /// itself happens on that thread, so this never blocks the caller.
    pub fn open(cfg: &VoiceSettings, tx: Sender<Msg>) -> Result<Self, String> {
        let key = ah_core::auth::deepgram_key().ok_or("no Deepgram API key")?;
        let model = cfg.model_for();
        let keyterms = keyterms(cfg);
        // Two seconds of headroom between the microphone thread and the
        // socket thread, which is far more than either needs.
        let (producer, consumer) =
            ah_voice::ring::ring(ah_voice::resample::TARGET_RATE as usize * 2);
        let live = deepgram::Live::open(
            deepgram::Config {
                api_key: key,
                model: model.clone(),
                language: cfg.language.clone(),
                keyterms: keyterms.clone(),
                sample_rate: ah_voice::resample::TARGET_RATE,
                endpointing_ms: cfg.dials().0,
                idle_secs: cfg.idle_secs,
            },
            consumer,
            move |e| {
                let _ = tx.send(Msg::Voice(Event::Live(e)));
            },
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            live,
            producer,
            model,
            language: cfg.language.clone(),
            keyterms,
        })
    }

    /// True when this socket was opened for what is being asked for now. A
    /// changed model or language means the connection has to be redialled.
    pub fn fits(&self, cfg: &VoiceSettings) -> bool {
        self.model == cfg.model_for()
            && self.language == cfg.language
            && self.keyterms == keyterms(cfg)
    }
}

/// `prompt_append` as the terms Deepgram should listen out for.
fn keyterms(cfg: &VoiceSettings) -> Vec<String> {
    cfg.prompt_append
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// What comes back from the worker and from the requests it starts.
pub enum Event {
    /// A phrase was recorded and is ready to be transcribed.
    Phrase(ah_voice::Phrase),
    Delta {
        seq: u64,
        text: String,
    },
    Done {
        seq: u64,
        cost: f64,
    },
    Failed {
        seq: u64,
        message: String,
    },
    /// Something Deepgram's socket said.
    Live(deepgram::Event),
}

/// How the talk key behaves here, which is not a matter of opinion but of
/// what the terminal reports.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hold {
    /// Not decided yet: waiting to see whether a release ever arrives.
    Probing,
    /// Releases arrive. Press starts, release stops, exactly.
    Push,
    /// No releases: the key repeating is what says it is still down, and a
    /// gap in the repeats is what says it is not.
    Gap,
    /// Asked for by hand. Press starts, press again stops.
    Toggle,
}

impl Hold {
    fn from_setting(s: &str) -> Self {
        match s {
            "push_to_talk" => Hold::Push,
            "toggle" => Hold::Toggle,
            _ => Hold::Probing,
        }
    }
}

/// The shortest gap that can mean "let go". Below this a repeat rate of a few
/// hundred a second would end a hold between its own keystrokes.
const MIN_GAP: Duration = Duration::from_millis(90);
/// A repeat has to be this much quieter than the gap for the gap to be sure.
const GAP_FACTOR: u32 = 3;
/// Two key events closer together than this are a keyboard repeating, not a
/// person typing. Nobody taps the same key twice in a tenth of a second.
const REPEAT_MAX: Duration = Duration::from_millis(120);
/// A tap that was never followed by anything is over, and the keystrokes it
/// produced are the user's to keep.
const TAP_OVER: Duration = Duration::from_millis(900);

/// What the talk key just asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// A hold has been recognised. `undo` is how many keystrokes already went
    /// into the input box while it was still ambiguous, and have to come back
    /// out before the words arrive.
    Start {
        undo: usize,
    },
    Stop,
    Nothing,
    /// Not a hold, or not yet: let the key through and type it.
    Type,
}

/// The talk key's own state, kept apart from the microphone and the socket so
/// that the part which decides when a key is down can be tested without
/// either. It is the part that went wrong.
struct Talk {
    hold: Hold,
    /// A release has been seen, so this terminal reports them and nothing
    /// needs timing.
    saw_release: bool,
    /// Last press or repeat.
    last: Option<Instant>,
    /// Silence that currently counts as letting go.
    gap: Duration,
    /// What the gap is before the repeat rate has been measured.
    first_gap: Duration,
    /// Keystrokes typed while it was still unclear whether the key was being
    /// tapped or held. They are real until a hold proves otherwise.
    pending: usize,
    /// True between a press and its release, on terminals that report one.
    down: bool,
    /// How long the key has to stay down before it counts as held rather than
    /// typed.
    dwell: Duration,
}

impl Talk {
    fn new(mode: &str, first_gap_ms: u64, dwell_ms: u64) -> Self {
        let first_gap = Duration::from_millis(first_gap_ms.max(MIN_GAP.as_millis() as u64));
        Self {
            hold: Hold::from_setting(mode),
            saw_release: false,
            last: None,
            gap: first_gap,
            first_gap,
            pending: 0,
            down: false,
            dwell: Duration::from_millis(dwell_ms),
        }
    }

    fn press(&mut self, now: Instant, listening: bool) -> Act {
        self.down = true;
        let apart = self.last.map(|prev| now.saturating_duration_since(prev));
        self.last = Some(now);

        // A toggle is a switch, not a hold: it flips on every press, and
        // nothing about dwell or repeats applies to it.
        if self.hold == Hold::Toggle {
            if listening {
                return Act::Stop;
            }
            self.gap = self.first_gap;
            return Act::Start { undo: 0 };
        }

        if listening {
            // The key is still down and the words are already coming. A
            // second event during a hold is the keyboard repeating, which is
            // the only thing that says how long a silence has to be before it
            // means the key came up. Half a second passes before a keyboard
            // starts repeating, so this can be measured but never assumed.
            if let Some(apart) = apart
                && apart < self.gap
            {
                self.gap = (apart * GAP_FACTOR).clamp(MIN_GAP, self.first_gap);
            }
            return Act::Nothing;
        }

        // Two events this close together are a keyboard repeating, not a
        // person typing: the key is being held. Take back the spaces that
        // went in while that was still in doubt.
        if apart.is_some_and(|a| a <= REPEAT_MAX) {
            self.gap = self.first_gap;
            let undo = std::mem::take(&mut self.pending);
            return Act::Start { undo };
        }

        // A press on its own says nothing yet. Let it type, and decide when
        // either a release arrives (a tap) or it does not (a hold).
        if apart.is_none_or(|a| a > TAP_OVER) {
            self.pending = 0;
        }
        self.pending += 1;
        Act::Type
    }

    fn release(&mut self, listening: bool) -> Act {
        self.saw_release = true;
        self.down = false;
        if self.hold == Hold::Probing || self.hold == Hold::Gap {
            self.hold = Hold::Push;
        }
        if listening && self.hold != Hold::Toggle {
            return Act::Stop;
        }
        // Let go before it counted as a hold: the keystrokes it produced were
        // meant, and stay.
        self.pending = 0;
        Act::Nothing
    }

    fn tick(&mut self, now: Instant, listening: bool) -> Act {
        if !listening {
            // Held past the dwell without a release: that is a hold, and the
            // keystrokes it produced were not meant. Only a terminal that has
            // proved it reports releases can be believed about the key still
            // being down; without one, the keyboard repeating is the only
            // evidence there is, and it arrives on its own.
            if self.hold != Hold::Toggle
                && self.saw_release
                && self.down
                && self.pending > 0
                && self
                    .last
                    .is_some_and(|l| now.saturating_duration_since(l) >= self.dwell)
            {
                self.gap = self.first_gap;
                let undo = std::mem::take(&mut self.pending);
                return Act::Start { undo };
            }
            return Act::Nothing;
        }
        if self.hold == Hold::Toggle {
            return Act::Nothing;
        }
        // A terminal that reports releases needs no clock: one will come.
        if self.hold == Hold::Push && self.saw_release {
            return Act::Nothing;
        }
        let Some(last) = self.last else {
            return Act::Nothing;
        };
        if now.saturating_duration_since(last) <= self.gap {
            return Act::Nothing;
        }
        if self.hold == Hold::Probing {
            self.hold = Hold::Gap;
        }
        Act::Stop
    }

    /// True when only a silence can tell us the key came up.
    fn gap_timed(&self) -> bool {
        self.hold != Hold::Toggle && !(self.hold == Hold::Push && self.saw_release)
    }
}

#[derive(Default)]
struct Chunk {
    text: String,
    done: bool,
}

pub struct Session {
    dictation: ah_voice::Dictation,
    provider: Arc<OpenRouter>,
    model: String,
    /// The model answers on `/audio/transcriptions` rather than as a chat.
    /// It is better at the job and cheaper, but it hands back the whole
    /// phrase at once and takes no context, so both paths are kept.
    stt: bool,
    /// Deepgram's socket, when it is the one listening. Everything below it
    /// belongs to that route and is untouched by the other two.
    live: Option<deepgram::Live>,
    /// Words the socket has settled on during this hold.
    live_said: String,
    /// The word or two still being decided. Replaced wholesale each time.
    live_guess: String,
    /// When the talk key came up, so the last words have a moment to land
    /// before the grey text is committed.
    released: Option<Instant>,
    /// Audio actually sent, which is what Deepgram bills for.
    pub audio_ms: u64,
    cfg: VoiceSettings,
    max_inflight: usize,
    /// Sticky-routing key, so every phrase of one dictation lands on the same
    /// upstream instead of paying a cold start each time.
    route: String,
    chunks: BTreeMap<u64, Chunk>,
    waiting: VecDeque<ah_voice::Phrase>,
    inflight: usize,
    pub cost: f64,
    pub requests: u32,
    listen_since: Option<Instant>,
    talk: Talk,
    pub note: Option<String>,
    /// What is already in the input box, so a phrase can be told where in the
    /// sentence it belongs.
    context: String,
    tx: Sender<Msg>,
}

impl Session {
    pub fn arm(cfg: &VoiceSettings, a: Arm, tx: Sender<Msg>) -> Result<Self, String> {
        let Arm {
            model,
            stt,
            provider,
            capture_cmd,
            route,
            warm,
        } = a;
        let (phrase_ms, max_chunk_ms, max_inflight) = cfg.dials();
        let audio = ah_voice::Config {
            device: cfg.device.clone(),
            capture_cmd,
            keep_open: cfg.keep_open,
            sample_rate: cfg.sample_rate,
            ring_ms: cfg.ring_ms,
            phrase_ms,
            max_chunk_ms,
            preroll_ms: cfg.preroll_ms,
            speech_ratio: cfg.speech_ratio,
        };
        // Deepgram listens continuously and finds the phrase boundaries
        // itself, so on that route the audio is streamed rather than cut, and
        // the local voice detector never runs.
        let (live, out) = if cfg.live() {
            // A socket dialled at startup is already up by now; one that does
            // not match what is being asked for is no use and is dropped.
            let warm = match warm {
                Some(w) if w.fits(cfg) => w,
                _ => Warm::open(cfg, tx.clone())?,
            };
            let Warm { live, producer, .. } = warm;
            (Some(live), ah_voice::Output::Live(producer))
        } else {
            let hand_tx = tx.clone();
            (
                None,
                ah_voice::Output::Phrases(Box::new(move |p| {
                    let _ = hand_tx.send(Msg::Voice(Event::Phrase(p)));
                })),
            )
        };
        let dictation = ah_voice::Dictation::arm(&audio, out).map_err(|e| e.to_string())?;
        Ok(Self {
            dictation,
            provider,
            model,
            stt,
            live,
            live_said: String::new(),
            live_guess: String::new(),
            released: None,
            audio_ms: 0,
            cfg: cfg.clone(),
            max_inflight,
            route,
            chunks: BTreeMap::new(),
            waiting: VecDeque::new(),
            inflight: 0,
            cost: 0.0,
            requests: 0,
            listen_since: None,
            talk: Talk::new(&cfg.hotkey_mode, cfg.release_grace_ms, cfg.dwell_ms),
            note: None,
            context: String::new(),
            tx,
        })
    }

    /// A microphone that would not open. Only found out about when the talk
    /// key goes down, because that is the only time one is reached for.
    pub fn mic_trouble(&self) -> Option<String> {
        self.dictation.take_trouble()
    }

    pub fn listening(&self) -> bool {
        self.listen_since.is_some()
    }

    pub fn level(&self) -> f32 {
        self.dictation.level()
    }

    /// Anything on screen or on the wire that has not been committed yet.
    pub fn busy(&self) -> bool {
        self.listening()
            || !self.chunks.is_empty()
            || !self.waiting.is_empty()
            || !self.live_said.is_empty()
            || !self.live_guess.is_empty()
    }

    // ---- the talk key ----------------------------------------------------

    /// Returns what the key turned out to mean, so the caller can put right
    /// what it typed while that was still unknown.
    pub fn press(&mut self) -> Act {
        let listening = self.listening();
        let a = self.talk.press(Instant::now(), listening);
        self.act(a);
        a
    }

    /// A repeat says the same thing a press does here: the key is still down.
    pub fn repeat(&mut self) -> Act {
        self.press()
    }

    pub fn release(&mut self) -> Act {
        let a = self.talk.release(self.listening());
        self.act(a);
        a
    }

    fn act(&mut self, a: Act) {
        match a {
            Act::Start { .. } => self.start_listening(),
            Act::Stop => self.stop_listening(),
            Act::Nothing | Act::Type => {}
        }
    }

    fn start_listening(&mut self) {
        self.listen_since = Some(Instant::now());
        self.released = None;
        self.dictation.listen(true);
        if let Some(l) = &self.live {
            l.listen(true);
        }
    }

    fn stop_listening(&mut self) {
        self.talk.last = None;
        if let Some(since) = self.listen_since {
            self.audio_ms += since.elapsed().as_millis() as u64;
        }
        self.listen_since = None;
        self.dictation.listen(false);
        if let Some(l) = &self.live {
            // Asks Deepgram for what it is still holding rather than waiting
            // out the endpointing silence.
            l.listen(false);
            self.released = Some(Instant::now());
        }
    }

    /// Called from the event loop while listening. Ends a hold on terminals
    /// with no release event, and stops a toggle that has been left on.
    pub fn tick(&mut self) -> Act {
        // A toggled microphone has nobody holding a key to end it, so it is
        // the one thing that needs an outright limit.
        if self.talk.hold == Hold::Toggle
            && let Some(since) = self.listen_since
        {
            if since.elapsed().as_secs() >= self.cfg.max_listen_secs {
                self.stop_listening();
                self.note = Some(format!(
                    "dictation stopped after {}s; press the talk key to start again",
                    self.cfg.max_listen_secs
                ));
            }
            return Act::Nothing;
        }
        let a = self.talk.tick(Instant::now(), self.listening());
        self.act(a);
        a
    }

    /// How often the event loop has to look in on us. `None` means nothing is
    /// waiting on a clock.
    pub fn wake_in(&self) -> Option<Duration> {
        // A key is down and it is not yet known whether it is being typed or
        // held. That is decided on a clock, so the clock has to be looked at.
        if self.listen_since.is_none() && self.talk.pending > 0 && self.talk.down {
            return Some((self.talk.dwell / 3).max(Duration::from_millis(15)));
        }
        if self.listen_since.is_some() {
            // While the key is down, the clock that decides the hold has
            // ended is the one that matters, and it has to be looked at
            // several times inside the gap or a hold ends late.
            let by_gap = if self.talk.gap_timed() {
                self.talk.gap / 3
            } else {
                Duration::from_millis(200)
            };
            let by_meter = if self.cfg.meter {
                Duration::from_millis(80)
            } else {
                Duration::from_millis(200)
            };
            return Some(by_gap.min(by_meter).max(Duration::from_millis(15)));
        }
        if self.is_live() {
            // Only waiting for the last words to land.
            return self.released.map(|_| Duration::from_millis(120));
        }
        (self.inflight > 0 || !self.waiting.is_empty()).then(|| Duration::from_millis(200))
    }

    // ---- phrases and requests --------------------------------------------

    pub fn phrase(&mut self, p: ah_voice::Phrase) {
        if self.over_budget() {
            return;
        }
        self.chunks.entry(p.seq).or_default();
        self.waiting.push_back(p);
        self.pump();
    }

    fn over_budget(&mut self) -> bool {
        if self.cfg.budget_usd > 0.0 && self.cost >= self.cfg.budget_usd {
            if self.note.is_none() {
                self.note = Some(format!(
                    "dictation stopped at ${:.3}, its voice.budget_usd",
                    self.cost
                ));
            }
            self.stop_listening();
            return true;
        }
        false
    }

    fn pump(&mut self) {
        while self.inflight < self.max_inflight {
            let Some(p) = self.waiting.pop_front() else {
                return;
            };
            self.inflight += 1;
            self.requests += 1;
            self.dispatch(p);
        }
    }

    /// What is already written, so the model can carry on the sentence rather
    /// than start a new one.
    pub fn set_context(&mut self, text: &str) {
        self.context = text.to_string();
    }

    /// The transcript so far, cut to the last `TAIL` characters. Deliberately
    /// short: it is paid for on every phrase.
    fn tail(&self) -> String {
        let mut t = self.context.clone();
        join(&mut t, &self.pending());
        let t = t.trim_end();
        let start = t.len().saturating_sub(TAIL);
        let at = t
            .char_indices()
            .map(|(i, _)| i)
            .find(|i| *i >= start)
            .unwrap_or(0);
        t[at..].to_string()
    }

    fn dispatch(&mut self, p: ah_voice::Phrase) {
        let seq = p.seq;
        if self.stt {
            let provider = self.provider.clone();
            let tx = self.tx.clone();
            let model = self.model.clone();
            let language = self.cfg.language.clone();
            let _ = std::thread::Builder::new()
                .name("ah-voice-stt".into())
                .spawn(move || {
                    let ev = match provider.transcribe(&model, &p.wav, "wav", &language) {
                        Ok((text, cost)) => {
                            let _ = tx.send(Msg::Voice(Event::Delta { seq, text }));
                            Event::Done { seq, cost }
                        }
                        Err(e) => Event::Failed {
                            seq,
                            message: e.to_string(),
                        },
                    };
                    let _ = tx.send(Msg::Voice(ev));
                });
            return;
        }
        let mut system = String::from(
            "Transcribe the audio. Output only the words spoken, verbatim. \
             No preamble, no quotes, no translation, no commentary. \
             If there is no speech, output nothing.",
        );
        if !self.cfg.language.trim().is_empty() {
            system.push_str("\nThe speaker is using ");
            system.push_str(self.cfg.language.trim());
            system.push('.');
        }
        if !self.cfg.prompt_append.trim().is_empty() {
            system.push_str("\nWords that may come up: ");
            system.push_str(self.cfg.prompt_append.trim());
        }
        let tail = self.tail();
        let mut user = String::new();
        if !tail.is_empty() {
            user.push_str("Continues: …");
            user.push_str(&tail);
        }
        let url = format!(
            "data:audio/wav;base64,{}",
            ah_core::clipboard::base64(&p.wav)
        );
        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                Message::system(system),
                Message::user_with_audio(user, vec![url]),
            ],
            tools: Vec::new(),
            max_tokens: Some(256),
            temperature: Some(0.0),
            top_p: None,
            // Transcription: words back, never pictures.
            modalities: Vec::new(),
            reasoning: None,
            provider: None,
            session_id: Some(self.route.clone()),
            cache_control: None,
        };
        let provider = self.provider.clone();
        let tx = self.tx.clone();
        let _ = std::thread::Builder::new()
            .name("ah-voice-stt".into())
            .spawn(move || transcribe(provider, req, seq, tx));
    }

    // ---- results ---------------------------------------------------------

    /// True while Deepgram is the one listening.
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// How the socket to Deepgram is doing, when there is one.
    pub fn link(&self) -> Option<deepgram::Link> {
        self.live.as_ref().map(|l| l.link())
    }

    pub fn live_event(&mut self, ev: deepgram::Event) {
        match ev {
            // Interim words replace each other: Deepgram is refining one
            // guess, not adding to it.
            deepgram::Event::Interim(t) => self.live_guess = t,
            deepgram::Event::Final(t) => {
                join(&mut self.live_said, &t);
                self.live_guess.clear();
                self.requests += 1;
            }
            deepgram::Event::UtteranceEnd => self.live_guess.clear(),
            deepgram::Event::Open => {}
            deepgram::Event::Trouble(m) => self.note = Some(format!("Deepgram: {m}")),
        }
    }

    pub fn delta(&mut self, seq: u64, text: &str) {
        if let Some(c) = self.chunks.get_mut(&seq) {
            c.text.push_str(text);
        }
    }

    pub fn done(&mut self, seq: u64, cost: f64) {
        self.cost += cost;
        self.inflight = self.inflight.saturating_sub(1);
        if let Some(c) = self.chunks.get_mut(&seq) {
            c.done = true;
            let cleaned = ah_voice::filter::clean(&c.text, self.cfg.filter);
            c.text = cleaned.unwrap_or_default();
        }
        self.over_budget();
        self.pump();
    }

    /// A phrase that never came back still has to resolve, or every phrase
    /// after it would wait behind it for ever.
    pub fn failed(&mut self, seq: u64, message: String) {
        self.inflight = self.inflight.saturating_sub(1);
        if let Some(c) = self.chunks.get_mut(&seq) {
            c.done = true;
            c.text.clear();
        }
        self.note = Some(format!("a phrase was lost: {message}"));
        self.pump();
    }

    /// Everything transcribed so far, in the order it was spoken, whether or
    /// not the requests came back in that order.
    pub fn pending(&self) -> String {
        if self.is_live() {
            let mut out = self.live_said.clone();
            join(&mut out, &self.live_guess);
            return out;
        }
        let mut out = String::new();
        for c in self.chunks.values() {
            join(&mut out, &c.text);
        }
        out
    }

    /// True once the key is up and nothing is still on the wire.
    pub fn settled(&self) -> bool {
        if self.is_live() {
            let Some(at) = self.released else {
                return false;
            };
            // The last words arrive a moment after the key comes up, so give
            // them one — but never wait on a socket that has gone quiet.
            return self.live_guess.is_empty() || at.elapsed() > SETTLE;
        }
        !self.listening()
            && self.waiting.is_empty()
            && self.inflight == 0
            && self.chunks.values().all(|c| c.done)
    }

    /// Take the finished text. The chunks go with it, so the next hold starts
    /// from nothing.
    pub fn take(&mut self) -> String {
        let text = self.pending();
        self.chunks.clear();
        self.live_said.clear();
        self.live_guess.clear();
        self.released = None;
        text
    }

    /// Throw away what has not been committed, and stop listening.
    pub fn discard(&mut self) {
        self.stop_listening();
        self.chunks.clear();
        self.waiting.clear();
        self.live_said.clear();
        self.live_guess.clear();
        self.released = None;
    }
}

/// How long the last words get to arrive after the talk key comes up before
/// the grey text is committed anyway.
const SETTLE: Duration = Duration::from_millis(1200);

/// Characters of the sentence so far sent with each phrase. Long enough to
/// finish a clause, short enough not to matter on the bill.
const TAIL: usize = 120;

/// Join two fragments of dictation. A phrase that opens with punctuation
/// closes up against the one before it.
fn join(out: &mut String, next: &str) {
    let next = next.trim();
    if next.is_empty() {
        return;
    }
    if !out.is_empty() && !next.starts_with([',', '.', '!', '?', ';', ':']) {
        out.push(' ');
    }
    out.push_str(next);
}

fn transcribe(provider: Arc<dyn Provider>, req: ChatRequest, seq: u64, tx: Sender<Msg>) {
    let mut attempt = 0;
    loop {
        let cancel = AtomicBool::new(false);
        let mut cost = 0.0f64;
        let mut got_any = false;
        let out = tx.clone();
        let r = provider.stream(&req, &cancel, &mut |ev| {
            match ev {
                StreamEvent::Text(t) => {
                    if !t.is_empty() {
                        got_any = true;
                        let _ = out.send(Msg::Voice(Event::Delta { seq, text: t }));
                    }
                }
                StreamEvent::Usage(u) => cost = u.cost,
                _ => {}
            }
            true
        });
        match r {
            Ok(()) => {
                let _ = tx.send(Msg::Voice(Event::Done { seq, cost }));
                return;
            }
            // One more go, and only if nothing arrived: a retry costs the
            // whole phrase again, and a slow one costs the moment it was for.
            Err(e) if attempt == 0 && !got_any => {
                attempt += 1;
                ah_core::debug!("voice phrase {seq} failed, retrying once: {e}");
            }
            Err(e) => {
                let _ = tx.send(Msg::Voice(Event::Failed {
                    seq,
                    message: e.to_string(),
                }));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::join;

    use super::{Act, Hold, Talk};
    use std::time::{Duration, Instant};

    const DWELL: u64 = 180;

    fn talk(mode: &str) -> Talk {
        Talk::new(mode, 700, DWELL)
    }

    /// A terminal that reports releases, once it has proved it does.
    fn kitty() -> Talk {
        let mut t = talk("auto");
        t.saw_release = true;
        t
    }

    #[test]
    fn a_tap_types_the_key_and_never_starts_listening() {
        let mut t = kitty();
        let t0 = Instant::now();
        assert_eq!(t.press(t0, false), Act::Type, "a tap must type, not listen");
        // Nothing has happened yet at 80 ms in.
        assert_eq!(t.tick(t0 + Duration::from_millis(80), false), Act::Nothing);
        assert_eq!(t.release(false), Act::Nothing);
        // And it stays typed: the keystroke was meant.
        assert_eq!(t.pending, 0);
    }

    #[test]
    fn typing_a_run_of_spaces_never_starts_listening() {
        let mut t = kitty();
        let mut at = Instant::now();
        for _ in 0..8 {
            assert_eq!(t.press(at, false), Act::Type);
            at += Duration::from_millis(140); // a brisk typist
            assert_eq!(t.release(false), Act::Nothing);
            assert_eq!(t.tick(at, false), Act::Nothing);
            at += Duration::from_millis(60);
        }
    }

    #[test]
    fn holding_past_the_dwell_starts_listening_and_takes_the_key_back() {
        let mut t = kitty();
        let t0 = Instant::now();
        assert_eq!(t.press(t0, false), Act::Type);
        assert_eq!(
            t.tick(t0 + Duration::from_millis(DWELL - 40), false),
            Act::Nothing
        );
        assert_eq!(
            t.tick(t0 + Duration::from_millis(DWELL + 10), false),
            Act::Start { undo: 1 },
            "held past the dwell, so the typed key comes back out"
        );
    }

    #[test]
    fn a_keyboard_repeating_is_a_hold_even_with_no_release_reported() {
        // WezTerm without the kitty protocol, measured on this machine: no
        // releases at all, one press, then nothing until the 500 ms repeat
        // delay, then one every 30 ms.
        let mut t = talk("auto");
        let t0 = Instant::now();
        assert_eq!(t.press(t0, false), Act::Type);
        // The dwell cannot be trusted here: nothing has said the key is still
        // down, so it must not start on a clock alone.
        assert_eq!(t.tick(t0 + Duration::from_millis(300), false), Act::Nothing);
        // The first repeat is 500 ms later, which is still too far apart to
        // tell from a person typing two spaces.
        assert_eq!(t.press(t0 + Duration::from_millis(500), false), Act::Type);
        // The second repeat, 30 ms after it, could only be a keyboard.
        assert_eq!(
            t.press(t0 + Duration::from_millis(530), false),
            Act::Start { undo: 2 },
            "both typed spaces come back out"
        );
    }

    #[test]
    fn a_hold_ends_a_gap_after_the_last_repeat() {
        let mut t = talk("auto");
        let t0 = Instant::now();
        t.press(t0, false);
        t.press(t0 + Duration::from_millis(500), false);
        assert!(matches!(
            t.press(t0 + Duration::from_millis(530), false),
            Act::Start { .. }
        ));
        let mut at = 560;
        while at < 2000 {
            assert_eq!(
                t.press(t0 + Duration::from_millis(at), true),
                Act::Nothing,
                "the hold was interrupted at {at} ms"
            );
            at += 30;
        }
        let last = t0 + Duration::from_millis(at - 30);
        assert!(t.gap <= Duration::from_millis(120), "gap {:?}", t.gap);
        assert_eq!(t.tick(last + Duration::from_millis(60), true), Act::Nothing);
        assert_eq!(t.tick(last + Duration::from_millis(200), true), Act::Stop);
        assert_eq!(t.hold, Hold::Gap);
    }

    #[test]
    fn a_release_ends_a_hold_at_once() {
        let mut t = kitty();
        let t0 = Instant::now();
        t.press(t0, false);
        assert!(matches!(
            t.tick(t0 + Duration::from_millis(DWELL + 10), false),
            Act::Start { .. }
        ));
        assert_eq!(t.release(true), Act::Stop);
        assert_eq!(t.hold, Hold::Push);
        assert!(!t.gap_timed());
    }

    #[test]
    fn the_repeat_delay_does_not_end_a_hold_before_it_starts() {
        let mut t = talk("auto");
        let t0 = Instant::now();
        t.press(t0, false);
        t.press(t0 + Duration::from_millis(500), false);
        assert!(matches!(
            t.press(t0 + Duration::from_millis(530), false),
            Act::Start { .. }
        ));
        // A fresh hold waits out the repeat delay again rather than using the
        // narrow gap the last one measured.
        for ms in [560, 700, 900, 1000] {
            assert_eq!(
                t.tick(t0 + Duration::from_millis(ms), true),
                Act::Nothing,
                "ended the hold at {ms} ms"
            );
        }
    }

    #[test]
    fn two_taps_far_apart_do_not_add_up_to_a_hold() {
        let mut t = kitty();
        let t0 = Instant::now();
        assert_eq!(t.press(t0, false), Act::Type);
        t.release(false);
        // Two seconds later, another tap. Nothing about that is a hold.
        assert_eq!(t.press(t0 + Duration::from_secs(2), false), Act::Type);
        assert_eq!(t.pending, 1, "the earlier tap should not still be counted");
    }

    #[test]
    fn toggle_is_only_ever_asked_for_by_hand() {
        let mut t = talk("toggle");
        let t0 = Instant::now();
        assert_eq!(t.press(t0, false), Act::Start { undo: 0 });
        assert_eq!(t.tick(t0 + Duration::from_secs(5), true), Act::Nothing);
        assert_eq!(
            t.press(t0 + Duration::from_secs(6), true),
            Act::Stop,
            "a toggle has to be able to switch off again"
        );
    }

    #[test]
    fn forced_push_to_talk_still_ends_where_no_release_arrives() {
        let mut t = talk("push_to_talk");
        let t0 = Instant::now();
        t.press(t0, false);
        t.press(t0 + Duration::from_millis(500), false);
        assert!(matches!(
            t.press(t0 + Duration::from_millis(530), false),
            Act::Start { .. }
        ));
        assert_eq!(
            t.tick(t0 + Duration::from_millis(1500), true),
            Act::Stop,
            "asking for push-to-talk on a terminal that cannot do it must not hang"
        );
    }

    #[test]
    fn phrases_join_with_one_space() {
        let mut s = String::new();
        join(&mut s, "fix the auth middleware");
        join(&mut s, "  in app.rs  ");
        assert_eq!(s, "fix the auth middleware in app.rs");
    }

    #[test]
    fn punctuation_closes_up_against_the_word_before_it() {
        let mut s = String::new();
        join(&mut s, "run the tests");
        join(&mut s, ", then commit");
        assert_eq!(s, "run the tests, then commit");
    }

    #[test]
    fn a_lost_phrase_leaves_no_gap() {
        let mut s = String::new();
        join(&mut s, "one");
        join(&mut s, "   ");
        join(&mut s, "three");
        assert_eq!(s, "one three");
    }
}
