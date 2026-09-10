//! The agent list, and the view that watches one work.

use std::sync::Arc;

use ah_core::agents::{Child, State};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::jobs::elapsed;
use super::picker::Row;
use super::theme::Palette;

pub fn state_text(child: &Child) -> String {
    match child.state() {
        State::Queued => "queued".into(),
        State::Running => "running".into(),
        State::Done => "done".into(),
        State::Stopped => "stopped".into(),
        State::Cancelled => "stopped".into(),
    }
}

fn style(child: &Child, pal: &Palette) -> Style {
    match child.state() {
        State::Queued => pal.dim(),
        State::Running => Style::default().fg(pal.agent),
        State::Done => Style::default().fg(pal.fg),
        State::Stopped | State::Cancelled => Style::default().fg(pal.error),
    }
}

/// One row per agent, newest first.
pub fn rows(kids: &[Arc<Child>], pal: &Palette) -> Vec<Row> {
    let mut rows: Vec<Row> = kids
        .iter()
        .map(|c| {
            let style = style(c, pal);
            Row {
                id: c.id.to_string(),
                search: format!("{} {} {}", c.id, c.kind, c.task),
                label: format!("{:>3}  {}  {}", c.id, c.kind, one_line(&c.task)),
                style: Some(style),
                cols: vec![
                    (format!("{:<8}", state_text(c)).into(), style),
                    (format!("{:>8}", elapsed(c.duration())).into(), pal.dim()),
                    (format!("{:>4} req", c.requests()).into(), pal.dim()),
                ],
            }
        })
        .collect();
    rows.reverse();
    rows
}

fn one_line(s: &str) -> String {
    s.replace('\n', " ")
}

/// Watches one agent: what it has done, then what it had to say.
pub struct View {
    pub id: u32,
    pub from: usize,
    pub follow: bool,
    /// The version drawn last, to skip redraws that would change nothing.
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

    pub fn scroll(&mut self, lines: i64, total: usize, height: usize) {
        let end = total.saturating_sub(height);
        let cur = if self.follow { end } else { self.from };
        let next = if lines < 0 {
            cur.saturating_sub(lines.unsigned_abs() as usize)
        } else {
            (cur + lines as usize).min(end)
        };
        self.follow = next >= end;
        self.from = next;
    }

    /// Everything worth showing: the steps it took, then its report.
    fn body(&self, child: &Child, pal: &Palette) -> Vec<Line<'static>> {
        let mut out: Vec<Line> = child
            .log(1000)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::default().fg(pal.tool_output))))
            .collect();
        let report = child.report();
        if !report.is_empty() {
            out.push(Line::from(""));
            for l in report.lines() {
                out.push(Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(pal.fg),
                )));
            }
        }
        if out.is_empty() {
            out.push(Line::from(Span::styled("starting…".to_string(), pal.dim())));
        }
        out
    }

    /// Fills the area the transcript would have had: watching an agent is
    /// stepping into it, not looking at it through a window.
    pub fn draw(&mut self, f: &mut Frame, area: Rect, pal: &Palette, child: &Child) {
        let r = area;
        f.render_widget(Clear, r);
        let block = pal.block(true).title(format!(
            " agent {} · {} · {} ",
            child.id,
            child.kind,
            one_line(&child.task)
        ));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [body, foot] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);

        let all = self.body(child, pal);
        let rows = body.height as usize;
        self.scroll(0, all.len(), rows);
        let shown: Vec<Line> = all.into_iter().skip(self.from).take(rows).collect();
        self.seen = child.version();
        f.render_widget(Paragraph::new(shown), body);

        let u = child.usage();
        let mut parts = vec![
            state_text(child),
            elapsed(child.duration()),
            format!("{} requests", child.requests()),
            format!("{} tools", child.tool_calls()),
            format!("${:.4}", u.cost),
            child.model.clone(),
        ];
        parts.push(
            if child.running() {
                "type to tell it more · Esc goes back · /agents stops it"
            } else {
                "type to set it off again · Esc goes back"
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
    fn watching_sticks_to_the_newest_line() {
        let mut v = View::new(3);
        v.scroll(-10, 100, 20);
        assert!(!v.follow);
        assert_eq!(v.from, 70);
        v.scroll(-100, 100, 20);
        assert_eq!(v.from, 0);
        v.scroll(1000, 100, 20);
        assert!(v.follow);
        assert_eq!(v.from, 80);
    }
}
