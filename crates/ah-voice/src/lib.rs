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
    pub sample_rate: u32,
    pub ring_ms: u64,
    pub phrase_ms: u64,
    pub max_chunk_ms: u64,
    pub preroll_ms: u64,
    pub speech_ratio: f32,
}

/// An open microphone. Dropping it closes the device.
pub struct Dictation {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    listening: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    phrases: Arc<AtomicU64>,
    source: String,
    rate: u32,
}

impl Dictation {
    /// Open the device and start the worker. `hand` is called on the worker
    /// thread with each finished phrase.
    pub fn arm(
        cfg: &Config,
        hand: impl Fn(Phrase) + Send + 'static,
    ) -> Result<Self, capture::Error> {
        let opened = capture::open(&capture::Request {
            device: cfg.device.clone(),
            command: cfg.capture_cmd.clone(),
            rate: cfg.sample_rate,
            ring_ms: cfg.ring_ms,
        })?;
        let source = opened.source.clone();
        let device_rate = opened.rate;
        let channels = opened.channels;
        let listening = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let phrases = Arc::new(AtomicU64::new(0));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let cfg = cfg.clone();
        let (l, lv, ph) = (listening.clone(), level.clone(), phrases.clone());
        let worker = std::thread::Builder::new()
            .name("ah-voice".into())
            .spawn(move || {
                run(opened, device_rate, channels, cfg, l, lv, ph, stop_rx, hand);
            })
            .map_err(|e| capture::Error::Device(e.to_string()))?;

        Ok(Self {
            stop: Some(stop_tx),
            worker: Some(worker),
            listening,
            level,
            phrases,
            source,
            rate: device_rate,
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

    /// What is doing the recording, for the status line and the log.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The device's own rate, before anything is resampled.
    pub fn device_rate(&self) -> u32 {
        self.rate
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

#[allow(clippy::too_many_arguments)]
fn run(
    opened: capture::Opened,
    device_rate: u32,
    channels: u16,
    cfg: Config,
    listening: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    phrases: Arc<AtomicU64>,
    stop: mpsc::Receiver<()>,
    hand: impl Fn(Phrase),
) {
    let mut resampler = resample::Resampler::new(device_rate);
    let mut chunker = vad::Chunker::new(vad::Config {
        rate: resample::TARGET_RATE,
        phrase_ms: cfg.phrase_ms,
        max_chunk_ms: cfg.max_chunk_ms,
        preroll_ms: cfg.preroll_ms,
        speech_ratio: cfg.speech_ratio,
        min_speech_ms: 250,
    });
    // Allocated once. The audio path never grows a buffer after this point.
    let mut raw: Vec<i16> = Vec::with_capacity(device_rate as usize);
    let mut mono: Vec<i16> = Vec::with_capacity(device_rate as usize);
    let mut pcm: Vec<i16> = Vec::with_capacity(resample::TARGET_RATE as usize);
    let mut chunks: Vec<vad::Chunk> = Vec::new();
    // Enough of the device's own samples to cover the run-up to a phrase.
    let preroll = (device_rate as u64 * channels as u64 * cfg.preroll_ms / 1000) as usize;
    let mut was_listening = false;
    let mut seq = 0u64;

    loop {
        let on = listening.load(Ordering::Acquire);
        if !on && !was_listening {
            // Keep only the run-up. Nothing is decoded, nothing is measured.
            opened.audio.keep_last(preroll);
            match stop.recv_timeout(QUIET) {
                Err(RecvTimeoutError::Timeout) => continue,
                _ => break,
            }
        }

        raw.clear();
        opened.audio.drain(&mut raw);
        if !raw.is_empty() {
            mono.clear();
            resample::downmix(&raw, channels, &mut mono);
            pcm.clear();
            resampler.process(&mono, &mut pcm);
            if let Some(l) = peak(&pcm) {
                level.store(l.to_bits(), Ordering::Relaxed);
            }
            chunks.clear();
            chunker.push(&pcm, true, &mut chunks);
            for c in chunks.drain(..) {
                seq += 1;
                phrases.fetch_add(1, Ordering::Relaxed);
                hand(Phrase {
                    seq,
                    wav: wav::encode(&c.pcm, resample::TARGET_RATE),
                    rate: resample::TARGET_RATE,
                    speech_ms: c.speech_ms,
                });
            }
        }

        if !on {
            // The key came up: hand over whatever was still open, then go
            // back to sleep.
            if let Some(c) = chunker.flush() {
                seq += 1;
                phrases.fetch_add(1, Ordering::Relaxed);
                hand(Phrase {
                    seq,
                    wav: wav::encode(&c.pcm, resample::TARGET_RATE),
                    rate: resample::TARGET_RATE,
                    speech_ms: c.speech_ms,
                });
            }
            level.store(0f32.to_bits(), Ordering::Relaxed);
            was_listening = false;
            continue;
        }
        was_listening = true;

        match stop.recv_timeout(BUSY) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    opened.close();
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
            sample_rate: 16_000,
            ring_ms: 4000,
            phrase_ms: 400,
            max_chunk_ms: 3500,
            preroll_ms: 300,
            speech_ratio: 3.0,
        }
    }

    #[test]
    fn a_held_key_turns_speech_into_numbered_phrases() {
        let got: Arc<Mutex<Vec<Phrase>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let d = Dictation::arm(&cfg(fixture_cmd()), move |p| {
            sink.lock().unwrap().push(p);
        })
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
        let d = Dictation::arm(&cfg(fixture_cmd()), move |p| {
            sink.lock().unwrap().push(p);
        })
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
        let d = Dictation::arm(&cfg(format!("cat {}", path.display())), move |p| {
            sink.lock().unwrap().push(p);
        })
        .unwrap();
        d.listen(true);
        std::thread::sleep(Duration::from_millis(300));
        d.listen(false);
        std::thread::sleep(Duration::from_millis(60));
        drop(d);
        assert!(got.lock().unwrap().is_empty(), "silence produced a request");
    }
}
