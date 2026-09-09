//! Where one phrase ends and the next begins.
//!
//! Everything here is arithmetic on 20 ms frames: a running noise floor, a
//! threshold above it, and two counters. It costs a few operations per frame,
//! which matters because it runs on every buffer the microphone produces.
//!
//! It also decides what is never sent. A phrase with no speech in it is not a
//! quality problem, it is a bill: every chunk that leaves is a request.

/// Frames are 20 ms, which is short enough to place a cut precisely and long
/// enough that one loud click cannot open a phrase on its own.
pub const FRAME_MS: u64 = 20;

#[derive(Debug, Clone)]
pub struct Config {
    pub rate: u32,
    /// Silence that ends a phrase.
    pub phrase_ms: u64,
    /// Longest phrase before it is cut anyway.
    pub max_chunk_ms: u64,
    /// Audio kept from before speech was detected.
    pub preroll_ms: u64,
    /// Speech has to be this much louder than the noise floor.
    pub speech_ratio: f32,
    /// Shortest run of speech that counts as a phrase.
    pub min_speech_ms: u64,
}

impl Config {
    pub fn frame_len(&self) -> usize {
        (self.rate as u64 * FRAME_MS / 1000) as usize
    }
    fn frames(&self, ms: u64) -> usize {
        (ms / FRAME_MS).max(1) as usize
    }
}

/// A run of audio the chunker decided is worth transcribing.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub pcm: Vec<i16>,
    pub speech_ms: u64,
}

/// Frame energy against an adaptive floor.
struct Floor {
    value: f32,
    seeded: bool,
}

impl Floor {
    fn new() -> Self {
        Self {
            value: 0.0,
            seeded: false,
        }
    }

    /// Falls to a quieter room quickly and rises slowly, so a long sentence
    /// cannot drag the floor up into its own level and cut itself off.
    fn update(&mut self, rms: f32) {
        if !self.seeded {
            self.value = rms.max(1.0);
            self.seeded = true;
            return;
        }
        let a = if rms < self.value { 0.30 } else { 0.002 };
        self.value += (rms - self.value) * a;
        self.value = self.value.max(1.0);
    }
}

pub struct Chunker {
    cfg: Config,
    floor: Floor,
    /// Frames held before speech starts, so nothing is clipped.
    preroll: Vec<Vec<i16>>,
    preroll_frames: usize,
    /// The phrase being built, frame by frame, with each frame's level.
    open: Vec<(Vec<i16>, f32)>,
    speech_frames: usize,
    silence_frames: usize,
    /// Samples not yet a whole frame.
    partial: Vec<i16>,
}

impl Chunker {
    pub fn new(cfg: Config) -> Self {
        let preroll_frames = cfg.frames(cfg.preroll_ms);
        Self {
            preroll: Vec::with_capacity(preroll_frames + 1),
            preroll_frames,
            floor: Floor::new(),
            open: Vec::new(),
            speech_frames: 0,
            silence_frames: 0,
            partial: Vec::new(),
            cfg,
        }
    }

    /// Feed audio. `listening` false keeps the run-up fresh and produces
    /// nothing, which is the state while the microphone is open but the talk
    /// key is not held.
    pub fn push(&mut self, pcm: &[i16], listening: bool, out: &mut Vec<Chunk>) {
        let len = self.cfg.frame_len();
        self.partial.extend_from_slice(pcm);
        while self.partial.len() >= len {
            let frame: Vec<i16> = self.partial.drain(..len).collect();
            self.frame(frame, listening, out);
        }
    }

    /// The talk key came up, or the microphone is closing: hand back whatever
    /// is open if there is speech in it.
    pub fn flush(&mut self) -> Option<Chunk> {
        let len = self.cfg.frame_len();
        if !self.partial.is_empty() {
            let mut frame = std::mem::take(&mut self.partial);
            frame.resize(len, 0);
            let level = rms(&frame);
            self.open.push((frame, level));
        }
        self.close()
    }

    fn frame(&mut self, frame: Vec<i16>, listening: bool, out: &mut Vec<Chunk>) {
        let level = rms(&frame);
        let speech = self.floor.seeded && level > self.floor.value * self.cfg.speech_ratio;
        self.floor.update(level);

        if !listening {
            self.preroll.push(frame);
            if self.preroll.len() > self.preroll_frames {
                self.preroll.remove(0);
            }
            return;
        }

        if self.open.is_empty() {
            if !speech {
                self.preroll.push(frame);
                if self.preroll.len() > self.preroll_frames {
                    self.preroll.remove(0);
                }
                return;
            }
            for f in self.preroll.drain(..) {
                let l = rms(&f);
                self.open.push((f, l));
            }
        }

        self.open.push((frame, level));
        if speech {
            self.speech_frames += 1;
            self.silence_frames = 0;
        } else {
            self.silence_frames += 1;
        }

        let phrase_over = self.silence_frames >= self.cfg.frames(self.cfg.phrase_ms)
            && self.speech_frames >= self.cfg.frames(self.cfg.min_speech_ms);
        if phrase_over {
            if let Some(c) = self.close() {
                out.push(c);
            }
            return;
        }
        if self.open.len() >= self.cfg.frames(self.cfg.max_chunk_ms)
            && let Some(c) = self.cut()
        {
            out.push(c);
        }
    }

