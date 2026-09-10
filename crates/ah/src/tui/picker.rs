//! Fuzzy list overlay shared by `/model`, `/resume`, `/effort`, `/favorite`,
//! `/rename` and `/skills`.

use std::borrow::Cow;
use std::cmp::Reverse;

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
    /// Models that take audio input, for `/voice model`.
    VoiceModel,
    /// Who does the transcribing: Deepgram's socket or an OpenRouter model.
    VoiceProvider,
    /// Free text for a Deepgram API key. Typed blind.
    VoiceKey,
    /// Input devices, for `/voice devices`.
    VoiceDevice,
    /// Background shell jobs. Enter opens the output view.
    Jobs,
    /// Pictures the model drew. Enter opens one in the desktop viewer.
    Images,
    /// What the status line shows. Space or Enter turns a row on or off.
    Statusline,
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
    /// Fixed-width trailing columns. Borrowed where the text is fixed — the
    /// model list draws two dozen single-character columns per row, and six
    /// hundred rows of those would be twelve thousand tiny allocations every
    /// time the picker opens.
    pub cols: Vec<(Cow<'static, str>, Style)>,
}

/// One category in a picker's tab row. `mask` of 0 matches every row.
pub struct Tab {
    /// What the category is called: what `/model <name>` takes and what is
    /// remembered between openings.
    pub name: String,
    /// What the row shows, which may be shorter — a tab row that spells out
    /// "transcription" has no room left for the counts.
    pub label: String,
    pub mask: u16,
    pub count: usize,
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
    /// What is typed is a secret: show its length, not its characters, and
    /// never let it onto the screen where a screenshot would keep it.
    pub secret: bool,
    /// Scored candidates, kept between keystrokes so filtering a catalogue of
    /// six hundred models allocates nothing after the first letter.
    scratch: Vec<(u32, usize)>,
    /// Categories the list can be narrowed to, empty for a list with none.
    pub tabs: Vec<Tab>,
    /// Which of them is showing.
    pub tab: usize,
    /// One mask per row, in row order. Kept beside the rows rather than in
    /// them so every other picker carries no tab machinery at all.
    row_tabs: Vec<u16>,
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
            secret: false,
            scratch: Vec::new(),
            tabs: Vec::new(),
            tab: 0,
            row_tabs: Vec::new(),
        };
        p.refilter();
        p
    }

    /// Give the list categories. `row_tabs` is one mask per row, in row order;
    /// a mismatched length turns the tabs off rather than filtering by the
    /// wrong row. `active` is clamped into range.
    pub fn set_tabs(&mut self, mut tabs: Vec<Tab>, row_tabs: Vec<u16>, active: usize) {
        if row_tabs.len() != self.rows.len() {
            // Filtering on masks that do not line up with the rows would hide
            // the wrong ones. No tabs is a worse list, not a wrong one.
            return;
        }
        for t in &mut tabs {
            t.count = if t.mask == 0 {
                row_tabs.len()
            } else {
                row_tabs.iter().filter(|m| *m & t.mask != 0).count()
            };
        }
        self.tab = active.min(tabs.len().saturating_sub(1));
        self.tabs = tabs;
        self.row_tabs = row_tabs;
        self.refilter();
    }

    /// A category that would match the current query, when this one does not.
    /// Only asked when the list is empty, so the extra pass over the rows is
    /// paid for exactly when there is nothing else to show.
    fn tab_with_matches(&self) -> Option<(&Tab, usize)> {
        let mut hit = 0u16;
        for (i, row) in self.rows.iter().enumerate() {
            if models::fuzzy_score(&self.query, &row.search).is_some() {
                hit |= self.row_tabs.get(i).copied().unwrap_or(0);
            }
        }
        let tab = self
            .tabs
            .iter()
            .find(|t| t.mask != 0 && t.mask & hit != 0)?;
        // How many match in there, which is not how many are in there.
        let n = self
            .rows
            .iter()
            .enumerate()
            .filter(|(i, row)| {
                self.row_tabs.get(*i).is_some_and(|m| m & tab.mask != 0)
                    && models::fuzzy_score(&self.query, &row.search).is_some()
            })
            .count();
        Some((tab, n))
    }

    /// The mask the visible rows must match; 0 when everything shows.
    fn mask(&self) -> u16 {
        self.tabs.get(self.tab).map(|t| t.mask).unwrap_or(0)
    }

    pub fn tab_name(&self) -> Option<&str> {
        self.tabs.get(self.tab).map(|t| t.name.as_str())
    }

    /// Move to another category, keeping the highlighted row when it is in
    /// there too — walking past a tab should not lose your place.
    fn cycle_tab(&mut self, delta: isize) {
        if self.tabs.len() < 2 {
            return;
        }
        let held = self.current().map(|r| r.id.clone());
        let n = self.tabs.len() as isize;
        self.tab = (self.tab as isize + delta).rem_euclid(n) as usize;
        self.refilter();
        if let Some(id) = held
            && let Some(i) = self.results.iter().position(|&r| self.rows[r].id == id)
        {
            self.selected = i;
        }
    }

    pub fn refilter(&mut self) {
        // Scored in place rather than through `models::rank`, which would want
        // a vector of indices and a key string per row every time a letter is
        // typed. Both buffers here are reused.
        let mask = self.mask();
        self.scratch.clear();
        for (i, row) in self.rows.iter().enumerate() {
            if mask != 0 && self.row_tabs.get(i).is_none_or(|m| m & mask == 0) {
                continue;
            }
            if let Some(score) = models::fuzzy_score(&self.query, &row.search) {
                self.scratch.push((score, i));
            }
        }
        // Stable, so rows of equal score keep the order they were given in.
        self.scratch.sort_by_key(|(score, _)| Reverse(*score));
        self.results.clear();
        self.results.extend(self.scratch.iter().map(|(_, i)| *i));
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
            (KeyCode::Char(' '), _) if matches!(self.kind, Kind::Statusline) => {
                return Action::Key(' ');
            }
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
            (KeyCode::Left, _) if !self.tabs.is_empty() => self.cycle_tab(-1),
            (KeyCode::Right, _) if !self.tabs.is_empty() => self.cycle_tab(1),
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

    /// The row of categories. Counts go first; when they do not fit they are
    /// dropped, and when the names alone still do not fit the row is windowed
    /// around the active one, which must always be visible.
    fn tab_line(&self, width: usize, pal: &Palette) -> Line<'static> {
        let width = width.max(1);
        for counts in [true, false] {
            let text = |t: &Tab| {
                if counts {
                    format!("{} {}", t.label, t.count)
                } else {
                    t.label.clone()
                }
            };
            if self.row_width(&text, 0, self.tabs.len()) <= width {
                return Line::from(self.spans(&text, 0, self.tabs.len(), pal));
            }
            if !counts {
                // Widen a window around the active tab until it fills the row.
                let (mut first, mut last) = (self.tab, self.tab + 1);
                loop {
                    let take_next = last < self.tabs.len()
                        && (first == 0 || last - self.tab <= self.tab - first);
                    let (f, l) = if take_next {
                        (first, last + 1)
                    } else if first > 0 {
                        (first - 1, last)
                    } else {
                        break;
                    };
                    if self.row_width(&text, f, l) > width {
                        break;
                    }
                    first = f;
                    last = l;
                }
                return Line::from(self.spans(&text, first, last, pal));
            }
        }
        Line::default()
    }

    /// Exactly what [`Picker::spans`] will occupy, so the fit test and the
    /// drawing cannot disagree.
    fn row_width(&self, text: &dyn Fn(&Tab) -> String, first: usize, last: usize) -> usize {
        let mut w = 1; // leading space
        for (i, t) in self.tabs.iter().enumerate().take(last).skip(first) {
            if i > first {
                w += 3; // " · "
            }
            // Only the active tab is padded, so its highlight has room.
            w += text(t).width() + if i == self.tab { 2 } else { 0 };
        }
        w
    }

    fn spans(
        &self,
        text: &dyn Fn(&Tab) -> String,
        first: usize,
        last: usize,
        pal: &Palette,
    ) -> Vec<Span<'static>> {
        let mut spans = vec![Span::raw(" ")];
        for (i, t) in self.tabs.iter().enumerate().take(last).skip(first) {
            if i > first {
                spans.push(Span::styled(" · ", pal.dim()));
            }
            if i == self.tab {
                spans.push(Span::styled(
                    format!(" {} ", text(t)),
                    pal.bold(pal.accent).add_modifier(Modifier::REVERSED),
                ));
            } else {
                spans.push(Span::styled(text(t), pal.dim()));
            }
        }
        spans
    }

    /// Draws the overlay and returns where the cursor belongs, if anywhere.
    /// The caller places it after the frame is on screen, so it is never seen
    /// on its way there.
    pub fn draw(&self, f: &mut Frame, area: Rect, pal: &Palette) -> Option<(u16, u16)> {
        let compact = matches!(
            self.kind,
            Kind::Effort { .. } | Kind::Name { .. } | Kind::SessionName | Kind::SkillArgs { .. }
        );
        // Boxes sized to their rows, rather than to the screen.
        let snug = matches!(self.kind, Kind::Plugin { .. } | Kind::Statusline);
        let width = if compact {
            48.min(area.width)
        } else if snug {
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
        let tab_row = u16::from(!self.tabs.is_empty());
        let height = if compact || self.hotkeys || snug {
            (self.rows.len() as u16 + 4 + tab_row).clamp(5, area.height)
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
        let [tab_area, q_area, list_area, foot_area] = Layout::vertical([
            Constraint::Length(if self.tabs.is_empty() { 0 } else { 1 }),
            Constraint::Length(if self.hotkeys { 0 } else { 1 }),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        if !self.tabs.is_empty() {
            f.render_widget(
                Paragraph::new(self.tab_line(tab_area.width as usize, pal)),
                tab_area,
            );
        }
        let mut cursor = None;
        if !self.hotkeys {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("> ", pal.bold(pal.accent)),
                    Span::raw(if self.secret {
                        "•".repeat(self.query.chars().count())
                    } else {
                        self.query.clone()
                    }),
                ])),
                q_area,
            );
            cursor = Some((q_area.x + 2 + self.query.chars().count() as u16, q_area.y));
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
            // Nothing here, but something one tab over: say which, rather than
            // let an empty list read as "no such model".
            (None, false, false) if self.results.is_empty() && !self.tabs.is_empty() => {
                match self.tab_with_matches() {
                    Some((t, n)) => format!(
                        " nothing in {} · {n} in {} (→)",
                        self.tabs[self.tab].label, t.label
                    ),
                    None => format!(" no match  {}", self.hint),
                }
            }
            (None, false, false) => format!(
                " {} of {}  {}",
                self.results.len(),
                self.rows.len(),
                self.hint
            ),
        };
        f.render_widget(Paragraph::new(Span::styled(foot, pal.dim())), foot_area);
        cursor
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> Row {
        Row {
            id: id.into(),
            search: id.into(),
            label: id.into(),
            style: None,
            cols: Vec::new(),
        }
    }

    fn tab(name: &str, mask: u16) -> Tab {
        Tab {
            name: name.into(),
            label: name.into(),
            mask,
            count: 0,
        }
    }

    /// text=1, image=2, as the modality table numbers them.
    fn tabbed() -> Picker {
        let rows = vec![row("chatty"), row("drawer"), row("both"), row("embedder")];
        let mut p = Picker::new(Kind::Session, "t", "", rows);
        p.set_tabs(
            vec![
                tab("all", 0),
                tab("text", 1),
                tab("image", 2),
                tab("embed", 4),
            ],
            vec![1, 2, 3, 4],
            1,
        );
        p
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn showing(p: &Picker) -> Vec<&str> {
        p.results.iter().map(|&i| p.rows[i].id.as_str()).collect()
    }

    #[test]
    fn a_category_shows_the_rows_that_belong_to_it() {
        let p = tabbed();
        assert_eq!(p.tab_name(), Some("text"));
        assert_eq!(showing(&p), vec!["chatty", "both"]);
        // Counts are per category, and "all" counts everything.
        let counts: Vec<usize> = p.tabs.iter().map(|t| t.count).collect();
        assert_eq!(counts, vec![4, 2, 2, 1]);
    }

    #[test]
    fn the_arrows_walk_the_categories_and_wrap() {
        let mut p = tabbed();
        p.key(key(KeyCode::Right));
        assert_eq!(p.tab_name(), Some("image"));
        assert_eq!(showing(&p), vec!["drawer", "both"]);
        p.key(key(KeyCode::Left));
        p.key(key(KeyCode::Left));
        assert_eq!(p.tab_name(), Some("all"));
        assert_eq!(showing(&p).len(), 4);
        // Wrapping backwards from the first lands on the last.
        p.key(key(KeyCode::Left));
        assert_eq!(p.tab_name(), Some("embed"));
    }

    #[test]
    fn switching_keeps_the_row_you_were_on_when_it_is_there_too() {
        let mut p = tabbed();
        p.selected = 1; // "both", which is in text and in image
        p.key(key(KeyCode::Right));
        assert_eq!(p.current().map(|r| r.id.as_str()), Some("both"));
        // "both" is not in the embed tab, so the cursor goes back to the top.
        p.tab = 3;
        p.refilter();
        assert_eq!(p.current().map(|r| r.id.as_str()), Some("embedder"));
    }

    #[test]
    fn typing_narrows_within_the_category() {
        let mut p = tabbed();
        p.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
        assert_eq!(showing(&p), vec!["both"]);
        // The query survives a category change.
        p.key(key(KeyCode::Right));
        assert_eq!(p.query, "b");
        assert_eq!(showing(&p), vec!["both"]);
    }

    #[test]
    fn the_row_is_measured_the_way_it_is_drawn() {
        use ah_core::abi::Theme;
        let pal = Palette::from_theme(&Theme::default());
        let mut p = tabbed();
        let text = |t: &Tab| format!("{} {}", t.label, t.count);
        for tab in 0..p.tabs.len() {
            p.tab = tab;
            let drawn: usize = p
                .spans(&text, 0, p.tabs.len(), &pal)
                .iter()
                .map(|s| s.content.width())
                .sum();
            assert_eq!(drawn, p.row_width(&text, 0, p.tabs.len()), "tab {tab}");
        }
    }

    #[test]
    fn a_narrow_row_drops_the_counts_then_keeps_the_active_tab_in_view() {
        use ah_core::abi::Theme;
        let pal = Palette::from_theme(&Theme::default());
        let mut p = tabbed();
        let plain = |l: &Line| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
        // Room for everything: names and counts.
        let wide = plain(&p.tab_line(80, &pal));
        assert!(wide.contains("all 4") && wide.contains("embed 1"), "{wide}");
        // Less room: the counts go first.
        let mid = plain(&p.tab_line(30, &pal));
        assert!(mid.contains("text") && !mid.contains("text 2"), "{mid}");
        // Less still: a window that always holds the one you are on.
        p.tab = 3;
        let narrow = plain(&p.tab_line(12, &pal));
        assert!(narrow.contains("embed"), "{narrow}");
        assert!(
            narrow.width() <= 12,
            "{narrow:?} is {} wide",
            narrow.width()
        );
    }

    #[test]
    fn an_empty_category_says_where_the_matches_are() {
        let mut p = tabbed();
        assert_eq!(p.tab_name(), Some("text"));
        for c in "drawer".chars() {
            p.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(p.results.is_empty(), "not in the text category");
        let (t, n) = p.tab_with_matches().expect("found elsewhere");
        assert_eq!((t.name.as_str(), n), ("image", 1));
        // A query nothing matches anywhere says nothing of the sort.
        for c in "zzz".chars() {
            p.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(p.tab_with_matches().is_none());
    }

    #[test]
    fn a_list_with_no_categories_is_untouched_by_the_arrows() {
        let mut p = Picker::new(Kind::Session, "t", "", vec![row("a"), row("b")]);
        p.key(key(KeyCode::Right));
        assert_eq!(showing(&p), vec!["a", "b"]);
        assert_eq!(p.tab_name(), None);
    }

    #[test]
    fn masks_that_do_not_match_the_rows_are_refused() {
        let mut p = Picker::new(Kind::Session, "t", "", vec![row("a")]);
        // One mask for two rows: filtering on it would hide the wrong row.
        p.set_tabs(vec![tab("all", 0), tab("text", 1)], vec![1, 1], 1);
        assert!(p.tabs.is_empty());
        assert_eq!(showing(&p), vec!["a"]);
    }
}
