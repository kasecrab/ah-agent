//! Dictation: microphone in, phrases out.
//!
//! Nothing here talks to a network. The crate turns a held key into a stream
//! of short WAV clips with speech in them; who transcribes them, and what it
//! costs, is decided a layer up.
//!
//! What it costs *here* is the point. Armed and quiet, one thread wakes five
//! times a second to throw away audio nobody asked for. Listening, it wakes
//! every 10 ms, which is the rate the device delivers anyway. Disarmed, there
//! is nothing at all: no thread, no buffer, no open device.

pub mod capture;
#[cfg(feature = "live")]
pub mod deepgram;
pub mod filter;
pub mod resample;
pub mod ring;
pub mod vad;
pub mod wav;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// One run of speech, encoded and ready to send.
#[derive(Debug, Clone, PartialEq)]
pub struct Phrase {
    /// Counts up for the life of the dictation, across separate holds of the
    /// talk key, so text can be put back in the order it was spoken however
    /// the replies come back.
    pub seq: u64,
    pub wav: Vec<u8>,
    pub rate: u32,
    /// How much of the clip was speech.
    pub speech_ms: u64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub device: String,
    pub capture_cmd: String,
    /// Hold the device open for as long as dictation is armed, rather than
    /// opening it for each hold. Opening costs a few tens of milliseconds, so
    /// this is rarely worth it — and a microphone that is open is a
    /// microphone that is on.
    pub keep_open: bool,
    pub sample_rate: u32,
    pub ring_ms: u64,
    pub phrase_ms: u64,
    pub max_chunk_ms: u64,
    pub preroll_ms: u64,
    pub speech_ratio: f32,
}

/// Where the 16 kHz mono stream goes once it has been captured.
pub enum Output {
    /// Cut it into phrases here and hand each one over encoded, for a
    /// transcriber that takes whole clips.
    Phrases(Box<dyn Fn(Phrase) + Send>),
    /// Pass it straight through, for a transcriber that listens continuously
    /// and decides for itself where a phrase ends.
    Live(ring::Producer),
}

/// Armed dictation. The device is not opened until the talk key goes down,
/// and is closed again when it comes up: a microphone that is open is a
/// microphone that is on, and on a Bluetooth headset it also holds the
/// earpieces in their low-quality call profile the whole time.
pub struct Dictation {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    listening: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    phrases: Arc<AtomicU64>,
    /// What recorded, and at what rate. Not known until the device has been
    /// opened for the first time.
    heard: Arc<Mutex<Heard>>,
}

#[derive(Default, Clone)]
struct Heard {
    source: String,
    rate: u32,
    /// A device that would not open, to be said once and then forgotten.
    trouble: Option<String>,
}

impl Dictation {
    /// Start the worker. The microphone stays shut until `listen(true)`.
    pub fn arm(cfg: &Config, out: Output) -> Result<Self, capture::Error> {
        let listening = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let phrases = Arc::new(AtomicU64::new(0));
        let heard = Arc::new(Mutex::new(Heard::default()));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let cfg = cfg.clone();
        let (l, lv, ph, hd) = (
            listening.clone(),
            level.clone(),
            phrases.clone(),
            heard.clone(),
        );
        let worker = std::thread::Builder::new()
            .name("ah-voice".into())
            .spawn(move || run(cfg, l, lv, ph, hd, stop_rx, out))
            .map_err(|e| capture::Error::Device(e.to_string()))?;

        Ok(Self {
            stop: Some(stop_tx),
            worker: Some(worker),
            listening,
            level,
            phrases,
            heard,
        })
    }

    /// Hold the talk key down, or let it up.
    pub fn listen(&self, on: bool) {
        self.listening.store(on, Ordering::Release);
    }

    pub fn listening(&self) -> bool {
        self.listening.load(Ordering::Acquire)
    }

    /// Loudest recent frame, 0.0 to 1.0, for the level bar.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Phrases handed over so far, so a caller can tell "said nothing" from
    /// "said something that has not come back yet".
    pub fn phrases(&self) -> u64 {
        self.phrases.load(Ordering::Relaxed)
    }

    /// What is doing the recording. Empty until the first hold, because
    /// nothing has been opened before then.
    pub fn source(&self) -> String {
        self.heard
            .lock()
            .map(|h| h.source.clone())
            .unwrap_or_default()
    }

    /// The device's own rate, before anything is resampled. 0 until the first
    /// hold.
    pub fn device_rate(&self) -> u32 {
        self.heard.lock().map(|h| h.rate).unwrap_or(0)
    }

    /// A device that would not open, said once. The microphone is only
    /// reached for when the key goes down, so this is where a missing one is
    /// found out about.
    pub fn take_trouble(&self) -> Option<String> {
        self.heard.lock().ok().and_then(|mut h| h.trouble.take())
    }
}

