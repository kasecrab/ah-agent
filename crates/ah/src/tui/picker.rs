//! Fuzzy list overlay shared by `/model`, `/resume`, `/effort`, `/favorite`,
//! `/rename` and `/skills`.

use ah_core::models;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::theme::Palette;

pub enum Kind {
    /// `favorite` is set when the pick defines or changes a favorite.
    Model {
        favorite: Option<String>,
    },
    Session,
    /// Reasoning effort; `model` is set when chosen right after a model pick.
    Effort {
        model: Option<String>,
        favorite: Option<String>,
    },
    Favorites,
    /// Free-text name for a favorite; `rename` holds the old name.
    Name {
        rename: Option<String>,
    },
    /// Free-text name for the current session.
    SessionName,
    /// Saved prompts from the skills directories.
    Skills,
    /// Free-text arguments for a skill that uses `$ARGUMENTS`.
    SkillArgs {
        name: String,
    },
    /// List returned by a plugin slash command. The command runs again with
    /// stage `pick` on Enter and, when `preview` is set, with stage
    /// `preview` as the cursor moves.
    Plugin {
        command: String,
        preview: bool,
        /// Last value sent for preview, to skip repeats.
        last: Option<String>,
    },
}

pub struct Row {
    pub id: String,
    /// Text the query is matched against.
    pub search: String,
    pub label: String,
    /// Label colour when the row is not selected.
    pub style: Option<Style>,
    /// Fixed-width trailing columns.
    pub cols: Vec<(String, Style)>,
}

pub enum Action {
    None,
    Close,
    Accept,
    /// A letter shortcut: Ctrl-<letter> (or Delete for `d`) in fuzzy pickers,
    /// a bare letter when `hotkeys` is on. Meaning depends on the kind.
    Key(char),
}

pub struct Picker {
    pub kind: Kind,
    pub title: String,
    pub query: String,
    pub rows: Vec<Row>,
    pub results: Vec<usize>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    /// Shown in the footer when there is nothing else to say.
    pub hint: String,
    /// Letters act as shortcuts instead of filtering (short, fixed lists).
    pub hotkeys: bool,
}

impl Picker {
    pub fn new(kind: Kind, title: &str, query: &str, rows: Vec<Row>) -> Self {
        let mut p = Self {
            kind,
            title: title.into(),
            query: query.into(),
            rows,
            results: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
            hint: String::new(),
            hotkeys: false,
        };
        p.refilter();
        p
    }

    pub fn refilter(&mut self) {
        let idx: Vec<usize> = (0..self.rows.len()).collect();
        let rows = &self.rows;
        self.results = models::rank(&self.query, &idx, |&i| rows[i].search.clone())
            .into_iter()
            .copied()
            .collect();
        self.selected = 0;
    }

    pub fn current(&self) -> Option<&Row> {
        self.results.get(self.selected).map(|&i| &self.rows[i])
    }

