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

/// A summary being written: `done` tokens of the `budget` it may use, and how
/// long the request has been going. Only the tokens streamed back can be
/// measured — the model spends the first stretch reading the conversation and
/// says nothing — so the clock carries the bar until they arrive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    pub done: u64,
    pub budget: u64,
    pub elapsed: Duration,
}

/// The bar in cells, `[` and `]` not counted.
const BAR: usize = 16;
/// Where the clock alone takes the bar, in percent, given long enough.
const CREEP_CEIL: f32 = 95.0;
/// Seconds the clock takes to carry the bar half that far. It eases off from
/// there rather than stopping, so a long wait still moves.
const CREEP_HALF: f32 = 25.0;

/// How full the bar is, 0 to 99.
///
/// Two things move it, and it takes whichever is further along. The clock
/// eases towards [`CREEP_CEIL`] and never stops, so a long silence while the
/// model reads the conversation never looks like a stall; the summary coming
/// back overtakes it when the model gets to the point quickly. It stops at 99
/// either way: a model stops when the summary is done, not when it runs out of
/// budget, so the last step belongs to the end of the request.
pub fn percent(p: Progress) -> u8 {
    let secs = p.elapsed.as_secs_f32();
    let creep = CREEP_CEIL * secs / (secs + CREEP_HALF);
    let written = 100.0 * (p.done as f32 / p.budget.max(1) as f32);
    creep.max(written).min(99.0) as u8
}

/// `[███░░░░░░░░░░░░░] 21%`, with the empty cells lit by the sweeping band
/// while nothing has come back yet.
fn bar(p: Progress, at: Option<Duration>, pal: &Palette) -> Vec<Span<'static>> {
    let pct = percent(p);
    let full = ((pct as f32 / 100.0 * BAR as f32).round() as usize).min(BAR);
    let mut spans = Vec::with_capacity(BAR + 4);
    spans.push(Span::styled("[", pal.dim()));
    if full > 0 {
        spans.push(Span::styled(
            "\u{2588}".repeat(full),
            Style::default().fg(pal.accent),
        ));
    }
    let rest = "\u{2591}".repeat(BAR - full);
    match at.filter(|_| p.done == 0) {
        Some(at) => spans.extend(shimmer(&rest, at, pal)),
        None if !rest.is_empty() => spans.push(Span::styled(rest, pal.dim())),
        None => {}
    }
    spans.push(Span::styled("]", pal.dim()));
    spans.push(Span::styled(format!(" {pct}%"), pal.dim()));
    spans
}

/// `• Working (3s · esc to interrupt)`, with a bar after the word when the
/// work has a measurable size. `at` is `None` when animation is off, which
/// leaves the word plain.
pub fn line(
    header: &str,
    secs: u64,
    at: Option<Duration>,
    progress: Option<Progress>,
    pal: &Palette,
) -> Line<'static> {
    let mut spans = Vec::with_capacity(8 + BAR);
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
    if let Some(p) = progress {
        spans.push(Span::raw(" "));
        spans.extend(bar(p, at, pal));
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
        let text = line(
            "Working",
            3,
            Some(Duration::from_secs_f32(1.0)),
            None,
            &pal(),
        )
        .to_string();
        assert_eq!(text, "• Working (3s · esc to interrupt)");
        let still = line("Working", 75, None, None, &pal()).to_string();
        assert_eq!(still, "• Working (1m 15s · esc to interrupt)");
    }

    #[test]
    fn the_clock_keeps_the_bar_moving_while_the_model_reads() {
        let waiting = |secs: f32| Progress {
            done: 0,
            budget: 4096,
            elapsed: Duration::from_secs_f32(secs),
        };
        assert_eq!(percent(waiting(0.0)), 0);
        // Nothing has come back yet, but the bar never sits still: the worst
        // gap over a two minute wait is still only seconds long.
        let mut worst = 0.0;
        let mut since = 0.0;
        let mut last = 0;
        for step in 1..=900 {
            let t = step as f32 / 10.0;
            let now = percent(waiting(t));
            assert!(now >= last, "went backwards at {t}s");
            since += 0.1;
            if now > last {
                worst = f32::max(worst, since);
                since = 0.0;
            }
            last = now;
        }
        assert!(worst < 8.0, "stuck for {worst}s");
        assert!(last > 65, "a minute and a half in and only at {last}%");
        // It slows down after that rather than stopping, and never turns back.
        let mut last = percent(waiting(90.0));
        for step in 90..=600 {
            let now = percent(waiting(step as f32));
            assert!(now >= last, "went backwards at {step}s");
            last = now;
        }
        assert!(last < CREEP_CEIL as u8);
    }

    #[test]
    fn a_summary_that_arrives_fast_overtakes_the_clock() {
        let p = |done, secs| Progress {
            done,
            budget: 4096,
            elapsed: Duration::from_secs(secs),
        };
        // Two seconds in the clock has barely moved, but half the budget is
        // written, so the bar shows the writing.
        assert_eq!(percent(p(2048, 2)), 50);
        // It stops short of 100 however much comes back.
        assert_eq!(percent(p(4096, 20)), 99);
        assert_eq!(percent(p(9000, 20)), 99);
    }

    #[test]
    fn a_percentage_is_all_the_line_says_about_size() {
        let text = line(
            "Compacting",
            2,
            Some(Duration::from_secs_f32(1.0)),
            Some(Progress {
                done: 0,
                budget: 4096,
                elapsed: Duration::from_secs(2),
            }),
            &pal(),
        )
        .to_string();
        assert_eq!(
            text,
            "• Compacting [█░░░░░░░░░░░░░░░] 7% (2s · esc to interrupt)"
        );
    }
}
