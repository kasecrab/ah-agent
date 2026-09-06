//! `/usage`: session spend by model plus what OpenRouter reports for the key.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ah_core::abi::Usage;
use ah_core::auth::{Account, ModelSpend};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::theme::Palette;

/// The current model and a modality-icon lookup for the models listed.
pub struct Models<'a> {
    pub current: &'a str,
    pub icons: &'a dyn Fn(&str) -> String,
}

/// Counters for the current session, kept by the TUI as events arrive.
pub struct Stats {
    pub started: Instant,
    pub api_time: Duration,
    pub request_started: Option<Instant>,
    pub requests: u32,
    pub tool_calls: u32,
    pub lines_added: u64,
    pub lines_removed: u64,
    pub by_model: BTreeMap<String, Usage>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            api_time: Duration::ZERO,
            request_started: None,
            requests: 0,
            tool_calls: 0,
            lines_added: 0,
            lines_removed: 0,
            by_model: BTreeMap::new(),
        }
    }
}

impl Stats {
    pub fn request_start(&mut self) {
        self.requests += 1;
        self.request_started = Some(Instant::now());
    }

    /// Close the open request timer, if any.
    pub fn request_end(&mut self) {
        if let Some(t) = self.request_started.take() {
            self.api_time += t.elapsed();
        }
    }

    pub fn add_usage(&mut self, model: &str, u: &Usage) {
        self.by_model.entry(model.to_string()).or_default().add(u);
    }

    pub fn add_diff(&mut self, diff: &str) {
        for l in diff.lines() {
            match l.as_bytes().first() {
                Some(b'+') => self.lines_added += 1,
                Some(b'-') => self.lines_removed += 1,
                _ => {}
            }
        }
    }
}

pub struct Remote {
    pub account: Account,
    pub top: Result<Vec<ModelSpend>, String>,
}

pub struct Pane {
    pub loading: bool,
    pub error: Option<String>,
    pub remote: Option<Remote>,
    pub fetched: Option<Instant>,
}

impl Pane {
    pub fn new() -> Self {
        Self {
            loading: true,
            error: None,
            remote: None,
            fetched: None,
        }
    }

