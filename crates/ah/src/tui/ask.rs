//! The box the `ask_user` tool puts on screen: one question at a time, its
//! options, and a line to type an answer of your own. The turn is stopped
//! while it is up, so it takes every key until it is answered or dismissed.

use ah_core::abi::{Answer, Ask};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::input::Editor;
use super::theme::Palette;
use super::transcript::wrap;

/// What the box wants the app to do after a key.
pub enum Action {
    None,
    /// Answered, in question order.
    Done(Vec<Answer>),
    /// Closed with nothing said.
    Dismiss,
}

pub struct View {
    ask: Ask,
    /// Question being answered.
    at: usize,
    /// Highlighted row: an option, or the typing row at `options.len()`.
    sel: usize,
    /// Ticked options of the current question; empty for a question with none.
    picked: Vec<bool>,
    note: Editor,
    answers: Vec<Answer>,
    /// Set when Enter had nothing to accept, so the footer says so.
    nag: bool,
    /// First body line drawn, so a long list scrolls instead of overflowing.
    top: usize,
    /// Question text wrapped, and the width it was wrapped to.
    wrapped: Vec<String>,
    wrap_w: u16,
}

impl View {
    /// `None` for an ask with no questions in it, which has nothing to show.
    pub fn new(ask: Ask) -> Option<Self> {
        if ask.questions.is_empty() {
            return None;
        }
        let mut v = Self {
            ask,
            at: 0,
            sel: 0,
            picked: Vec::new(),
            note: Editor::default(),
            answers: Vec::new(),
            nag: false,
            top: 0,
            wrapped: Vec::new(),
            wrap_w: 0,
        };
        v.enter();
        Some(v)
    }

    fn question(&self) -> &ah_core::abi::Question {
        &self.ask.questions[self.at]
    }

    /// Reset the per-question state for `self.at`.
    fn enter(&mut self) {
        let n = self.question().options.len();
        self.picked = vec![false; n];
        self.sel = 0;
        self.note.clear();
        self.nag = false;
        self.top = 0;
        self.wrap_w = 0;
    }

    fn on_note(&self) -> bool {
        self.sel >= self.question().options.len()
    }

    fn toggle(&mut self, i: usize) {
        if let Some(p) = self.picked.get_mut(i) {
            *p = !*p;
            self.nag = false;
        }
    }

    /// Take the current question's answer, if there is one, and move on.
    fn accept(&mut self) -> Action {
        let q = self.question();
        let note = self.note.text.trim().to_string();
        let picked: Vec<String> = if q.multi {
            self.picked
                .iter()
                .enumerate()
                .filter(|(_, p)| **p)
                .map(|(i, _)| q.options[i].label.clone())
                .collect()
        } else if self.sel < q.options.len() {
            vec![q.options[self.sel].label.clone()]
        } else {
            Vec::new()
        };
        let answer = Answer { picked, note };
        if answer.is_empty() {
            self.nag = true;
            return Action::None;
        }
        self.answers.push(answer);
        if self.at + 1 == self.ask.questions.len() {
            return Action::Done(std::mem::take(&mut self.answers));
        }
        self.at += 1;
        self.enter();
        Action::None
    }

    pub fn paste(&mut self, s: &str) {
        self.sel = self.question().options.len();
        self.note.insert_str(&s.replace('\n', " "));
        self.nag = false;
    }

