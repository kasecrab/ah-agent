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

/// The line above the input: how many shell jobs are running, then the plan
/// while tasks are open. `None` when neither has anything to say.
pub fn status_line(
    plan: &Plan,
    jobs: usize,
    show_plan: bool,
    width: usize,
    pal: &Palette,
) -> Option<Line<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    if jobs > 0 {
        let chip = format!(" {jobs} Bash ");
        used += chip.chars().count() + 1;
        spans.push(Span::styled(
            chip,
            Style::default()
                .bg(pal.job)
                .fg(ratatui::style::Color::Black),
        ));
        spans.push(Span::raw(" "));
    }
    let (done, total) = plan.counts();
    if show_plan && total > 0 && done < total {
        let mut text = plan.summary();
        let room = width.saturating_sub(used);
        if room > 4 && text.chars().count() > room {
            text = text.chars().take(room - 1).collect::<String>() + "…";
        }
        spans.push(Span::styled(text, pal.dim()));
    }
    (!spans.is_empty()).then(|| Line::from(spans))
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
    fn the_line_disappears_when_there_is_nothing_left_to_say() {
        let pal = Palette::from_theme(&Theme::default());
        let mut p = plan();
        assert!(status_line(&p, 0, true, 80, &pal).is_some());
        p.set_status(&[2], Status::Done, "").unwrap();
        p.set_status(&[1], Status::Done, "").unwrap();
        assert!(status_line(&p, 0, true, 80, &pal).is_none());
        assert!(status_line(&p, 0, false, 80, &pal).is_none());
        assert!(status_line(&Plan::default(), 0, true, 80, &pal).is_none());
    }

    #[test]
    fn running_jobs_get_a_chip_of_their_own() {
        let pal = Palette::from_theme(&Theme::default());
        let line = status_line(&Plan::default(), 2, true, 80, &pal).unwrap();
        assert_eq!(line.to_string().trim(), "2 Bash");
        // The plan keeps whatever room the chip leaves.
        let line = status_line(&plan(), 3, true, 80, &pal).unwrap();
        assert!(line.to_string().starts_with(" 3 Bash "));
        assert!(line.to_string().contains("0/2 done"));
    }

    #[test]
    fn a_long_summary_is_cut_to_the_width() {
        let pal = Palette::from_theme(&Theme::default());
        let line = status_line(&plan(), 0, true, 20, &pal).unwrap();
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
