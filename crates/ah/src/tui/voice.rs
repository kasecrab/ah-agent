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
use ah_voice::deepgram;
use ah_core::provider::{Provider, StreamEvent};

use super::Msg;

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
    /// Not decided yet: the first press is being watched.
    Probing,
    /// Key releases arrive. Hold to talk.
    Push,
    /// No releases, but repeats while held. A gap in the repeats is a release.
    Repeat,
    /// Neither. The key has to toggle.
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
    /// Last press or repeat of the talk key, for the terminals that report no
    /// release.
    last_key: Option<Instant>,
    /// Repeats seen during the probe, which is what tells `Repeat` from
    /// `Toggle`.
    saw_repeat: bool,
    hold: Hold,
    pub note: Option<String>,
    /// What is already in the input box, so a phrase can be told where in the
    /// sentence it belongs.
    context: String,
    tx: Sender<Msg>,
}

impl Session {
    pub fn arm(
        cfg: &VoiceSettings,
        model: String,
        stt: bool,
        provider: Arc<OpenRouter>,
        capture_cmd: String,
        tx: Sender<Msg>,
        route: String,
    ) -> Result<Self, String> {
        let (phrase_ms, max_chunk_ms, max_inflight) = cfg.dials();
        let audio = ah_voice::Config {
            device: cfg.device.clone(),
            capture_cmd,
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
            let key = ah_core::auth::deepgram_key()
                .ok_or("no Deepgram API key: run /voice and choose Deepgram again")?;
            // Two seconds of headroom between the microphone thread and the
            // socket thread, which is far more than either needs.
            let (producer, consumer) = ah_voice::ring::ring(ah_voice::resample::TARGET_RATE as usize * 2);
            let live_tx = tx.clone();
            let live = deepgram::Live::open(
                deepgram::Config {
                    api_key: key,
                    model: model.clone(),
                    language: cfg.language.clone(),
                    keyterms: cfg
                        .prompt_append
                        .split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect(),
                    sample_rate: ah_voice::resample::TARGET_RATE,
                    endpointing_ms: phrase_ms,
                    idle_secs: cfg.idle_secs,
                },
                consumer,
                move |e| {
                    let _ = live_tx.send(Msg::Voice(Event::Live(e)));
                },
            )
            .map_err(|e| e.to_string())?;
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
            last_key: None,
            saw_repeat: false,
            hold: Hold::from_setting(&cfg.hotkey_mode),
            note: None,
            context: String::new(),
            tx,
        })
    }

    pub fn source(&self) -> &str {
        self.dictation.source()
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

    pub fn press(&mut self) {
        self.last_key = Some(Instant::now());
        if self.hold == Hold::Toggle && self.listening() {
            self.stop_listening();
            return;
        }
        if self.listening() {
            return;
        }
        self.saw_repeat = false;
        self.listen_since = Some(Instant::now());
        self.released = None;
        self.dictation.listen(true);
        if let Some(l) = &self.live {
            l.listen(true);
        }
    }

    pub fn repeat(&mut self) {
        self.last_key = Some(Instant::now());
        self.saw_repeat = true;
        if self.hold == Hold::Probing {
            // Repeats without a release: this terminal can do hold-to-talk,
            // just not the tidy way.
            self.hold = Hold::Repeat;
            self.note = Some(
                "this terminal reports no key release, so dictation follows the key repeat instead"
                    .into(),
            );
        }
    }

    pub fn release(&mut self) {
        if self.hold == Hold::Probing || self.hold == Hold::Repeat {
            self.hold = Hold::Push;
            self.note = None;
        }
        if self.hold == Hold::Push {
            self.stop_listening();
        }
    }

    fn stop_listening(&mut self) {
        if let Some(since) = self.listen_since {
            self.audio_ms += since.elapsed().as_millis() as u64;
        }
        self.listen_since = None;
        self.last_key = None;
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
    pub fn tick(&mut self) {
        let Some(since) = self.listen_since else {
            return;
        };
        if self.hold == Hold::Toggle {
            if since.elapsed().as_secs() >= self.cfg.max_listen_secs {
                self.stop_listening();
                self.note = Some(format!(
                    "dictation stopped after {}s; press the talk key to start again",
                    self.cfg.max_listen_secs
                ));
            }
            return;
        }
        let grace = Duration::from_millis(self.cfg.release_grace_ms);
        let Some(last) = self.last_key else { return };
        if last.elapsed() <= grace {
            return;
        }
        match self.hold {
            Hold::Repeat => self.stop_listening(),
            // No release and no repeat: the key cannot be held here.
            Hold::Probing if !self.saw_repeat => {
                self.hold = Hold::Toggle;
                self.note = Some(
                    "this terminal reports neither key release nor repeat, \
                     so the talk key toggles instead of being held"
                        .into(),
                );
                self.last_key = None;
            }
            _ => {}
        }
    }

    /// How often the event loop has to look in on us. `None` means nothing is
    /// waiting on a clock.
    pub fn wake_in(&self) -> Option<Duration> {
        if self.is_live() {
            // The socket thread wakes on its own; this is only about noticing
            // that the last words have landed.
            return if self.listen_since.is_some() {
                Some(Duration::from_millis(if self.cfg.meter { 80 } else { 120 }))
            } else {
                self.released.map(|_| Duration::from_millis(120))
            };
        }
        if self.listen_since.is_none() {
            return (self.inflight > 0 || !self.waiting.is_empty())
                .then(|| Duration::from_millis(200));
        }
        if self.cfg.meter {
            return Some(Duration::from_millis(80));
        }
        let grace = self.cfg.release_grace_ms.max(60) / 3;
        Some(Duration::from_millis(grace.min(200)))
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

    /// The socket is up, so the next phrase costs no handshake.
    pub fn live_ready(&self) -> bool {
        self.live.as_ref().is_some_and(|l| l.connected())
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

    #[test]
    fn hold_falls_back_from_the_setting() {
        assert_eq!(super::Hold::from_setting("auto"), super::Hold::Probing);
        assert_eq!(super::Hold::from_setting("push_to_talk"), super::Hold::Push);
        assert_eq!(super::Hold::from_setting("toggle"), super::Hold::Toggle);
        assert_eq!(super::Hold::from_setting("nonsense"), super::Hold::Probing);
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
