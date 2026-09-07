//! The "Working" line shown while a turn runs: a cosine band of light sweeping
//! through the word, then the elapsed time and how to stop.
//!
//! The sweep follows Codex's status indicator: a 2 second period over the word
//! plus ten cells of padding on each side, a band five cells wide, and a raised
//! cosine across it, so the light eases in and out instead of stepping.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::theme::Palette;

/// Cells of run-up on each side of the word, so the band arrives and leaves.
const PADDING: usize = 10;
/// One sweep, start to start.
const SWEEP: f32 = 2.0;
/// Half the width of the lit band, in cells.
const BAND: f32 = 5.0;
/// Colours used when the theme leaves `fg`/`bg` to the terminal.
const BASE: (u8, u8, u8) = (128, 128, 128);
const HIGHLIGHT: (u8, u8, u8) = (255, 255, 255);

fn rgb(c: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => fallback,
    }
}

fn blend(a: (u8, u8, u8), b: (u8, u8, u8), alpha: f32) -> (u8, u8, u8) {
    let mix = |x: u8, y: u8| (x as f32 * alpha + y as f32 * (1.0 - alpha)) as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// How lit the cell at `i` is, `0.0` outside the band and `1.0` at its centre.
fn intensity(i: usize, len: usize, at: Duration) -> f32 {
    let period = (len + PADDING * 2) as f32;
    let pos = ((at.as_secs_f32() % SWEEP) / SWEEP * period) as usize;
    let dist = ((i + PADDING) as isize - pos as isize).abs() as f32;
    if dist > BAND {
        return 0.0;
    }
    0.5 * (1.0 + (std::f32::consts::PI * (dist / BAND)).cos())
}

/// `text` with the band at `at` painted across it.
pub fn shimmer(text: &str, at: Duration, pal: &Palette) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let base = rgb(pal.fg, BASE);
    let highlight = rgb(pal.bg, HIGHLIGHT);
    chars
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            let t = intensity(i, chars.len(), at).clamp(0.0, 1.0);
            let (r, g, b) = blend(highlight, base, t * 0.9);
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

/// `0s`, `59s`, `1m 00s`, `2h 03m 09s`.
pub fn elapsed(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3600 {
        return format!("{}m {:02}s", secs / 60, secs % 60);
    }
    format!(
        "{}h {:02}m {:02}s",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// `• Working (3s · esc to interrupt)`. `at` is `None` when animation is off,
/// which leaves the word plain.
pub fn line(header: &str, secs: u64, at: Option<Duration>, pal: &Palette) -> Line<'static> {
    let mut spans = Vec::with_capacity(8);
    match at {
        Some(at) => {
            spans.extend(shimmer("•", at, pal));
            spans.push(Span::raw(" "));
            spans.extend(shimmer(header, at, pal));
        }
        None => {
            spans.push(Span::styled("•", pal.dim()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                header.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
    }
    spans.push(Span::styled(format!(" ({} · ", elapsed(secs)), pal.dim()));
    spans.push(Span::styled("esc", Style::default().fg(pal.fg)));
    spans.push(Span::styled(" to interrupt)", pal.dim()));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ah_core::abi::Theme;

    fn pal() -> Palette {
        Palette::from_theme(&Theme::default())
    }

    /// Where the band sits after `secs` of a sweep, over a word of `len`.
    fn band(len: usize, secs: f32) -> Vec<f32> {
        (0..len)
            .map(|i| intensity(i, len, Duration::from_secs_f32(secs)))
            .collect()
    }

    #[test]
    fn the_band_sweeps_across_the_word_and_starts_over() {
        let len = 7; // "Working"
        // It starts left of the word, ten cells of run-up away.
        assert!(band(len, 0.0).iter().all(|t| *t == 0.0));
        // Halfway through the sweep it is inside the word.
        let mid = band(len, 1.0);
        assert!(mid.iter().any(|t| *t > 0.5), "{mid:?}");
        // And a full period later everything is where it began.
        assert_eq!(band(len, 0.4), band(len, 0.4 + SWEEP));
    }

    #[test]
    fn the_middle_of_the_band_is_the_brightest_cell() {
        let len = 7;
        let at = Duration::from_secs_f32(1.0);
        let lit: Vec<f32> = (0..len).map(|i| intensity(i, len, at)).collect();
        let peak = lit
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        for (i, t) in lit.iter().enumerate() {
            if i != peak {
                assert!(*t <= lit[peak], "cell {i} outshines the band centre");
            }
        }
        assert!(lit[peak] > 0.9);
    }

    #[test]
    fn a_lit_cell_is_brighter_than_a_dark_one() {
        let spans = shimmer("Working", Duration::from_secs_f32(1.0), &pal());
        let brightness = |s: &Span| match s.style.fg {
            Some(Color::Rgb(r, g, b)) => r as u32 + g as u32 + b as u32,
            _ => 0,
        };
        let max = spans.iter().map(brightness).max().unwrap();
        let min = spans.iter().map(brightness).min().unwrap();
        assert!(max > min, "the band left no mark");
        assert_eq!(spans.len(), 7);
    }

    #[test]
    fn the_line_says_what_it_is_doing_and_how_to_stop() {
        let text = line("Working", 3, Some(Duration::from_secs_f32(1.0)), &pal()).to_string();
        assert_eq!(text, "• Working (3s · esc to interrupt)");
        let still = line("Working", 75, None, &pal()).to_string();
        assert_eq!(still, "• Working (1m 15s · esc to interrupt)");
    }

    #[test]
    fn elapsed_reads_as_time() {
        assert_eq!(elapsed(0), "0s");
        assert_eq!(elapsed(59), "59s");
        assert_eq!(elapsed(60), "1m 00s");
        assert_eq!(elapsed(3599), "59m 59s");
        assert_eq!(elapsed(7389), "2h 03m 09s");
    }
}
