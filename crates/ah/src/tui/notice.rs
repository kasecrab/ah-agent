//! Passing notices. Anything that only reports what just happened — a copy, a
//! model switch, a plugin complaining — sits above the input box for a few
//! seconds and then goes, instead of settling into the conversation where it
//! would still be scrolled past an hour later.

use std::time::{Duration, Instant};

use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::theme::Palette;
use super::transcript::wrap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Note,
    Warn,
}

struct Note {
    /// What counts as "the same notice again": the text with every run of
    /// digits flattened, so ten copies in a row are one line and a counter
    /// rather than ten lines that differ only in a character count.
    key: String,
    text: String,
    level: Level,
    count: u32,
    until: Instant,
}

/// On screen at once. A new one past this pushes the oldest off.
const MAX: usize = 3;
/// Rows the whole deck may take, however many are stacked.
const MAX_ROWS: usize = 6;
/// Rows one notice wraps to before it is cut short. A long error is a hint to
/// go and look at the log, not the log itself.
const MAX_LINES: usize = 3;
/// Widest a notice gets, so the deck stays a column on the right rather than
/// a banner across the window.
const MAX_WIDTH: u16 = 72;
/// Columns the leading glyph takes. `·` and `✗` are both one cell wide.
const MARK_W: usize = 2;

#[derive(Default)]
pub struct Deck {
    items: Vec<Note>,
}

/// The dedupe key: digits collapse to `#` so `copied 42 chars` and `copied 7
/// chars` are the same notice said twice.
fn key_of(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_digits = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
                in_digits = true;
            }
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

/// Put an ellipsis on the end of a cut-short row without letting it grow
/// past the column it has to fit in.
fn ellipsize(row: &mut String, room: usize) {
    while row.width() + 1 > room && !row.is_empty() {
        row.pop();
    }
    row.push('…');
}

impl Deck {
    /// Say something for `ttl`. A notice already on screen that says the same
    /// thing is bumped and moved to the bottom rather than stacked under
    /// itself; an explicit `key` groups notices whose wording differs every
    /// time, like the model one.
    pub fn push(&mut self, level: Level, key: Option<&str>, text: String, ttl: Duration) {
        let key = key.map(str::to_string).unwrap_or_else(|| key_of(&text));
        let now = Instant::now();
        let count = match self
            .items
            .iter()
            .position(|t| t.key == key && t.level == level)
        {
            Some(i) => self.items.remove(i).count + 1,
            None => 1,
        };
        self.items.push(Note {
            key,
            text,
            level,
            count,
            until: now + ttl,
        });
        while self.items.len() > MAX {
            self.items.remove(0);
        }
    }

    /// Drop whatever has run out. True when anything went, so the caller
    /// knows the frame is stale.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.items.len();
        self.items.retain(|t| t.until > now);
        self.items.len() != before
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// When the next one runs out, for the event loop's timeout.
    pub fn wake_in(&self, now: Instant) -> Option<Duration> {
        self.items
            .iter()
            .map(|t| t.until.saturating_duration_since(now))
            .min()
    }

    /// The deck laid out for a window this wide, oldest first.
    pub fn lines(&self, width: u16, pal: &Palette) -> Vec<Line<'static>> {
        let room = width.saturating_sub(2).clamp(8, MAX_WIDTH) as usize;
        let mut out: Vec<Line<'static>> = Vec::new();
        for t in &self.items {
            // Only the mark changes colour between a note and a warning; the
            // text of both sits at the same weight, so the deck reads as one
            // thing rather than as two competing ones.
            let (mark, mark_style) = match t.level {
                Level::Note => ("· ", pal.dim()),
                Level::Warn => ("✗ ", pal.bold(pal.error)),
            };
            let text_style = pal.dim();
            let body = match t.count {
                0 | 1 => t.text.clone(),
                n => format!("{} ×{n}", t.text),
            };
            let room = room.saturating_sub(MARK_W).max(1);
            // The mark rides on the first row only, so a wrapped notice reads
            // as one thing and not as two.
            let mut rows = wrap(&body, room);
            if rows.len() > MAX_LINES {
                rows.truncate(MAX_LINES);
                if let Some(last) = rows.last_mut() {
                    ellipsize(last, room);
                }
            }
            for (i, r) in rows.into_iter().enumerate() {
                if out.len() == MAX_ROWS {
                    return out;
                }
                out.push(if i == 0 {
                    Line::from(vec![
                        Span::styled(mark, mark_style),
                        Span::styled(r, text_style),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw(" ".repeat(MARK_W)),
                        Span::styled(r, text_style),
                    ])
                });
            }
        }
        out
    }
}

