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
        Color::Black => (0, 0, 0),
        Color::Red => (170, 0, 0),
        Color::Green => (0, 170, 0),
        Color::Yellow => (170, 85, 0),
        Color::Blue => (0, 0, 170),
        Color::Magenta => (170, 0, 170),
        Color::Cyan => (0, 170, 170),
        Color::Gray => (170, 170, 170),
        Color::DarkGray => (85, 85, 85),
        Color::LightRed => (255, 85, 85),
        Color::LightGreen => (85, 255, 85),
        Color::LightYellow => (255, 255, 85),
        Color::LightBlue => (85, 85, 255),
        Color::LightMagenta => (255, 85, 255),
        Color::LightCyan => (85, 255, 255),
        Color::White => (255, 255, 255),
        Color::Indexed(i) => indexed(i, fallback),
        Color::Reset => fallback,
    }
}

/// An xterm palette index as a colour: the sixteen names, then the 6×6×6 cube,
/// then the greys.
fn indexed(i: u8, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match i {
        0..=15 => {
            let names = [
                Color::Black,
                Color::Red,
                Color::Green,
                Color::Yellow,
                Color::Blue,
                Color::Magenta,
                Color::Cyan,
                Color::Gray,
                Color::DarkGray,
                Color::LightRed,
                Color::LightGreen,
                Color::LightYellow,
                Color::LightBlue,
                Color::LightMagenta,
                Color::LightCyan,
                Color::White,
            ];
            rgb(names[i as usize], fallback)
        }
        16..=231 => {
            let n = i - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (step(n / 36), step((n / 6) % 6), step(n % 6))
        }
        232.. => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// How light a colour is, 0.0 to 1.0.
fn luma((r, g, b): (u8, u8, u8)) -> f32 {
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

/// The lit end of the sweep: white, unless the theme paints a light
/// background, where black is what stands out. Blending towards the background
/// itself would sink the band into it and leave a moving hole in the line.
fn lit(pal: &Palette) -> (u8, u8, u8) {
    match pal.bg {
        Color::Reset => HIGHLIGHT,
        c if luma(rgb(c, BASE)) > 0.6 => (0, 0, 0),
        _ => HIGHLIGHT,
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
    let highlight = lit(pal);
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

/// How full the bar is, 0.0 to 0.99.
///
/// Two things move it, and it takes whichever is further along. The clock
/// eases towards [`CREEP_CEIL`] and never stops, so a long silence while the
/// model reads the conversation never looks like a stall; the summary coming
/// back overtakes it when the model gets to the point quickly. It stops short
/// of full either way: a model stops when the summary is done, not when it
/// runs out of budget, so the last step belongs to the end of the request.
pub fn share(p: Progress) -> f32 {
    let secs = p.elapsed.as_secs_f32();
    let creep = CREEP_CEIL * secs / (secs + CREEP_HALF);
    let written = 100.0 * (p.done as f32 / p.budget.max(1) as f32);
    creep.max(written).min(99.0) / 100.0
}

/// [`share`] as the whole number the bar is labelled with.
pub fn percent(p: Progress) -> u8 {
    (share(p) * 100.0) as u8
}

/// Halves of a cell: a heavy rule for what is done, a light one for what is
/// left. Both sit in the middle of the row, so the fill always meets the line
/// it is eating. A block fill cannot: its part-filled cell is a sliver hanging
/// at the cell's left edge, which reads as a hole between the two.
const HALVES: [&str; 3] = ["\u{2500}", "\u{2578}", "\u{2501}"];

/// `[━━━━━━╸─────────] 21%`, the band sweeping the whole bar the way it sweeps
/// the word, so the wait always has something moving in it.
fn bar(p: Progress, at: Option<Duration>, pal: &Palette) -> Vec<Span<'static>> {
    let share = share(p);
    let pct = percent(p);
    let halves = (share * (BAR * 2) as f32).round() as usize;
    let filled = rgb(pal.accent, HIGHLIGHT);
    let empty = rgb(pal.fg, BASE);
    let light = lit(pal);
    let mut spans = Vec::with_capacity(BAR + 4);
    spans.push(Span::styled("[", pal.dim()));
    for i in 0..BAR {
        let part = halves.saturating_sub(i * 2).min(2);
        let t = at.map_or(0.0, |at| intensity(i, BAR, at).clamp(0.0, 1.0));
        let base = if part > 0 { filled } else { empty };
        let (r, g, b) = blend(light, base, t * 0.7);
        spans.push(Span::styled(
            HALVES[part],
            Style::default().fg(Color::Rgb(r, g, b)),
        ));
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
    fn the_bar_grows_by_halves_of_a_cell() {
        let at = |secs| Progress {
            done: 0,
            budget: 4096,
            elapsed: Duration::from_secs_f32(secs),
        };
        // A cell is six percent of the bar. Three seconds apart is less than
        // that, and the bar still shows the difference rather than waiting to
        // jump a whole cell.
        let (a, b) = (
            line("Compacting", 0, None, Some(at(3.0)), &pal()).to_string(),
            line("Compacting", 0, None, Some(at(6.0)), &pal()).to_string(),
        );
        assert_ne!(a, b);
        // A half-filled cell, which meets the line on both sides of it.
        assert!(a.contains(HALVES[1]), "{a}");
    }

    #[test]
    fn the_fill_and_the_track_meet_in_the_middle_of_the_row() {
        // Whatever the share, every cell is a rule on the row's centre line:
        // heavy behind the head, light in front of it. A part-filled block
        // used to sit at the cell's left edge instead, which left a hole
        // between the fill and the line.
        for step in 0..=400 {
            let p = Progress {
                done: step * 12,
                budget: 4096,
                elapsed: Duration::from_secs_f32(step as f32 / 8.0),
            };
            let spans = bar(p, Some(Duration::from_secs_f32(step as f32 / 20.0)), &pal());
            let cells = &spans[1..1 + BAR];
            assert!(
                cells.iter().all(|s| HALVES.contains(&s.content.as_ref())),
                "{:?}",
                cells.iter().map(|s| s.content.clone()).collect::<Vec<_>>()
            );
            // Heavy first, then at most one half, then light: no gaps in it.
            let heavy = cells.iter().filter(|s| s.content == HALVES[2]).count();
            let half = cells.iter().filter(|s| s.content == HALVES[1]).count();
            assert!(half <= 1);
            assert!(cells[..heavy].iter().all(|s| s.content == HALVES[2]));
            assert!(cells[heavy + half..].iter().all(|s| s.content == HALVES[0]));
        }
    }

    #[test]
    fn the_band_sweeps_the_bar_while_the_summary_is_written() {
        let p = Progress {
            done: 2048,
            budget: 4096,
            elapsed: Duration::from_secs(12),
        };
        // Colours, not characters, carry the sweep, and it runs whether or not
        // anything has come back yet.
        let colours = |secs| {
            bar(p, Some(Duration::from_secs_f32(secs)), &pal())
                .iter()
                .map(|s| format!("{:?}", s.style.fg))
                .collect::<Vec<_>>()
        };
        assert_ne!(colours(0.4), colours(1.2));
        // With animation off it is one flat colour per half of the bar.
        let still: Vec<String> = bar(p, None, &pal())
            .iter()
            .map(|s| format!("{:?}", s.style.fg))
            .collect();
        assert_eq!(still.len(), BAR + 3);
    }

    /// A theme with a dark background, the shape the gap showed up in.
    fn dark() -> Palette {
        let t = Theme {
            bg: "#0b0e14".into(),
            fg: "#c5c9c5".into(),
            accent: "#7fd4e8".into(),
            ..Theme::default()
        };
        Palette::from_theme(&t)
    }

    #[test]
    fn the_band_only_ever_lights_the_bar_up() {
        let pal = dark();
        let p = Progress {
            done: 2048,
            budget: 4096,
            elapsed: Duration::from_secs(24),
        };
        // The unlit colours: what a cell is worth with the band far away.
        let floor: Vec<f32> = bar(p, None, &pal)
            .iter()
            .filter_map(|s| match s.style.fg {
                Some(Color::Rgb(r, g, b)) => Some(luma((r, g, b))),
                _ => None,
            })
            .collect();
        // Over a whole sweep, no cell is ever dimmer than that. Blending
        // towards the background instead left a moving hole in the bar.
        for step in 0..60 {
            let at = Duration::from_secs_f32(step as f32 * SWEEP / 60.0);
            let cells: Vec<f32> = bar(p, Some(at), &pal)
                .iter()
                .filter_map(|s| match s.style.fg {
                    Some(Color::Rgb(r, g, b)) => Some(luma((r, g, b))),
                    _ => None,
                })
                .collect();
            for (i, (now, base)) in cells.iter().zip(&floor).enumerate() {
                assert!(now >= base, "cell {i} went dark at {at:?}: {now} < {base}");
            }
        }
    }

    #[test]
    fn the_band_stands_out_against_the_background_it_is_given() {
        // Left to the terminal, or dark: the band is white.
        assert_eq!(lit(&pal()), (255, 255, 255));
        assert_eq!(lit(&dark()), (255, 255, 255));
        // On a light theme white would vanish, so the band is black instead.
        let light = Theme {
            bg: "#fdf6e3".into(),
            ..Theme::default()
        };
        assert_eq!(lit(&Palette::from_theme(&light)), (0, 0, 0));
    }

    #[test]
    fn a_named_or_indexed_colour_is_still_a_colour() {
        // Named colours used to fall back to the highlight, which painted a
        // cyan accent white and left the band with nothing to do.
        assert_eq!(rgb(Color::Cyan, BASE), (0, 170, 170));
        assert_eq!(rgb(Color::Indexed(15), BASE), (255, 255, 255));
        assert_eq!(rgb(Color::Indexed(196), BASE), (255, 0, 0));
        assert_eq!(rgb(Color::Indexed(244), BASE), (128, 128, 128));
        assert_eq!(rgb(Color::Reset, BASE), BASE);
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
            "• Compacting [━───────────────] 7% (2s · esc to interrupt)"
        );
    }
}