    pub fn draw(
        &self,
        f: &mut Frame,
        area: Rect,
        pal: &Palette,
        usage: &Usage,
        s: &Stats,
        models: &Models<'_>,
    ) {
        let model = models.current;
        let icons = models.icons;
        let dim = pal.dim();
        let key = pal.bold(pal.accent);
        let val = Style::default().fg(pal.fg);
        let row = |k: &str, v: String| -> Line<'static> {
            Line::from(vec![
                Span::styled(format!(" {k:<11}"), key),
                Span::styled(v, val),
            ])
        };
        let sub = |name: &str, cols: String| -> Line<'static> {
            Line::from(vec![
                Span::styled(format!("   {name:<32.32}"), val),
                Span::styled(
                    format!("{:<6}", icons(name)),
                    Style::default().fg(pal.accent),
                ),
                Span::styled(cols, dim),
            ])
        };

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled(format!(" {:<11}", "model"), key),
            Span::styled(format!("{model} "), val),
            Span::styled(icons(model), Style::default().fg(pal.accent)),
        ]));
        let api = s.api_time
            + s.request_started
                .map(|t| t.elapsed())
                .unwrap_or(Duration::ZERO);
        lines.push(row(
            "session",
            format!(
                "{} wall · {} api · {} request{} · {} tool call{} · +{} -{} lines",
                duration(s.started.elapsed()),
                duration(api),
                s.requests,
                plural(s.requests as u64),
                s.tool_calls,
                plural(s.tool_calls as u64),
                s.lines_added,
                s.lines_removed
            ),
        ));
        lines.push(row(
            "cost",
            format!(
                "${:.4} · ↑{} ↓{} tokens",
                usage.cost,
                tokens(usage.prompt_tokens),
                tokens(usage.completion_tokens)
            ),
        ));
        for (m, u) in &s.by_model {
            lines.push(sub(
                m,
                format!(
                    "↑{:>7} ↓{:>7}  ${:.4}",
                    tokens(u.prompt_tokens),
                    tokens(u.completion_tokens),
                    u.cost
                ),
            ));
        }
        lines.push(Line::default());

        match (&self.remote, &self.error) {
            (Some(r), _) => {
                let a = &r.account;
                let mut parts = Vec::new();
                if !a.label.is_empty() {
                    parts.push(format!("key `{}`", a.label));
                }
                match (a.balance, a.total_credits) {
                    (Some(b), Some(t)) => parts.push(format!("balance ${b:.2} of ${t:.2}")),
                    (Some(b), None) => parts.push(format!("balance ${b:.2}")),
                    _ => {}
                }
                if let (Some(l), Some(rem)) = (a.limit, a.limit_remaining) {
                    parts.push(format!("key limit ${rem:.2} left of ${l:.2}"));
                }
                if a.free_tier {
                    parts.push("free tier".into());
                }
                lines.push(row("openrouter", parts.join(" · ")));
                let mut spend = Vec::new();
                if let Some(d) = a.usage_daily {
                    spend.push(format!("today ${d:.2}"));
                }
                if let Some(w) = a.usage_weekly {
                    spend.push(format!("week ${w:.2}"));
                }
                if let Some(m) = a.usage_monthly {
                    spend.push(format!("month ${m:.2}"));
                }
                if let Some(t) = a.total_usage {
                    spend.push(format!("all time ${t:.2}"));
                }
                if !spend.is_empty() {
                    lines.push(row("spent", spend.join(" · ")));
                }
                match &r.top {
                    Ok(top) if top.is_empty() => {
                        lines.push(row("top models", "no activity in the last 30 days".into()))
                    }
                    Ok(top) => {
                        lines.push(row("top models", "last 30 days".into()));
                        let total: f64 = top.iter().map(|m| m.cost).sum();
                        for m in top.iter().take(8) {
                            let share = if total > 0.0 {
                                m.cost / total * 100.0
                            } else {
                                0.0
                            };
                            lines.push(sub(
                                &m.model,
                                format!(
                                    "${:>8.2} {:>4.0}%  {:>6} req  ↑{:>7} ↓{:>7}",
                                    m.cost,
                                    share,
                                    m.requests,
                                    tokens(m.prompt_tokens),
                                    tokens(m.completion_tokens)
                                ),
                            ));
                        }
                    }
                    Err(e) => lines.push(row("top models", format!("unavailable: {e}"))),
                }
            }
            (None, Some(e)) => lines.push(row("openrouter", format!("error: {e}"))),
            (None, None) => lines.push(row("openrouter", "fetching…".into())),
        }

        let foot = match (self.loading, self.fetched) {
            (true, _) => " refreshing…".to_string(),
            (false, Some(t)) => format!(" updated {} ago", duration(t.elapsed())),
            (false, None) => String::new(),
        };
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("{foot} · r refresh · Esc close"),
            dim,
        )));

        let width = (area.width * 9 / 10).clamp(60, 104).min(area.width);
        let height = (lines.len() as u16 + 2).min(area.height);
        let r = Rect {
            x: (area.width - width) / 2,
            y: (area.height - height) / 2,
            width,
            height,
        };
        f.render_widget(Clear, r);
        let block = pal.block(true).title(" usage ");
        let inner = block.inner(r);
        f.render_widget(block, r);
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `1.2k`, `33.5m`.
pub fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// `1h 6m 55s`, `4m 2s`, `12s`.
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m}m {sec}s")
    } else if m > 0 {
        format!("{m}m {sec}s")
    } else {
        format!("{sec}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_234), "1.2k");
        assert_eq!(tokens(33_500_000), "33.5m");
        assert_eq!(duration(Duration::from_secs(4015)), "1h 6m 55s");
        assert_eq!(duration(Duration::from_secs(62)), "1m 2s");
        assert_eq!(duration(Duration::from_secs(9)), "9s");
    }

    #[test]
    fn diff_counts() {
        let mut s = Stats::default();
        s.add_diff("@@ -1,2 +1,3 @@\n a\n-b\n+c\n+d\n");
        assert_eq!((s.lines_added, s.lines_removed), (2, 1));
    }
}