    pub fn key(&mut self, k: KeyEvent) -> Action {
        let last = self.question().options.len();
        let multi = self.question().multi;
        let on_note = self.on_note();
        let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _) => return Action::Dismiss,
            (KeyCode::Enter, _) => return self.accept(),
            (KeyCode::Up | KeyCode::BackTab, _) => self.sel = self.sel.saturating_sub(1),
            (KeyCode::Down | KeyCode::Tab, _) => self.sel = (self.sel + 1).min(last),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => self.note.delete_all(),
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => self.note.delete_word(),
            (KeyCode::Backspace, m) if m.contains(KeyModifiers::CONTROL) => self.note.delete_word(),
            (KeyCode::Backspace, _) => self.note.backspace(),
            (KeyCode::Delete, _) => self.note.delete(),
            (KeyCode::Left, _) => self.note.left(),
            (KeyCode::Right, _) => self.note.right(),
            (KeyCode::Home, _) => self.note.home(),
            (KeyCode::End, _) => self.note.end(),
            // A digit picks the row it numbers, the typing row included, but
            // only while it cannot be part of what is being typed.
            (KeyCode::Char(c), m)
                if c.is_ascii_digit()
                    && !m.intersects(ctrl_alt)
                    && self.note.is_empty()
                    && (1..=last + 1).contains(&(c as usize - '0' as usize)) =>
            {
                self.sel = c as usize - '0' as usize - 1;
                if self.sel == last {
                    // The typing row is chosen by going to it, not by picking.
                } else if multi {
                    self.toggle(self.sel);
                } else {
                    return self.accept();
                }
            }
            (KeyCode::Char(' '), m) if !on_note && !m.intersects(ctrl_alt) => {
                if multi {
                    self.toggle(self.sel);
                } else {
                    return self.accept();
                }
            }
            // Anything else typed is the start of an answer of your own,
            // wherever the highlight was.
            (KeyCode::Char(c), m) if !m.intersects(ctrl_alt) => {
                self.sel = last;
                self.note.insert_char(c);
                self.nag = false;
            }
            _ => {}
        }
        Action::None
    }

    /// Draws the box and returns where the cursor belongs, if anywhere.
    pub fn draw(&mut self, f: &mut Frame, area: Rect, pal: &Palette) -> Option<(u16, u16)> {
        let q = &self.ask.questions[self.at];
        let widest = q
            .options
            .iter()
            .map(|o| o.label.width().max(o.description.width() + 2) + 8)
            .max()
            .unwrap_or(0)
            .max(48) as u16;
        let width = widest.clamp(40, 78).min(area.width);
        let text_w = width.saturating_sub(4) as usize;
        if self.wrap_w != width {
            self.wrapped = wrap(&q.question, text_w.max(8));
            self.wrap_w = width;
        }

        // Body lines, and where the highlight sits among them.
        let mut lines: Vec<Line> = Vec::with_capacity(self.wrapped.len() + q.options.len() * 2 + 3);
        for l in &self.wrapped {
            lines.push(Line::from(Span::styled(
                format!(" {l}"),
                Style::default().fg(pal.fg).add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::default());
        // Where a label starts, so its description lines up under it.
        let indent = if q.multi { 8 } else { 6 };
        let mut focus = 0;
        for (i, o) in q.options.iter().enumerate() {
            let mark = match (q.multi, self.picked.get(i), i == self.sel) {
                (true, Some(true), _) => "[x] ",
                (true, _, _) => "[ ] ",
                (false, _, true) => "▸ ",
                (false, _, false) => "  ",
            };
            let num = if i < 9 {
                format!(" {} ", i + 1)
            } else {
                "   ".into()
            };
            let style = if i == self.sel {
                pal.bold(pal.accent)
            } else {
                Style::default().fg(pal.fg)
            };
            if i == self.sel {
                focus = lines.len();
            }
            lines.push(Line::from(vec![
                Span::styled(format!(" {num}"), pal.dim()),
                Span::styled(mark, style),
                Span::styled(o.label.clone(), style),
            ]));
            // `wrap` gives a blank line for a blank description; a row with
            // nothing to say gets no second row at all.
            for l in wrap(o.description.trim(), text_w.saturating_sub(indent).max(8))
                .into_iter()
                .take(2)
                .filter(|l| !l.is_empty())
            {
                lines.push(Line::from(Span::styled(
                    format!("{:indent$}{l}", "", indent = indent),
                    pal.dim(),
                )));
            }
        }
        // The typing row is one of the list: same number, same marker, same
        // column, so it reads as the last answer rather than as a separate
        // field stuck to the bottom of the box.
        let note_line = lines.len();
        if self.on_note() {
            focus = note_line;
        }
        let typed = !self.note.text.is_empty();
        let mark = match (q.multi, typed, self.on_note()) {
            (true, true, _) => "[x] ",
            (true, false, _) => "[ ] ",
            (false, _, true) => "▸ ",
            (false, _, false) => "  ",
        };
        let num = if q.options.len() < 9 {
            format!(" {} ", q.options.len() + 1)
        } else {
            "   ".into()
        };
        let style = if self.on_note() {
            pal.bold(pal.accent)
        } else {
            Style::default().fg(pal.fg)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {num}"), pal.dim()),
            Span::styled(mark, style),
            if typed {
                Span::styled(self.note.text.clone(), Style::default().fg(pal.fg))
            } else {
                Span::styled(
                    if q.options.is_empty() {
                        "type an answer"
                    } else {
                        "something else"
                    },
                    pal.dim(),
                )
            },
        ]));

        let foot = if self.nag {
            // The nudge goes where the hint was, so the box does not jump.
            Span::styled(
                if q.options.is_empty() {
                    "type an answer, or esc to dismiss"
                } else {
                    "pick an option or type an answer"
                },
                Style::default().fg(pal.error),
            )
        } else {
            Span::styled(
                match (q.options.is_empty(), q.multi) {
                    (true, _) => "type an answer · enter accepts · esc dismisses",
                    (false, true) => "space picks · enter accepts · esc dismisses",
                    (false, false) => "↑↓ or 1-9 · enter accepts · esc dismisses",
                },
                pal.dim(),
            )
        };

        let height = (lines.len() as u16 + 3).min(area.height);
        let r = Rect {
            x: (area.width.saturating_sub(width)) / 2,
            y: (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        let title = match (self.ask.questions.len(), q.header.trim()) {
            (1, "") => " question ".to_string(),
            (1, h) => format!(" {h} "),
            (n, "") => format!(" question {}/{n} ", self.at + 1),
            (n, h) => format!(" {h} · {}/{n} ", self.at + 1),
        };
        f.render_widget(Clear, r);
        let block = pal.block(true).title(title);
        let inner = block.inner(r);
        f.render_widget(block, r);

        // Keep the highlighted row on screen when the box cannot hold it all.
        let body = inner.height.saturating_sub(1) as usize;
        if focus < self.top {
            self.top = focus;
        } else if body > 0 && focus >= self.top + body {
            self.top = focus + 1 - body;
        }
        self.top = self.top.min(lines.len().saturating_sub(body));
        let shown: Vec<Line> = lines.into_iter().skip(self.top).take(body).collect();
        f.render_widget(
            Paragraph::new(shown),
            Rect {
                height: body as u16,
                ..inner
            },
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::raw(" "), foot])),
            Rect {
                y: inner.y + inner.height.saturating_sub(1),
                height: 1,
                ..inner
            },
        );

        // The cursor belongs on the typing row, and only while it is on screen.
        let row = note_line.checked_sub(self.top)?;
        if !self.on_note() || row >= body {
            return None;
        }
        let before: String = self.note.text.chars().take(self.note.cursor).collect();
        Some((
            inner.x + (indent + before.width()) as u16,
            inner.y + row as u16,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ah_core::abi::{Choice, Question};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn typed(v: &mut View, s: &str) {
        for c in s.chars() {
            v.key(key(KeyCode::Char(c)));
        }
    }

    fn question(text: &str, opts: &[&str], multi: bool) -> Question {
        Question {
            header: String::new(),
            question: text.into(),
            options: opts
                .iter()
                .map(|l| Choice {
                    label: (*l).into(),
                    description: String::new(),
                })
                .collect(),
            multi,
        }
    }

    fn view(questions: Vec<Question>) -> View {
        View::new(Ask { questions }).expect("questions")
    }

    fn answers(a: Action) -> Vec<Answer> {
        match a {
            Action::Done(v) => v,
            _ => panic!("not done"),
        }
    }

    #[test]
    fn a_number_answers_a_question_in_one_key() {
        let mut v = view(vec![question("Which?", &["a", "b", "c"], false)]);
        let a = answers(v.key(key(KeyCode::Char('2'))));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].picked, ["b"]);
        assert!(a[0].note.is_empty());
    }

    #[test]
    fn the_highlight_moves_and_enter_takes_what_it_is_on() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        v.key(key(KeyCode::Down));
        assert_eq!(v.sel, 1);
        // Down stops at the typing row rather than wrapping round.
        v.key(key(KeyCode::Down));
        v.key(key(KeyCode::Down));
        assert_eq!(v.sel, 2);
        v.key(key(KeyCode::Up));
        let a = answers(v.key(key(KeyCode::Enter)));
        assert_eq!(a[0].picked, ["b"]);
    }

    #[test]
    fn several_options_can_be_ticked_when_the_question_allows_it() {
        let mut v = view(vec![question("Which?", &["a", "b", "c"], true)]);
        v.key(key(KeyCode::Char('1')));
        v.key(key(KeyCode::Char('3')));
        // A tick comes off again.
        v.key(key(KeyCode::Char('3')));
        v.key(key(KeyCode::Char('3')));
        let a = answers(v.key(key(KeyCode::Enter)));
        assert_eq!(a[0].picked, ["a", "c"], "in the order they were offered");
    }

    #[test]
    fn typing_anywhere_starts_an_answer_of_your_own() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        typed(&mut v, "neither, 2 is closer");
        assert!(v.on_note(), "typing moved to the typing row");
        let a = answers(v.key(key(KeyCode::Enter)));
        assert_eq!(a[0].note, "neither, 2 is closer");
        assert!(a[0].picked.is_empty());
        // A digit typed into text stays text rather than picking a row.
        assert!(v.answers.is_empty());
    }

    #[test]
    fn a_choice_can_be_qualified_as_well_as_made() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        typed(&mut v, "with care");
        v.key(key(KeyCode::Up));
        v.key(key(KeyCode::Up));
        assert_eq!(v.sel, 0);
        let a = answers(v.key(key(KeyCode::Enter)));
        assert_eq!(a[0].picked, ["a"]);
        assert_eq!(a[0].note, "with care");
    }

    #[test]
    fn a_question_with_no_options_wants_text_and_says_so() {
        let mut v = view(vec![question("What name?", &[], false)]);
        assert!(v.on_note());
        assert!(matches!(v.key(key(KeyCode::Enter)), Action::None));
        assert!(v.nag, "Enter with nothing said asks again");
        typed(&mut v, "ah");
        assert!(!v.nag);
        let a = answers(v.key(key(KeyCode::Enter)));
        assert_eq!(a[0].note, "ah");
    }

    #[test]
    fn nothing_ticked_is_not_an_answer() {
        let mut v = view(vec![question("Which?", &["a", "b"], true)]);
        assert!(matches!(v.key(key(KeyCode::Enter)), Action::None));
        assert!(v.nag);
        v.key(key(KeyCode::Char(' ')));
        let a = answers(v.key(key(KeyCode::Enter)));
        assert_eq!(a[0].picked, ["a"]);
    }

    #[test]
    fn questions_are_asked_one_after_another() {
        let mut v = view(vec![
            question("First?", &["a", "b"], false),
            question("Second?", &[], false),
        ]);
        assert!(matches!(v.key(key(KeyCode::Char('1'))), Action::None));
        assert_eq!(v.at, 1, "moved on to the second question");
        assert!(v.note.is_empty(), "the typing row starts empty again");
        typed(&mut v, "yes");
        let a = answers(v.key(key(KeyCode::Enter)));
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].picked, ["a"]);
        assert_eq!(a[1].note, "yes");
    }

    fn drawn(v: &mut View, w: u16, h: u16) -> (String, Option<(u16, u16)>) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        let pal = Palette::from_theme(&ah_core::abi::Theme::default());
        let mut cursor = None;
        term.draw(|f| cursor = v.draw(f, f.area(), &pal)).unwrap();
        let buf = term.backend().buffer().clone();
        let text = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, cursor)
    }

    #[test]
    fn the_box_shows_the_question_and_puts_the_cursor_where_you_type() {
        let mut v = view(vec![Question {
            header: "Store".into(),
            question: "Which store should it use?".into(),
            options: vec![
                Choice {
                    label: "sqlite".into(),
                    description: "one file, one writer".into(),
                },
                Choice {
                    label: "postgres".into(),
                    description: String::new(),
                },
            ],
            multi: false,
        }]);
        let (text, cursor) = drawn(&mut v, 80, 24);
        assert!(text.contains("Store"), "{text}");
        assert!(text.contains("Which store should it use?"), "{text}");
        assert!(text.contains("1 ") && text.contains("sqlite"), "{text}");
        assert!(text.contains("one file, one writer"), "{text}");
        assert!(text.contains("enter accepts"), "{text}");
        assert_eq!(cursor, None, "the highlight starts on an option");
        typed(&mut v, "duckdb");
        let (text, cursor) = drawn(&mut v, 80, 24);
        assert!(text.contains("duckdb"), "{text}");
        let (x, y) = cursor.expect("cursor on the typing row");
        // Just past what has been typed.
        assert!(x > 0 && y > 0, "{cursor:?}");
    }

    #[test]
    fn a_box_too_big_for_the_screen_still_draws() {
        let mut v = view(vec![question(
            "A question long enough to need more than one line of a narrow box?",
            &[
                "one", "two", "three", "four", "five", "six", "seven", "eight",
            ],
            true,
        )]);
        for (w, h) in [(20, 6), (40, 8), (10, 3), (200, 60)] {
            let (text, _) = drawn(&mut v, w, h);
            assert_eq!(text.lines().count(), h as usize);
        }
        // The highlight stays on screen as it moves down a list that does not
        // fit, which is what `top` is for.
        for _ in 0..8 {
            v.key(key(KeyCode::Down));
        }
        let (text, _) = drawn(&mut v, 40, 8);
        assert!(text.contains("eight"), "{text}");
    }

    #[test]
    fn the_typing_row_is_the_last_row_of_the_list() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        // The number after the last option goes to it, and typing lands there.
        v.key(key(KeyCode::Char('3')));
        assert!(v.on_note());
        assert!(v.answers.is_empty(), "going to it answers nothing");
        let (text, cursor) = drawn(&mut v, 60, 20);
        let rows: Vec<&str> = text
            .lines()
            .filter(|l| l.contains(" 1 ") || l.contains(" 2 ") || l.contains(" 3 "))
            .collect();
        assert_eq!(rows.len(), 3, "{text}");
        // Columns, not byte offsets: the border and the marker are wide chars.
        let col = |l: &str, want: char| l.chars().position(|c| c == want);
        assert_eq!(
            col(rows[0], 'a'),
            col(rows[2], 's'),
            "same column as the options: {text}"
        );
        assert!(rows[2].contains("▸"), "the highlight is on it: {text}");
        assert!(cursor.is_some(), "and the cursor with it");
    }

    #[test]
    fn an_ask_with_no_questions_is_no_box() {
        assert!(View::new(Ask::default()).is_none());
    }

    #[test]
    fn esc_says_nothing_at_all() {
        let mut v = view(vec![question("Which?", &["a"], false)]);
        typed(&mut v, "half an answer");
        assert!(matches!(v.key(key(KeyCode::Esc)), Action::Dismiss));
    }

    #[test]
    fn editing_keys_reach_the_typed_answer() {
        let mut v = view(vec![question("What?", &[], false)]);
        typed(&mut v, "one two");
        v.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(v.note.text, "one ");
        v.key(key(KeyCode::Backspace));
        assert_eq!(v.note.text, "one");
        v.key(key(KeyCode::Left));
        v.key(key(KeyCode::Char('x')));
        assert_eq!(v.note.text, "onxe");
        v.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(v.note.is_empty());
        v.paste("pasted\nline");
        assert_eq!(v.note.text, "pasted line");
    }
}