impl Drop for Dictation {
    fn drop(&mut self) {
        self.listening.store(false, Ordering::Release);
        drop(self.stop.take());
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// How often the worker looks at the ring. While the talk key is up there is
/// nothing to do but drop stale audio, so it barely wakes at all.
const BUSY: Duration = Duration::from_millis(10);
const QUIET: Duration = Duration::from_millis(200);

/// One open device, and everything derived from its rate.
struct Mic {
    opened: capture::Opened,
    resampler: resample::Resampler,
    channels: u16,
}

#[allow(clippy::too_many_arguments)]
fn run(
    cfg: Config,
    listening: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    phrases: Arc<AtomicU64>,
    heard: Arc<Mutex<Heard>>,
    stop: mpsc::Receiver<()>,
    out: Output,
) {
    let mut chunker = vad::Chunker::new(vad::Config {
        rate: resample::TARGET_RATE,
        phrase_ms: cfg.phrase_ms,
        max_chunk_ms: cfg.max_chunk_ms,
        preroll_ms: cfg.preroll_ms,
        speech_ratio: cfg.speech_ratio,
        min_speech_ms: 250,
    });
    // Allocated once. The audio path never grows a buffer after this point.
    let mut raw: Vec<i16> = Vec::with_capacity(resample::TARGET_RATE as usize);
    let mut mono: Vec<i16> = Vec::with_capacity(resample::TARGET_RATE as usize);
    let mut pcm: Vec<i16> = Vec::with_capacity(resample::TARGET_RATE as usize);
    let mut chunks: Vec<vad::Chunk> = Vec::new();
    let mut mic: Option<Mic> = None;
    let mut seq = 0u64;

    let hand_over = |c: vad::Chunk, seq: &mut u64| {
        if let Output::Phrases(hand) = &out {
            *seq += 1;
            phrases.fetch_add(1, Ordering::Relaxed);
            hand(Phrase {
                seq: *seq,
                wav: wav::encode(&c.pcm, resample::TARGET_RATE),
                rate: resample::TARGET_RATE,
                speech_ms: c.speech_ms,
            });
        }
    };

    loop {
        let on = listening.load(Ordering::Acquire);

        // The key went down and there is no device yet. This is the only
        // place a microphone is ever reached for.
        if on && mic.is_none() {
            match capture::open(&capture::Request {
                device: cfg.device.clone(),
                command: cfg.capture_cmd.clone(),
                rate: cfg.sample_rate,
                ring_ms: cfg.ring_ms,
            }) {
                Ok(opened) => {
                    crate::log(&format!(
                        "recording with {} at {} Hz, {} channel(s)",
                        opened.source, opened.rate, opened.channels
                    ));
                    if let Ok(mut h) = heard.lock() {
                        h.source = opened.source.clone();
                        h.rate = opened.rate;
                    }
                    let resampler = resample::Resampler::new(opened.rate);
                    let channels = opened.channels;
                    mic = Some(Mic {
                        opened,
                        resampler,
                        channels,
                    });
                }
                Err(e) => {
                    // Nothing to record with. Say so once and stop trying, or
                    // every tick would be another failed open.
                    if let Ok(mut h) = heard.lock() {
                        h.trouble = Some(e.to_string());
                    }
                    listening.store(false, Ordering::Release);
                    match stop.recv_timeout(QUIET) {
                        Err(RecvTimeoutError::Timeout) => continue,
                        _ => break,
                    }
                }
            }
        }

        if let Some(m) = mic.as_mut() {
            raw.clear();
            m.opened.audio.drain(&mut raw);
            if !raw.is_empty() {
                mono.clear();
                resample::downmix(&raw, m.channels, &mut mono);
                pcm.clear();
                m.resampler.process(&mono, &mut pcm);
                if let Some(l) = peak(&pcm) {
                    level.store(l.to_bits(), Ordering::Relaxed);
                }
                match &out {
                    // Somebody downstream is listening continuously and
                    // decides for itself where a phrase ends, so nothing is
                    // cut here.
                    Output::Live(to) => to.write(&pcm),
                    Output::Phrases(_) => {
                        chunks.clear();
                        chunker.push(&pcm, true, &mut chunks);
                        for c in chunks.drain(..) {
                            hand_over(c, &mut seq);
                        }
                    }
                }
            }
        }

        if !on {
            // The key came up: hand over whatever phrase was still open, then
            // shut the microphone unless it was asked to stay.
            if let Some(c) = chunker.flush() {
                hand_over(c, &mut seq);
            }
            level.store(0f32.to_bits(), Ordering::Relaxed);
            if !cfg.keep_open
                && let Some(m) = mic.take()
            {
                m.opened.close();
            }
            match stop.recv_timeout(QUIET) {
                Err(RecvTimeoutError::Timeout) => continue,
                _ => break,
            }
        }

        match stop.recv_timeout(BUSY) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    if let Some(m) = mic.take() {
        m.opened.close();
    }
}

fn peak(pcm: &[i16]) -> Option<f32> {
    let m = pcm.iter().map(|s| s.unsigned_abs()).max()?;
    Some(m as f32 / 32768.0)
}

/// Where the crate's own troubles go. The TUI owns the screen, so writing to
/// stderr would corrupt it; this is wired to the harness log instead.
pub fn log(message: &str) {
    if let Some(f) = LOG.get() {
        f(message);
    }
}

static LOG: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

pub fn set_log(f: fn(&str)) {
    let _ = LOG.set(f);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A recorder that prints a second of loud tone, a pause, another second
    /// of tone, then stops.
    fn fixture_cmd() -> String {
        let mut pcm: Vec<i16> = Vec::new();
        let rate = 16_000usize;
        let tone = |v: &mut Vec<i16>, ms: usize| {
            for i in 0..(rate * ms / 1000) {
                let t = i as f64 / rate as f64;
                v.push(((t * 300.0 * 2.0 * std::f64::consts::PI).sin() * 9000.0) as i16);
            }
        };
        let hush =
            |v: &mut Vec<i16>, ms: usize| v.extend(std::iter::repeat_n(0i16, rate * ms / 1000));
        hush(&mut pcm, 500);
        tone(&mut pcm, 800);
        hush(&mut pcm, 700);
        tone(&mut pcm, 800);
        hush(&mut pcm, 700);
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in &pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let path = std::env::temp_dir().join("ah-voice-fixture.raw");
        std::fs::write(&path, &bytes).unwrap();
        format!("cat {}", path.display())
    }

    fn cfg(cmd: String) -> Config {
        Config {
            device: String::new(),
            capture_cmd: cmd,
            keep_open: false,
            sample_rate: 16_000,
            ring_ms: 4000,
            phrase_ms: 400,
            max_chunk_ms: 3500,
            preroll_ms: 300,
            speech_ratio: 3.0,
        }
    }

    #[test]
    fn live_output_streams_instead_of_cutting_phrases() {
        let (producer, consumer) = ring::ring(16_000 * 4);
        let d = Dictation::arm(&cfg(fixture_cmd()), Output::Live(producer)).unwrap();
        d.listen(true);
        let mut pcm = Vec::new();
        for _ in 0..300 {
            consumer.drain(&mut pcm);
            if pcm.len() > 8_000 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        d.listen(false);
        drop(d);
        // Half a second of 16 kHz audio, silence and all: nothing was cut out
        // and nothing waited for a pause.
        assert!(pcm.len() > 8_000, "only {} samples arrived", pcm.len());
    }

    #[test]
    fn a_held_key_turns_speech_into_numbered_phrases() {
        let got: Arc<Mutex<Vec<Phrase>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let d = Dictation::arm(
            &cfg(fixture_cmd()),
            Output::Phrases(Box::new(move |p| sink.lock().unwrap().push(p))),
        )
        .unwrap();
        d.listen(true);
        for _ in 0..200 {
            if got.lock().unwrap().len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        d.listen(false);
        std::thread::sleep(Duration::from_millis(60));
        drop(d);
        let got = got.lock().unwrap();
        assert!(got.len() >= 2, "expected two phrases, got {}", got.len());
        assert_eq!(got[0].seq, 1);
        assert_eq!(got[1].seq, 2);
        for p in got.iter() {
            assert_eq!(&p.wav[0..4], b"RIFF");
            assert_eq!(p.rate, 16_000);
            assert!(p.speech_ms >= 250);
        }
    }

    #[test]
    fn a_key_that_is_never_held_sends_nothing() {
        let got: Arc<Mutex<Vec<Phrase>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let d = Dictation::arm(
            &cfg(fixture_cmd()),
            Output::Phrases(Box::new(move |p| sink.lock().unwrap().push(p))),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(d.phrases(), 0);
        drop(d);
        assert!(got.lock().unwrap().is_empty());
    }

    #[test]
    fn silence_alone_costs_nothing() {
        let path = std::env::temp_dir().join("ah-voice-silence.raw");
        std::fs::write(&path, vec![0u8; 16_000 * 2 * 2]).unwrap();
        let got: Arc<Mutex<Vec<Phrase>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let d = Dictation::arm(
            &cfg(format!("cat {}", path.display())),
            Output::Phrases(Box::new(move |p| sink.lock().unwrap().push(p))),
        )
        .unwrap();
        d.listen(true);
        std::thread::sleep(Duration::from_millis(300));
        d.listen(false);
        std::thread::sleep(Duration::from_millis(60));
        drop(d);
        assert!(got.lock().unwrap().is_empty(), "silence produced a request");
    }
}
