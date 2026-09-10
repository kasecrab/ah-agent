//! The strip under the status bar: everything running beside the conversation,
//! and the way to move between them.
//!
//! It is only there when there is something to switch to. One row for the
//! background jobs, one for the conversation itself, one per live agent.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::jobs::elapsed;
use super::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Jobs,
    Main,
    Agent(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub target: Target,
    /// What it is: `jobs`, `main`, or the agent type.
    pub name: String,
    /// What it is doing, in a few words.
    pub doing: String,
    /// How long and how much, at the right edge.
    pub right: String,
}

/// What can be switched to right now. Empty when nothing is running: no strip.
pub fn rows() -> Vec<Row> {
    let mut out = Vec::new();
    let jobs = ah_core::jobs::table().owned_by(0);
    let running = jobs.iter().filter(|j| j.running()).count();
    if running > 0 {
        out.push(Row {
            target: Target::Jobs,
            name: "jobs".into(),
            doing: jobs
                .iter()
                .rev()
                .find(|j| j.running())
                .map(|j| j.command.replace('\n', " "))
                .unwrap_or_default(),
            right: format!("{running} running"),
        });
    }
    let kids: Vec<_> = ah_core::agents::table()
        .owned_by(0)
        .into_iter()
        .filter(|c| c.running())
        .collect();
    if kids.is_empty() && running == 0 {
        return Vec::new();
    }
    out.push(Row {
        target: Target::Main,
        name: "main".into(),
        doing: String::new(),
        right: String::new(),
    });
    for c in kids {
        let tokens = c.usage().completion_tokens;
        out.push(Row {
            target: Target::Agent(c.id),
            name: c.kind.clone(),
            doing: match c.activity() {
                a if a.is_empty() => c.task.replace('\n', " "),
                a => a,
            },
            right: format!("{} · ↓{}", elapsed(c.duration()), thousands(tokens)),
        });
    }
    out
}

fn thousands(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        _ => format!("{:.1}k", n as f64 / 1000.0),
    }
}

/// The strip as it is drawn: `active` is what the input is bound to, `cursor`
/// the row the keys are on when the strip has the focus.
pub fn lines(
    rows: &[Row],
    active: Target,
    cursor: Option<usize>,
    width: usize,
    pal: &Palette,
) -> Vec<Line<'static>> {
    let name_w = rows
        .iter()
        .map(|r| r.name.width())
        .max()
        .unwrap_or(4)
        .min(16);
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let on = r.target == active;
            let here = cursor == Some(i);
            let mark = if on { "●" } else { "○" };
            let mut style = match r.target {
                Target::Jobs => Style::default().fg(pal.job),
                Target::Main => Style::default().fg(pal.fg),
                Target::Agent(_) => Style::default().fg(pal.agent),
            };
            if on {
                style = style.add_modifier(Modifier::BOLD);
            }
            let head = format!(" {mark} {:name_w$}  ", r.name);
            let room = width
                .saturating_sub(head.width() + r.right.width() + 2)
                .max(1);
            let doing = cut(&r.doing, room);
            let gap = width
                .saturating_sub(head.width() + doing.width() + r.right.width())
                .max(1);
            let mut spans = vec![
                Span::styled(head, style),
                Span::styled(doing, pal.dim()),
                Span::raw(" ".repeat(gap)),
                Span::styled(r.right.clone(), pal.dim()),
            ];
            if here {
                // The row the keys are on, whichever one the input is bound to.
                spans = spans
                    .into_iter()
                    .map(|s| {
                        let st = s.style.add_modifier(Modifier::REVERSED);
                        Span::styled(s.content, st)
                    })
                    .collect();
            }
            Line::from(spans)
        })
        .collect()
}

fn cut(s: &str, room: usize) -> String {
    let s = s.replace('\n', " ");
    if s.width() <= room {
        return s;
    }
    let mut out = String::new();
    for c in s.chars() {
        if out.width() + 1 >= room {
            break;
        }
        out.push(c);
    }
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ah_core::abi::Theme;

    fn row(target: Target, name: &str, doing: &str) -> Row {
        Row {
            target,
            name: name.into(),
            doing: doing.into(),
            right: "1m 04s · ↓2.1k".into(),
        }
    }

    #[test]
    fn the_bound_row_is_the_marked_one() {
        let pal = Palette::from_theme(&Theme::default());
        let rows = vec![
            row(Target::Main, "main", ""),
            row(Target::Agent(3), "explorer", "reading sse.rs"),
        ];
        let out = lines(&rows, Target::Agent(3), None, 60, &pal);
        assert!(out[0].to_string().starts_with(" ○ main"), "{}", out[0]);
        assert!(out[1].to_string().starts_with(" ● explorer"), "{}", out[1]);

        let out = lines(&rows, Target::Main, None, 60, &pal);
        assert!(out[0].to_string().starts_with(" ● main"), "{}", out[0]);
    }

    #[test]
    fn every_row_fills_the_width_and_keeps_the_right_edge() {
        let pal = Palette::from_theme(&Theme::default());
        let rows = vec![row(
            Target::Agent(3),
            "explorer",
            &"a very long thing it is doing ".repeat(10),
        )];
        let out = lines(&rows, Target::Main, None, 50, &pal);
        let text = out[0].to_string();
        assert!(text.width() <= 50, "{} cells: {text}", text.width());
        assert!(text.ends_with("1m 04s · ↓2.1k"), "{text}");
        assert!(text.contains('…'), "{text}");
    }

    #[test]
    fn nothing_running_means_no_strip() {
        // The tables are process-wide and empty in a fresh test binary.
        assert!(rows().is_empty());
    }
}