/// Draw the deck flush right, a column clear of the edge so it lines up with
/// nothing and reads as floating over the input box. The lines come from
/// `Deck::lines`, which the layout has already counted.
pub fn draw(f: &mut ratatui::Frame, area: Rect, lines: Vec<Line<'static>>) {
    if area.height == 0 || lines.is_empty() {
        return;
    }
    let r = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Right), r);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ah_core::abi::Theme;

    fn deck() -> Deck {
        Deck::default()
    }

    #[test]
    fn the_same_notice_twice_is_one_line_and_a_count() {
        let mut d = deck();
        let ttl = Duration::from_secs(5);
        for _ in 0..10 {
            d.push(Level::Note, None, "copied 42 chars".into(), ttl);
        }
        let pal = Palette::from_theme(&Theme::default());
        let lines = d.lines(80, &pal);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].spans.iter().any(|s| s.content.contains("×10")));
    }

    #[test]
    fn a_count_that_moved_is_still_the_same_notice() {
        let mut d = deck();
        let ttl = Duration::from_secs(5);
        d.push(Level::Note, None, "copied 42 chars".into(), ttl);
        d.push(Level::Note, None, "copied 7 chars".into(), ttl);
        let pal = Palette::from_theme(&Theme::default());
        let lines = d.lines(80, &pal);
        assert_eq!(lines.len(), 1);
        // The newest wording wins, so the number on screen is the true one.
        let said: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(said.contains("copied 7 chars"), "{said}");
        assert!(said.contains("×2"), "{said}");
    }

    #[test]
    fn an_error_and_a_notice_that_read_alike_stay_apart() {
        let mut d = deck();
        let ttl = Duration::from_secs(5);
        d.push(Level::Note, None, "settings: bad".into(), ttl);
        d.push(Level::Warn, None, "settings: bad".into(), ttl);
        let pal = Palette::from_theme(&Theme::default());
        assert_eq!(d.lines(80, &pal).len(), 2);
    }

    #[test]
    fn only_the_last_few_are_kept() {
        let mut d = deck();
        let ttl = Duration::from_secs(5);
        for i in 0..8 {
            d.push(
                Level::Note,
                Some(&format!("k{i}")),
                format!("thing {i}"),
                ttl,
            );
        }
        let pal = Palette::from_theme(&Theme::default());
        let lines = d.lines(80, &pal);
        assert_eq!(lines.len(), MAX);
        let last: String = lines[MAX - 1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(last.contains("thing 7"), "{last}");
    }

    #[test]
    fn they_go_when_their_time_is_up() {
        let mut d = deck();
        d.push(
            Level::Note,
            None,
            "gone soon".into(),
            Duration::from_millis(0),
        );
        d.push(
            Level::Note,
            Some("stay"),
            "still here".into(),
            Duration::from_secs(30),
        );
        assert!(d.expire(Instant::now()));
        let pal = Palette::from_theme(&Theme::default());
        assert_eq!(d.lines(80, &pal).len(), 1);
    }

    #[test]
    fn a_long_one_is_cut_short_rather_than_taking_the_window() {
        let mut d = deck();
        d.push(
            Level::Warn,
            None,
            "a ".repeat(400).trim().to_string(),
            Duration::from_secs(5),
        );
        let pal = Palette::from_theme(&Theme::default());
        let lines = d.lines(200, &pal);
        assert_eq!(lines.len(), MAX_LINES);
        // Nothing is wider than the column, however wide the window is.
        for l in &lines {
            assert!(l.width() <= MAX_WIDTH as usize, "{}", l.width());
        }
    }

    #[test]
    fn it_draws_flush_right_a_column_clear_of_the_edge() {
        let mut d = deck();
        d.push(
            Level::Note,
            None,
            "copied 12 chars".into(),
            Duration::from_secs(5),
        );
        let pal = Palette::from_theme(&Theme::default());
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 1)).unwrap();
        let lines = d.lines(40, &pal);
        term.draw(|f| draw(f, f.area(), lines.clone())).unwrap();
        let buf = term.backend().buffer().clone();
        let row: String = (0..40).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(row, "                      · copied 12 chars ");
    }

    #[test]
    fn the_deck_never_grows_past_its_rows() {
        let mut d = deck();
        for i in 0..MAX {
            d.push(
                Level::Warn,
                Some(&format!("k{i}")),
                "b ".repeat(400).trim().to_string(),
                Duration::from_secs(5),
            );
        }
        let pal = Palette::from_theme(&Theme::default());
        assert!(d.lines(80, &pal).len() <= MAX_ROWS);
    }
}
