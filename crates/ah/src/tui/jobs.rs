//! Background job list and the live output view behind it.

use std::sync::Arc;

use ah_core::jobs::{Job, State};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::picker::Row;
use super::theme::Palette;

/// `12s`, `3m 04s`, `1h 02m`.
pub fn elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

pub fn state_text(job: &Job) -> String {
    match job.state() {
        State::Running => "running".into(),
        State::Done(0) => "done".into(),
        State::Done(c) => format!("exit {c}"),
    }
}

/// One row per job, newest first.
pub fn rows(jobs: &[Arc<Job>], pal: &Palette) -> Vec<Row> {
    let mut rows: Vec<Row> = jobs
        .iter()
        .map(|j| {
            let (lines, _) = j.counts();
            let style = match j.state() {
                State::Running => Style::default().fg(pal.accent),
                State::Done(0) => Style::default().fg(pal.fg),
                State::Done(_) => Style::default().fg(pal.error),
            };
            Row {
                id: j.id.to_string(),
                search: format!("{} {}", j.id, j.command),
                label: format!("{:>3}  {}", j.id, j.command.replace('\n', " ")),
                style: Some(style),
                cols: vec![
                    (format!("{:<8}", state_text(j)), style),
                    (format!("{:>8}", elapsed(j.duration())), pal.dim()),
                    (format!("{lines:>7} ln"), pal.dim()),
                ],
            }
        })
        .collect();
    rows.reverse();
    rows
}

/// Follows one job's output. `from` is the first line shown once the user
/// scrolls away from the end.
pub struct View {
    pub id: u32,
    pub from: u64,
    pub follow: bool,
    /// Output version drawn last, to skip redraws that would change nothing.
    pub seen: u64,
}

impl View {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            from: 0,
            follow: true,
            seen: 0,
        }
    }

    pub fn scroll(&mut self, lines: i64, total: u64, height: usize) {
        let page = height as u64;
        let end = total.saturating_sub(page);
        let cur = if self.follow { end } else { self.from };
        let next = if lines < 0 {
            cur.saturating_sub(lines.unsigned_abs())
        } else {
            (cur + lines as u64).min(end)
        };
        self.follow = next >= end;
        self.from = next;
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, pal: &Palette, job: &Job) {
        let width = (area.width * 9 / 10).clamp(40, 140).min(area.width);
        let height = (area.height * 4 / 5).clamp(8, 60).min(area.height);
        let r = Rect {
            x: (area.width - width) / 2,
            y: (area.height - height) / 2,
            width,
            height,
        };
        f.render_widget(Clear, r);
        let block = pal.block(true).title(format!(
            " job {} · {} ",
            job.id,
            job.command.replace('\n', " ")
        ));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [body, foot] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

        let rows = body.height as usize;
        let (total, dropped) = job.counts();
        let (lines, _) = if self.follow {
            job.tail(rows)
        } else {
            job.view(self.from, rows)
        };
        self.seen = job.version();
        let text: Vec<Line> = lines
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(pal.tool_output))))
            .collect();
        f.render_widget(Paragraph::new(text), body);

        let mut parts = vec![
            state_text(job),
            elapsed(job.duration()),
            format!("{total} lines"),
        ];
        if dropped > 0 {
            parts.push(format!("{dropped} dropped"));
        }
        parts.push(
            if self.follow {
                "following · Up/PgUp scroll · k stops · Esc closes"
            } else {
                "End follows · k stops · Esc closes"
            }
            .into(),
        );
        f.render_widget(
            Paragraph::new(Span::styled(format!(" {}", parts.join(" · ")), pal.dim())),
            foot,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolling_sticks_to_the_end() {
        let mut v = View::new(1);
        v.scroll(-10, 100, 20);
        assert!(!v.follow);
        assert_eq!(v.from, 70);
        v.scroll(-100, 100, 20);
        assert_eq!(v.from, 0);
        v.scroll(1000, 100, 20);
        assert!(v.follow);
        assert_eq!(v.from, 80);
    }

    #[test]
    fn elapsed_reads_as_time() {
        assert_eq!(elapsed(std::time::Duration::from_secs(9)), "9s");
        assert_eq!(elapsed(std::time::Duration::from_secs(184)), "3m 04s");
        assert_eq!(elapsed(std::time::Duration::from_secs(3720)), "1h 02m");
    }
}
