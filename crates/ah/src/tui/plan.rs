//! The task list: the row above the input, and the whole plan behind `/plan`.

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

/// The plan in a few cells: how far it has got, and the task in hand.
fn brief(plan: &Plan, room: usize) -> Option<String> {
    let (done, total) = plan.counts();
    if total == 0 || done == total {
        return None;
    }
    let mut s = format!("plan {done}/{total}");
    let task = plan.doing().first().copied().or_else(|| plan.next_ready());
    if let Some(t) = task {
        let room = room.saturating_sub(s.chars().count() + 3);
        if room >= 8 {
            s.push_str(" \u{b7} ");
            s.push_str(&cut(&t.title, room));
        }
    }
    Some(s)
}

/// `title` shortened to `room` cells, with an ellipsis when it does not fit.
fn cut(title: &str, room: usize) -> String {
    if title.chars().count() <= room {
        return title.to_string();
    }
    title
        .chars()
        .take(room.saturating_sub(1))
        .collect::<String>()
        + "\u{2026}"
}

fn width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.width()).sum()
}

/// The row above the input: what the turn is doing on the left, the running
/// shell jobs and the plan on the right. `None` when neither side has
/// anything to say.
pub fn dock(
    left: Vec<Span<'static>>,
    plan: &Plan,
    show_plan: bool,
    width_cells: usize,
    pal: &Palette,
) -> Option<Line<'static>> {
    let chip: Vec<Span<'static>> = Vec::new();
    let room = width_cells.saturating_sub(width(&left) + width(&chip) + 2);
    let text = (show_plan && room >= 8)
        .then(|| brief(plan, room))
        .flatten();
    let mut right = chip;
    if let Some(text) = text {
        right.push(Span::styled(text, pal.dim()));
    } else if let Some(last) = right.last()
        && last.content == " "
    {
        right.pop();
    }
    if left.is_empty() && right.is_empty() {
        return None;
    }
    let gap = width_cells
        .saturating_sub(width(&left) + width(&right))
        .max(usize::from(!left.is_empty() && !right.is_empty()));
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    Some(Line::from(spans))
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
    fn the_row_disappears_when_there_is_nothing_left_to_say() {
        let pal = Palette::from_theme(&Theme::default());
        let mut p = plan();
        assert!(dock(vec![], &p, true, 80, &pal).is_some());
        p.set_status(&[2], Status::Done, "").unwrap();
        p.set_status(&[1], Status::Done, "").unwrap();
        assert!(dock(vec![], &p, true, 80, &pal).is_none());
        assert!(dock(vec![], &p, false, 80, &pal).is_none());
        assert!(dock(vec![], &Plan::default(), true, 80, &pal).is_none());
    }

    #[test]
    fn the_turn_takes_the_left_and_the_plan_the_right_edge() {
        let pal = Palette::from_theme(&Theme::default());
        let left = vec![Span::raw("Working")];
        let line = dock(left, &plan(), true, 60, &pal).unwrap();
        assert_eq!(line.width(), 60);
        let text = line.to_string();
        assert!(text.starts_with("Working "), "{text}");
        assert!(text.ends_with("plan 0/2 \u{b7} api"), "{text}");
    }

    #[test]
    fn a_narrow_row_drops_the_plan() {
        let pal = Palette::from_theme(&Theme::default());
        let line = dock(vec![Span::raw("Working")], &plan(), true, 14, &pal).unwrap();
        assert_eq!(line.to_string().trim_end(), "Working");
        // With a little more room the plan comes back, without the task name.
        let line = dock(vec![Span::raw("Working")], &plan(), true, 24, &pal).unwrap();
        assert!(line.to_string().ends_with("plan 0/2"), "{line}");
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