    pub fn key(&mut self, k: KeyEvent) -> Action {
        let page = 10;
        let last = self.results.len().saturating_sub(1);
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _) => return Action::Close,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Action::Close,
            (KeyCode::Enter, _) => return Action::Accept,
            (KeyCode::Delete, _) => return Action::Key('d'),
            (KeyCode::Char('j'), _) if self.hotkeys => {
                self.selected = (self.selected + 1).min(last)
            }
            (KeyCode::Char('k'), _) if self.hotkeys => {
                self.selected = self.selected.saturating_sub(1)
            }
            (KeyCode::Char('q'), _) if self.hotkeys => return Action::Close,
            (KeyCode::Char(c), m) if self.hotkeys && !m.intersects(KeyModifiers::ALT) => {
                return Action::Key(c.to_ascii_lowercase());
            }
            (KeyCode::Up, _) | (KeyCode::BackTab, _) => {
                self.selected = self.selected.saturating_sub(1)
            }
            (KeyCode::Down, _) | (KeyCode::Tab, _) => self.selected = (self.selected + 1).min(last),
            (KeyCode::PageUp, _) => self.selected = self.selected.saturating_sub(page),
            (KeyCode::PageDown, _) => self.selected = (self.selected + page).min(last),
            (KeyCode::Home, _) => self.selected = 0,
            (KeyCode::End, _) => self.selected = last,
            (KeyCode::Backspace, _) => {
                self.query.pop();
                self.refilter();
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.query.clear();
                self.refilter();
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                let t = self.query.trim_end().to_string();
                self.query = t
                    .rsplit_once(' ')
                    .map(|(a, _)| format!("{a} "))
                    .unwrap_or_default();
                self.refilter();
            }
            (KeyCode::Char(c), KeyModifiers::CONTROL) if c.is_ascii_lowercase() => {
                return Action::Key(c);
            }
            (KeyCode::Char(c), m) if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.query.push(c);
                self.refilter();
            }
            _ => {}
        }
        Action::None
    }

    pub fn draw(&self, f: &mut Frame, area: Rect, pal: &Palette) {
        let compact = matches!(
            self.kind,
            Kind::Effort { .. } | Kind::Name { .. } | Kind::SessionName | Kind::SkillArgs { .. }
        );
        let plugin = matches!(self.kind, Kind::Plugin { .. });
        let width = if compact {
            48.min(area.width)
        } else if plugin {
            let widest = self
                .rows
                .iter()
                .map(|r| r.label.width() + r.cols.iter().map(|(c, _)| c.width() + 2).sum::<usize>())
                .max()
                .unwrap_or(0) as u16;
            (widest + 8).clamp(30, 90).min(area.width)
        } else {
            (area.width * 9 / 10).clamp(40, 110).min(area.width)
        };
        let height = if compact || self.hotkeys || plugin {
            (self.rows.len() as u16 + 4).clamp(5, area.height)
        } else {
            (area.height * 4 / 5).clamp(8, 40).min(area.height)
        };
        let r = Rect {
            x: (area.width - width) / 2,
            y: (area.height - height) / 2,
            width,
            height,
        };
        f.render_widget(Clear, r);
        let block = pal.block(true).title(format!(" {} ", self.title));
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [q_area, list_area, foot_area] = Layout::vertical([
            Constraint::Length(if self.hotkeys { 0 } else { 1 }),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        if !self.hotkeys {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("> ", pal.bold(pal.accent)),
                    Span::raw(self.query.clone()),
                ])),
                q_area,
            );
            f.set_cursor_position((q_area.x + 2 + self.query.chars().count() as u16, q_area.y));
        }

        let rows = list_area.height as usize;
        let first = self.selected.saturating_sub(rows.saturating_sub(1));
        let cols_w = |r: &Row| r.cols.iter().map(|(c, _)| c.width()).sum::<usize>();
        let lines: Vec<Line> = self
            .results
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(i, &ri)| {
                let row = &self.rows[ri];
                let label_w = (inner.width as usize)
                    .saturating_sub(cols_w(row) + 2)
                    .max(8);
                let mut label: String = row.label.chars().take(label_w).collect();
                let pad = label_w.saturating_sub(label.width());
                label.extend(std::iter::repeat_n(' ', pad));
                let style = if i == self.selected {
                    pal.bold(pal.accent).add_modifier(Modifier::REVERSED)
                } else {
                    row.style.unwrap_or(Style::default().fg(pal.fg))
                };
                let mut spans = vec![Span::styled(format!(" {label} "), style)];
                for (text, st) in &row.cols {
                    spans.push(Span::styled(text.clone(), *st));
                }
                Line::from(spans)
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list_area);

        let foot = match (&self.error, self.loading, self.rows.is_empty()) {
            (Some(e), _, _) => format!(" {e}"),
            (None, true, true) => " loading…".to_string(),
            (None, true, false) => format!(
                " {} of {} · refreshing…",
                self.results.len(),
                self.rows.len()
            ),
            (None, false, true) => format!(" {}", self.hint),
            (None, false, false) => format!(
                " {} of {}  {}",
                self.results.len(),
                self.rows.len(),
                self.hint
            ),
        };
        f.render_widget(Paragraph::new(Span::styled(foot, pal.dim())), foot_area);
    }
}

pub const EFFORTS: &[(&str, &str)] = &[
    ("off", "no reasoning"),
    ("minimal", "a few tokens of thought"),
    ("low", "quick"),
    ("medium", "balanced"),
    ("high", "thorough"),
    ("xhigh", "maximum, slow"),
];

/// `3m ago`, `2h ago`, `5d ago`.
pub fn age(started_ms: u128) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let s = now.saturating_sub(started_ms) / 1000;
    if s < 60 {
        "now".into()
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}
