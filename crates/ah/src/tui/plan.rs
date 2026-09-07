//! The task list: one line above the input, and the whole plan behind `/plan`.

use ah_core::plan::{Plan, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::theme::Palette;

fn style(status: Status, pal: &Palette) -> Style {
    match status {
        Status::Todo => Style::default().fg(pal.fg),
        Status::Doing => Style::default().fg(pal.accent),
        Status::Done | Status::Dropped => pal.dim(),
    }
}

/// The whole plan, one styled line per task.
pub fn lines(plan: &Plan, pal: &Palette) -> Vec<Line<'static>> {
    plan.ordered()
        .into_iter()
        .filter_map(|(id, depth)| {
            let task = plan.get(id)?;
            Some(Line::from(Span::styled(
                plan.line(id, depth),
                style(task.status, pal),
            )))
        })
        .collect()
}

/// The line above the input while tasks are open; `None` when there is
/// nothing worth a row of the screen.
pub fn summary(plan: &Plan, width: usize, pal: &Palette) -> Option<Line<'static>> {
    let (done, total) = plan.counts();
    if total == 0 || done == total {
        return None;
    }
    let mut text = plan.summary();
    if width > 4 && text.chars().count() > width {
        text = text.chars().take(width - 1).collect::<String>() + "…";
    }
    Some(Line::from(Span::styled(text, pal.dim())))
}

/// Scroll position of the `/plan` overlay.
#[derive(Default)]
pub struct View {
    pub from: usize,
}

impl View {
    pub fn scroll(&mut self, lines: i64, total: usize, height: usize) {
        let end = total.saturating_sub(height);
        self.from = if lines < 0 {
            self.from.saturating_sub(lines.unsigned_abs() as usize)
        } else {
            (self.from + lines as usize).min(end)
        }
        .min(end);
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, pal: &Palette, plan: &Plan) {
        let all = lines(plan, pal);
        let width = (area.width * 9 / 10).clamp(40, 140).min(area.width);
        let height = (all.len() as u16 + 4)
            .clamp(6, area.height.max(6))
            .min(area.height);
        let r = Rect {
            x: (area.width - width) / 2,
            y: (area.height - height) / 2,
            width,
            height,
        };
        f.render_widget(Clear, r);
        let (done, total) = plan.counts();
        let block = pal
            .block(true)
            .title(format!(" plan · {done}/{total} done "));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [body, foot] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
        self.scroll(0, all.len(), body.height as usize);
        let shown: Vec<Line> = all.into_iter().skip(self.from).collect();
        f.render_widget(Paragraph::new(shown), body);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(" {} · Esc closes", plan.summary()),
                pal.dim(),
            )),
            foot,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ah_core::abi::Theme;
    use ah_core::plan::NewTask;

    fn plan() -> Plan {
        let mut p = Plan::default();
        p.set(&[
            NewTask {
                title: "api".into(),
                ..Default::default()
            },
            NewTask {
                title: "routes".into(),
                parent: Some(1),
                ..Default::default()
            },
        ])
        .unwrap();
        p
    }

    #[test]
    fn the_summary_disappears_when_there_is_nothing_left_to_do() {
        let pal = Palette::from_theme(&Theme::default());
        let mut p = plan();
        assert!(summary(&p, 80, &pal).is_some());
        p.set_status(&[2], Status::Done, "").unwrap();
        p.set_status(&[1], Status::Done, "").unwrap();
        assert!(summary(&p, 80, &pal).is_none());
        assert!(summary(&Plan::default(), 80, &pal).is_none());
    }

    #[test]
    fn a_long_summary_is_cut_to_the_width() {
        let pal = Palette::from_theme(&Theme::default());
        let line = summary(&plan(), 20, &pal).unwrap();
        assert_eq!(line.width(), 20);
        assert!(line.to_string().ends_with('…'));
    }

    #[test]
    fn every_task_gets_a_line() {
        let pal = Palette::from_theme(&Theme::default());
        assert_eq!(lines(&plan(), &pal).len(), 2);
    }

    #[test]
    fn scrolling_stops_at_both_ends() {
        let mut v = View::default();
        v.scroll(5, 10, 4);
        assert_eq!(v.from, 5);
        v.scroll(50, 10, 4);
        assert_eq!(v.from, 6);
        v.scroll(-100, 10, 4);
        assert_eq!(v.from, 0);
    }
}