    /// End the open phrase. Nothing goes out without speech in it.
    fn close(&mut self) -> Option<Chunk> {
        let frames = std::mem::take(&mut self.open);
        let speech = self.speech_frames;
        self.speech_frames = 0;
        self.silence_frames = 0;
        if speech < self.cfg.frames(self.cfg.min_speech_ms) {
            return None;
        }
        Some(Chunk {
            pcm: frames.into_iter().flat_map(|(f, _)| f).collect(),
            speech_ms: speech as u64 * FRAME_MS,
        })
    }

    /// A phrase that has run on too long is cut at the quietest frame near the
    /// end, so the break falls between words rather than through one.
    fn cut(&mut self) -> Option<Chunk> {
        let n = self.open.len();
        let window = self.cfg.frames(500).min(n.saturating_sub(1)).max(1);
        let start = n - window;
        let mut at = n - 1;
        let mut quietest = f32::MAX;
        for (i, (_, level)) in self.open.iter().enumerate().skip(start) {
            if *level < quietest {
                quietest = *level;
                at = i;
            }
        }
        let rest = self.open.split_off(at + 1);
        let head = std::mem::replace(&mut self.open, rest);
        let speech = self.speech_frames;
        // The tail carries on as the next phrase; assume it is all speech,
        // which is why the cut happened.
        self.speech_frames = self.open.len();
        self.silence_frames = 0;
        if speech < self.cfg.frames(self.cfg.min_speech_ms) {
            return None;
        }
        Some(Chunk {
            pcm: head.into_iter().flat_map(|(f, _)| f).collect(),
            speech_ms: speech as u64 * FRAME_MS,
        })
    }
}

fn rms(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum / frame.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    fn cfg() -> Config {
        Config {
            rate: RATE,
            phrase_ms: 400,
            max_chunk_ms: 3500,
            preroll_ms: 300,
            speech_ratio: 3.0,
            min_speech_ms: 250,
        }
    }

    fn loud(ms: u64) -> Vec<i16> {
        let n = (RATE as u64 * ms / 1000) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / RATE as f64;
                ((t * 300.0 * 2.0 * std::f64::consts::PI).sin() * 9000.0) as i16
            })
            .collect()
    }

    fn quiet(ms: u64) -> Vec<i16> {
        let n = (RATE as u64 * ms / 1000) as usize;
        (0..n).map(|i| ((i % 7) as i16) - 3).collect()
    }

    #[test]
    fn silence_alone_never_becomes_a_request() {
        let mut c = Chunker::new(cfg());
        let mut out = Vec::new();
        c.push(&quiet(5000), true, &mut out);
        assert!(out.is_empty());
        assert!(c.flush().is_none());
    }

    #[test]
    fn a_pause_ends_a_phrase() {
        let mut c = Chunker::new(cfg());
        let mut out = Vec::new();
        c.push(&quiet(600), true, &mut out);
        c.push(&loud(800), true, &mut out);
        c.push(&quiet(600), true, &mut out);
        assert_eq!(out.len(), 1, "expected one phrase, got {}", out.len());
        assert!(out[0].speech_ms >= 250);
    }

    #[test]
    fn two_phrases_come_back_separately() {
        let mut c = Chunker::new(cfg());
        let mut out = Vec::new();
        c.push(&quiet(600), true, &mut out);
        for _ in 0..2 {
            c.push(&loud(700), true, &mut out);
            c.push(&quiet(600), true, &mut out);
        }
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_monologue_is_cut_before_it_runs_away() {
        let mut c = Chunker::new(cfg());
        let mut out = Vec::new();
        c.push(&quiet(600), true, &mut out);
        c.push(&loud(9000), true, &mut out);
        assert!(
            out.len() >= 2,
            "9 s of speech produced {} chunks",
            out.len()
        );
        let longest = out.iter().map(|c| c.pcm.len()).max().unwrap();
        let cap = (RATE as u64 * cfg().max_chunk_ms / 1000) as usize;
        assert!(
            longest <= cap + cfg().frame_len(),
            "chunk of {longest} samples"
        );
    }

    #[test]
    fn the_run_up_to_a_phrase_is_kept() {
        let mut c = Chunker::new(cfg());
        let mut out = Vec::new();
        c.push(&quiet(2000), true, &mut out);
        c.push(&loud(600), true, &mut out);
        c.push(&quiet(600), true, &mut out);
        assert_eq!(out.len(), 1);
        let ms = out[0].pcm.len() as u64 * 1000 / RATE as u64;
        assert!(ms > 600, "phrase came back as {ms} ms with no run-up");
    }

    #[test]
    fn nothing_is_produced_while_the_key_is_up() {
        let mut c = Chunker::new(cfg());
        let mut out = Vec::new();
        c.push(&loud(3000), false, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_key_released_mid_phrase_still_hands_it_over() {
        let mut c = Chunker::new(cfg());
        let mut out = Vec::new();
        c.push(&quiet(600), true, &mut out);
        c.push(&loud(700), true, &mut out);
        assert!(
            out.is_empty(),
            "no pause yet, so nothing should have closed"
        );
        let tail = c.flush().expect("release should hand over the open phrase");
        assert!(tail.speech_ms >= 250);
    }

    #[test]
    fn a_cough_is_too_short_to_send() {
        let mut c = Chunker::new(cfg());
        let mut out = Vec::new();
        c.push(&quiet(600), true, &mut out);
        c.push(&loud(60), true, &mut out);
        c.push(&quiet(800), true, &mut out);
        assert!(out.is_empty());
    }
}
