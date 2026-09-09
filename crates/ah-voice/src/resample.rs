//! Bring whatever the device gives down to one 16 kHz mono channel.
//!
//! It is worth the few microseconds: the audio is uploaded over the user's
//! own connection on every phrase, and 48 kHz stereo is six times the bytes
//! of 16 kHz mono for no gain a transcriber can hear.

/// Rate every transcriber is happy with, and the cheapest one to send.
pub const TARGET_RATE: u32 = 16_000;

const TAPS: usize = 31;

/// Average the channels into one.
pub fn downmix(interleaved: &[i16], channels: u16, out: &mut Vec<i16>) {
    if channels <= 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let n = channels as usize;
    out.reserve(interleaved.len() / n);
    for frame in interleaved.chunks_exact(n) {
        let sum: i32 = frame.iter().map(|s| *s as i32).sum();
        out.push((sum / n as i32) as i16);
    }
}

/// A low-pass followed by linear interpolation. Both halves keep their state
/// between calls, so a phrase spanning many buffers has no seam or click in it.
pub struct Resampler {
    src_rate: u32,
    taps: [f32; TAPS],
    hist: [f32; TAPS],
    /// Where in the filtered stream the next output sample sits.
    pos: f64,
    step: f64,
    /// The filtered sample before `pos`, kept so interpolation can span calls.
    prev: f32,
    started: bool,
    filtered: Vec<f32>,
}

impl Resampler {
    pub fn new(src_rate: u32) -> Self {
        let step = src_rate as f64 / TARGET_RATE as f64;
        // Guard the target's Nyquist, with a little margin so the transition
        // band does not eat speech.
        let cutoff = if src_rate > TARGET_RATE {
            0.45 * TARGET_RATE as f64 / src_rate as f64
        } else {
            0.5
        };
        Self {
            src_rate,
            taps: sinc_taps(cutoff),
            hist: [0.0; TAPS],
            pos: 0.0,
            step,
            prev: 0.0,
            started: false,
            filtered: Vec::new(),
        }
    }

    pub fn src_rate(&self) -> u32 {
        self.src_rate
    }

    pub fn passthrough(&self) -> bool {
        self.src_rate == TARGET_RATE
    }

    pub fn process(&mut self, pcm: &[i16], out: &mut Vec<i16>) {
        if self.passthrough() {
            out.extend_from_slice(pcm);
            return;
        }
        self.filtered.clear();
        self.filtered.reserve(pcm.len());
        for s in pcm {
            self.hist.copy_within(0..TAPS - 1, 1);
            self.hist[0] = *s as f32;
            let mut acc = 0.0f32;
            for i in 0..TAPS {
                acc += self.hist[i] * self.taps[i];
            }
            self.filtered.push(acc);
        }
        if !self.started {
            self.started = true;
            self.prev = self.filtered.first().copied().unwrap_or(0.0);
        }
        // `pos` is relative to the start of `filtered`, and may be negative by
        // less than one sample, which is what `prev` is for.
        while self.pos < self.filtered.len() as f64 {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let idx = i as isize;
            let a = if idx < 0 {
                self.prev
            } else {
                self.filtered[idx as usize]
            };
            let b = self
                .filtered
                .get((idx + 1).max(0) as usize)
                .copied()
                .unwrap_or(a);
            let v = a + (b - a) * frac;
            out.push(v.clamp(-32768.0, 32767.0) as i16);
            self.pos += self.step;
        }
        self.prev = self.filtered.last().copied().unwrap_or(self.prev);
        self.pos -= self.filtered.len() as f64;
    }
}

/// Windowed sinc, normalised to unity gain at DC.
fn sinc_taps(cutoff: f64) -> [f32; TAPS] {
    let mut t = [0.0f32; TAPS];
    let mid = (TAPS - 1) as f64 / 2.0;
    let mut sum = 0.0f64;
    for (i, slot) in t.iter_mut().enumerate() {
        let x = i as f64 - mid;
        let sinc = if x.abs() < 1e-9 {
            2.0 * cutoff
        } else {
            (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
        };
        // Hamming.
        let w = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (TAPS - 1) as f64).cos();
        let v = sinc * w;
        sum += v;
        *slot = v as f32;
    }
    for slot in t.iter_mut() {
        *slot = (*slot as f64 / sum) as f32;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, ms: u64) -> Vec<i16> {
        let n = (rate as u64 * ms / 1000) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / rate as f64;
                ((t * hz * 2.0 * std::f64::consts::PI).sin() * 12000.0) as i16
            })
            .collect()
    }

    fn rms(pcm: &[i16]) -> f64 {
        if pcm.is_empty() {
            return 0.0;
        }
        let s: f64 = pcm.iter().map(|v| (*v as f64) * (*v as f64)).sum();
        (s / pcm.len() as f64).sqrt()
    }

    #[test]
    fn downmix_averages_the_channels() {
        let mut out = Vec::new();
        downmix(&[100, 300, -50, 50], 2, &mut out);
        assert_eq!(out, vec![200, 0]);
    }

    #[test]
    fn downmix_leaves_mono_alone() {
        let mut out = Vec::new();
        downmix(&[1, 2, 3], 1, &mut out);
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn sixteen_k_is_a_passthrough() {
        let mut r = Resampler::new(16_000);
        assert!(r.passthrough());
        let mut out = Vec::new();
        r.process(&[1, 2, 3], &mut out);
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn forty_eight_k_comes_out_a_third_as_long() {
        let mut r = Resampler::new(48_000);
        let mut out = Vec::new();
        r.process(&tone(48_000, 440.0, 1000), &mut out);
        let n = out.len() as i64;
        assert!((n - 16_000).abs() < 32, "got {n} samples, wanted ~16000");
    }

    #[test]
    fn forty_four_one_k_comes_out_at_the_right_length() {
        let mut r = Resampler::new(44_100);
        let mut out = Vec::new();
        r.process(&tone(44_100, 440.0, 1000), &mut out);
        let n = out.len() as i64;
        assert!((n - 16_000).abs() < 32, "got {n} samples, wanted ~16000");
    }

    #[test]
    fn speech_band_survives_the_trip() {
        let mut r = Resampler::new(48_000);
        let src = tone(48_000, 1000.0, 500);
        let mut out = Vec::new();
        r.process(&src, &mut out);
        let (a, b) = (rms(&src), rms(&out[40..]));
        assert!(b > a * 0.8, "1 kHz lost too much level: {a} -> {b}");
    }

    #[test]
    fn a_tone_above_the_new_nyquist_is_filtered_away() {
        let mut r = Resampler::new(48_000);
        let mut out = Vec::new();
        r.process(&tone(48_000, 11_000.0, 500), &mut out);
        // Without the low-pass this aliases down into the speech band at full
        // level instead of being attenuated.
        assert!(rms(&out[40..]) < 2000.0, "aliased at {}", rms(&out[40..]));
    }

    #[test]
    fn buffers_join_without_a_seam() {
        let src = tone(48_000, 440.0, 300);
        let mut whole = Vec::new();
        Resampler::new(48_000).process(&src, &mut whole);
        let mut split = Vec::new();
        let mut r = Resampler::new(48_000);
        for part in src.chunks(1024) {
            r.process(part, &mut split);
        }
        assert_eq!(whole.len(), split.len());
        let worst = whole
            .iter()
            .zip(&split)
            .map(|(a, b)| (*a as i32 - *b as i32).abs())
            .max()
            .unwrap_or(0);
        assert!(worst <= 1, "chunked output drifts by {worst}");
    }
}
