//! The box the `ask_user` tool puts on screen: the questions it asks, their
//! options, and a row to type an answer of your own. The turn is stopped while
//! it is up, so it takes every key until it is sent or dismissed.

use ah_core::abi::{Answer, Ask, Question};
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

/// A step that cannot be taken back, held for a second key.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Confirm {
    /// Send the answers to the model.
    Send,
    /// Close the box with nothing said.
    Leave,
}

/// What the box wants the app to do after a key.
pub enum Action {
    None,
    /// Answered, in question order.
    Done(Vec<Answer>),
    /// Closed with nothing said.
    Dismiss,
}

/// One question's answer as it is being given. Every question keeps its own,
/// so moving between them loses nothing.
struct Slot {
    /// Highlighted row: an option, or the typing row at `options.len()`.
    sel: usize,
    /// Ticked options. A question that takes one answer ticks one at a time.
    picked: Vec<bool>,
    note: Editor,
}

impl Slot {
    fn answered(&self) -> bool {
        self.picked.iter().any(|p| *p) || !self.note.text.trim().is_empty()
    }
}

pub struct View {
    ask: Ask,
    /// Question being looked at.
    at: usize,
    slots: Vec<Slot>,
    /// Waiting for a second key on something that cannot be undone.
    confirm: Option<Confirm>,
    /// Set when a send found this question unanswered, so the footer says so.
    nag: bool,
    /// First body line drawn, so a long list scrolls instead of overflowing.
    top: usize,
    /// Question text wrapped, and the width and question it was wrapped for.
    wrapped: Vec<String>,
    wrap_for: (u16, usize),
}

impl View {
    /// `None` for an ask with no questions in it, which has nothing to show.
    pub fn new(ask: Ask) -> Option<Self> {
        if ask.questions.is_empty() {
            return None;
        }
        let slots = ask
            .questions
            .iter()
            .map(|q| Slot {
                sel: 0,
                picked: vec![false; q.options.len()],
                note: Editor::default(),
            })
            .collect();
        Some(Self {
            ask,
            at: 0,
            slots,
            confirm: None,
            nag: false,
            top: 0,
            wrapped: Vec::new(),
            wrap_for: (0, usize::MAX),
        })
    }

    fn question(&self) -> &Question {
        &self.ask.questions[self.at]
    }

    fn slot(&self) -> &Slot {
        &self.slots[self.at]
    }

    fn slot_mut(&mut self) -> &mut Slot {
        &mut self.slots[self.at]
    }

    fn on_note(&self) -> bool {
        self.slot().sel >= self.question().options.len()
    }

    /// True while there is text under the cursor to move through, which is
    /// what decides whether Left and Right move in it or between questions.
    fn editing(&self) -> bool {
        self.on_note() && !self.slot().note.text.is_empty()
    }

    /// Tick row `i`. A question that takes one answer keeps one tick.
    fn pick(&mut self, i: usize) {
        let multi = self.question().multi;
        let slot = self.slot_mut();
        let Some(&was) = slot.picked.get(i) else {
            return;
        };
        if !multi {
            slot.picked.fill(false);
        }
        slot.picked[i] = !was;
        slot.sel = i;
        self.nag = false;
    }

    /// Look at question `to`, clamped to the ones there are.
    fn go(&mut self, to: usize) {
        let to = to.min(self.ask.questions.len() - 1);
        if to != self.at {
            self.at = to;
            self.top = 0;
            self.nag = false;
        }
    }

    /// The first question still without an answer.
    fn unanswered(&self) -> Option<usize> {
        self.slots.iter().position(|s| !s.answered())
    }

    fn answers(&self) -> Vec<Answer> {
        self.ask
            .questions
            .iter()
            .zip(&self.slots)
            .map(|(q, s)| Answer {
                picked: q
                    .options
                    .iter()
                    .zip(&s.picked)
                    .filter(|(_, p)| **p)
                    .map(|(o, _)| o.label.clone())
                    .collect(),
                note: s.note.text.trim().to_string(),
            })
            .collect()
    }

    /// Enter: take the highlighted option when the question takes one answer
    /// and has none yet, then send if every question is answered and go to the
    /// first that is not if any is left. A question that takes several answers
    /// is never ticked for the user: leaving them all off may be the point.
    fn enter(&mut self) -> Action {
        if !self.question().multi && !self.slot().answered() && !self.on_note() {
            let i = self.slot().sel;
            self.pick(i);
        }
        match self.unanswered() {
            Some(i) => {
                self.go(i);
                self.nag = true;
            }
            None => self.confirm = Some(Confirm::Send),
        }
        Action::None
    }

    /// A key while something is waiting to be confirmed. Enter (or `y`) goes
    /// through with it; anything else puts the box back as it was.
    fn confirmed(&mut self, k: KeyEvent, c: Confirm) -> Action {
        let yes = matches!(k.code, KeyCode::Enter | KeyCode::Char('y' | 'Y'));
        self.confirm = None;
        match (yes, c) {
            (true, Confirm::Send) => Action::Done(self.answers()),
            (true, Confirm::Leave) => Action::Dismiss,
            (false, _) => Action::None,
        }
    }

    pub fn paste(&mut self, s: &str) {
        let to = self.question().options.len();
        let text = s.replace('\n', " ");
        let slot = self.slot_mut();
        slot.sel = to;
        slot.note.insert_str(&text);
        self.nag = false;
    }

    pub fn key(&mut self, k: KeyEvent) -> Action {
        if let Some(c) = self.confirm {
            return self.confirmed(k, c);
        }
        let last = self.question().options.len();
        let on_note = self.on_note();
        let editing = self.editing();
        let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _) => {
                self.confirm = Some(Confirm::Leave);
                self.nag = false;
            }
            (KeyCode::Enter, _) => return self.enter(),
            (KeyCode::Up, _) => self.slot_mut().sel = self.slot().sel.saturating_sub(1),
            (KeyCode::Down, _) => self.slot_mut().sel = (self.slot().sel + 1).min(last),
            // Left and Right walk the questions, unless there is text under
            // the cursor, where they walk that instead. Tab always walks the
            // questions, so the typing row is never a dead end.
            (KeyCode::Left, _) if editing => self.slot_mut().note.left(),
            (KeyCode::Right, _) if editing => self.slot_mut().note.right(),
            (KeyCode::Left | KeyCode::BackTab, _) => self.go(self.at.saturating_sub(1)),
            (KeyCode::Right | KeyCode::Tab, _) => self.go(self.at + 1),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => self.slot_mut().note.delete_all(),
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => self.slot_mut().note.delete_word(),
            (KeyCode::Backspace, m) if m.contains(KeyModifiers::CONTROL) => {
                self.slot_mut().note.delete_word()
            }
            (KeyCode::Backspace, _) => self.slot_mut().note.backspace(),
            (KeyCode::Delete, _) => self.slot_mut().note.delete(),
            (KeyCode::Home, _) => self.slot_mut().note.home(),
            (KeyCode::End, _) => self.slot_mut().note.end(),
            // A digit goes to the row it numbers, the typing row included, but
            // only while it cannot be part of what is being typed.
            (KeyCode::Char(c), m)
                if c.is_ascii_digit()
                    && !m.intersects(ctrl_alt)
                    && self.slot().note.is_empty()
                    && (1..=last + 1).contains(&(c as usize - '0' as usize)) =>
            {
                let i = c as usize - '0' as usize - 1;
                if i == last {
                    self.slot_mut().sel = i;
                } else {
                    self.pick(i);
                }
            }
            (KeyCode::Char(' '), m) if !on_note && !m.intersects(ctrl_alt) => {
                self.pick(self.slot().sel)
            }
            // Anything else typed is the start of an answer of your own,
            // wherever the highlight was.
            (KeyCode::Char(c), m) if !m.intersects(ctrl_alt) => {
                self.slot_mut().sel = last;
                self.slot_mut().note.insert_char(c);
                self.nag = false;
            }
            _ => {}
        }
        Action::None
    }

    /// Draws the box and returns where the cursor belongs, if anywhere.
    pub fn draw(&mut self, f: &mut Frame, area: Rect, pal: &Palette) -> Option<(u16, u16)> {
        let q = &self.ask.questions[self.at];
        let sel = self.slots[self.at].sel.min(q.options.len());
        let widest = q
            .options
            .iter()
            .map(|o| o.label.width().max(o.description.width() + 2) + 8)
            .max()
            .unwrap_or(0)
            // Wide enough for the longest thing the footer says, so a hint is
            // never cut in half.
            .max(52) as u16;
        let width = widest.clamp(40, 78).min(area.width);
        let text_w = width.saturating_sub(4) as usize;
        if self.wrap_for != (width, self.at) {
            self.wrapped = wrap(&q.question, text_w.max(8));
            self.wrap_for = (width, self.at);
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
        let picked = &self.slots[self.at].picked;
        let note = &self.slots[self.at].note;
        let row = |i: usize, ticked: bool, text: Span<'static>, lines: &mut Vec<Line<'static>>| {
            let mark = match (q.multi, ticked, i == sel) {
                (true, true, _) => "[x] ",
                (true, false, _) => "[ ] ",
                (false, true, _) => "● ",
                (false, false, true) => "▸ ",
                (false, false, false) => "  ",
            };
            let num = if i < 9 {
                format!(" {} ", i + 1)
            } else {
                "   ".into()
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {num}"), pal.dim()),
                Span::styled(
                    mark,
                    if i == sel {
                        pal.bold(pal.accent)
                    } else {
                        Style::default().fg(pal.fg)
                    },
                ),
                text,
            ]));
        };
        for (i, o) in q.options.iter().enumerate() {
            let style = if i == sel {
                pal.bold(pal.accent)
            } else {
                Style::default().fg(pal.fg)
            };
            if i == sel {
                focus = lines.len();
            }
            row(
                i,
                picked[i],
                Span::styled(o.label.clone(), style),
                &mut lines,
            );
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
        let on_note = sel == q.options.len();
        if on_note {
            focus = note_line;
        }
        let typed = !note.text.is_empty();
        let text = if typed {
            Span::styled(note.text.clone(), Style::default().fg(pal.fg))
        } else {
            Span::styled(
                if q.options.is_empty() {
                    "type an answer"
                } else {
                    "something else"
                },
                pal.dim(),
            )
        };
        row(q.options.len(), typed, text, &mut lines);

        let n = self.ask.questions.len();
        let foot = if let Some(c) = self.confirm {
            Span::styled(
                match c {
                    Confirm::Send => "send these answers? enter yes · esc goes back",
                    Confirm::Leave => "leave the question? enter yes · esc goes back",
                },
                pal.bold(pal.accent),
            )
        } else if self.nag {
            // The nudge goes where the hint was, so the box does not jump.
            Span::styled(
                if n > 1 {
                    format!("question {} of {n} still needs an answer", self.at + 1)
                } else if q.options.is_empty() {
                    "type an answer, or esc to dismiss".to_string()
                } else {
                    "pick an option or type an answer".to_string()
                },
                Style::default().fg(pal.error),
            )
        } else {
            Span::styled(
                match (n > 1, q.options.is_empty()) {
                    (true, _) => "←→ question · space picks · enter sends".to_string(),
                    (false, true) => "type an answer · enter sends · esc dismisses".to_string(),
                    (false, false) => "↑↓ or 1-9 · space picks · enter sends".to_string(),
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
        let answered = self.slots.iter().filter(|s| s.answered()).count();
        let title = match (n, q.header.trim()) {
            (1, "") => " question ".to_string(),
            (1, h) => format!(" {h} "),
            (n, "") => format!(" question {}/{n} · {answered} answered ", self.at + 1),
            (n, h) => format!(" {h} · {}/{n} · {answered} answered ", self.at + 1),
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
        if !on_note || row >= body {
            return None;
        }
        let before: String = note.text.chars().take(note.cursor).collect();
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

    /// Confirm the send, which is what hands the answers over.
    fn done(v: &mut View) -> Vec<Answer> {
        assert_eq!(
            v.confirm,
            Some(Confirm::Send),
            "the box asks before it sends"
        );
        match v.key(key(KeyCode::Enter)) {
            Action::Done(a) => a,
            _ => panic!("the confirmation did not send"),
        }
    }

    /// Answer everything that is answerable the quick way, then send.
    fn send(v: &mut View) -> Vec<Answer> {
        v.key(key(KeyCode::Enter));
        done(v)
    }

    #[test]
    fn a_number_ticks_the_option_it_numbers() {
        let mut v = view(vec![question("Which?", &["a", "b", "c"], false)]);
        v.key(key(KeyCode::Char('2')));
        let a = send(&mut v);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].picked, ["b"]);
        assert!(a[0].note.is_empty());
    }

    #[test]
    fn the_highlight_moves_and_enter_takes_what_it_is_on() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        v.key(key(KeyCode::Down));
        assert_eq!(v.slot().sel, 1);
        // Down stops at the typing row rather than wrapping round.
        v.key(key(KeyCode::Down));
        v.key(key(KeyCode::Down));
        assert_eq!(v.slot().sel, 2);
        v.key(key(KeyCode::Up));
        let a = send(&mut v);
        assert_eq!(a[0].picked, ["b"], "enter took the highlighted row");
    }

    #[test]
    fn one_answer_replaces_another_when_the_question_takes_one() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        v.key(key(KeyCode::Char('1')));
        v.key(key(KeyCode::Char('2')));
        let a = send(&mut v);
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
        let a = send(&mut v);
        assert_eq!(a[0].picked, ["a", "c"], "in the order they were offered");
    }

    #[test]
    fn typing_anywhere_starts_an_answer_of_your_own() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        typed(&mut v, "neither, 2 is closer");
        assert!(v.on_note(), "typing moved to the typing row");
        let a = send(&mut v);
        assert_eq!(a[0].note, "neither, 2 is closer");
        assert!(a[0].picked.is_empty(), "a digit in text is text");
    }

    #[test]
    fn a_choice_can_be_qualified_as_well_as_made() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        typed(&mut v, "with care");
        v.key(key(KeyCode::Up));
        v.key(key(KeyCode::Up));
        assert_eq!(v.slot().sel, 0);
        v.key(key(KeyCode::Char(' ')));
        let a = send(&mut v);
        assert_eq!(a[0].picked, ["a"]);
        assert_eq!(a[0].note, "with care");
    }

    #[test]
    fn a_question_with_no_options_wants_text_and_says_so() {
        let mut v = view(vec![question("What name?", &[], false)]);
        assert!(v.on_note());
        assert!(matches!(v.key(key(KeyCode::Enter)), Action::None));
        assert!(v.nag, "enter with nothing said asks again");
        assert_eq!(v.confirm, None);
        typed(&mut v, "ah");
        assert!(!v.nag);
        let a = send(&mut v);
        assert_eq!(a[0].note, "ah");
    }

    #[test]
    fn nothing_ticked_is_not_an_answer() {
        let mut v = view(vec![question("Which?", &["a", "b"], true)]);
        assert!(matches!(v.key(key(KeyCode::Enter)), Action::None));
        assert!(v.nag);
        assert_eq!(v.confirm, None, "nothing to send yet");
        v.key(key(KeyCode::Char(' ')));
        let a = send(&mut v);
        assert_eq!(a[0].picked, ["a"]);
    }

    #[test]
    fn every_question_is_answered_before_any_of_them_are_sent() {
        let mut v = view(vec![
            question("First?", &["a", "b"], false),
            question("Second?", &[], false),
            question("Third?", &["x"], true),
        ]);
        v.key(key(KeyCode::Char('1')));
        // Enter on the last unanswered question goes to it instead of sending.
        assert!(matches!(v.key(key(KeyCode::Enter)), Action::None));
        assert_eq!(v.at, 1);
        assert!(v.nag);
        assert_eq!(v.confirm, None);
        typed(&mut v, "second");
        v.key(key(KeyCode::Enter));
        assert_eq!(v.at, 2, "and on to the one still missing");
        assert!(v.nag);
        v.key(key(KeyCode::Char(' ')));
        let a = send(&mut v);
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].picked, ["a"]);
        assert_eq!(a[1].note, "second");
        assert_eq!(a[2].picked, ["x"]);
    }

    #[test]
    fn the_question_that_is_missing_an_answer_is_named() {
        let mut v = view(vec![
            question("First?", &["a"], false),
            question("Second?", &["b"], false),
            question("Third?", &["c"], false),
        ]);
        v.key(key(KeyCode::Char('1')));
        v.key(key(KeyCode::Right));
        v.key(key(KeyCode::Char('1')));
        v.key(key(KeyCode::Enter));
        assert_eq!(v.at, 2);
        let (text, _) = drawn(&mut v, 70, 20);
        assert!(
            text.contains("question 3 of 3 still needs an answer"),
            "{text}"
        );
    }

    #[test]
    fn the_questions_can_be_walked_and_each_keeps_its_own_answer() {
        let mut v = view(vec![
            question("First?", &["a", "b"], false),
            question("Second?", &["c", "d"], true),
        ]);
        v.key(key(KeyCode::Char('2')));
        v.key(key(KeyCode::Right));
        assert_eq!(v.at, 1);
        typed(&mut v, "own answer");
        // Right at the last question stays there rather than wrapping round.
        v.key(key(KeyCode::Tab));
        assert_eq!(v.at, 1);
        v.key(key(KeyCode::BackTab));
        assert_eq!(v.at, 0, "shift-tab walks back too");
        assert_eq!(v.slot().sel, 1, "the highlight was left where it was");
        assert!(v.slot().picked[1], "and the tick with it");
        v.key(key(KeyCode::Left));
        assert_eq!(v.at, 0, "left at the first question stays there");
        v.key(key(KeyCode::Right));
        assert_eq!(v.slot().note.text, "own answer", "and the text with it");
        let a = send(&mut v);
        assert_eq!(a[0].picked, ["b"]);
        assert_eq!(a[1].note, "own answer");
    }

    #[test]
    fn arrows_move_through_what_is_typed_before_they_move_questions() {
        let mut v = view(vec![
            question("First?", &["a"], false),
            question("Second?", &["b"], false),
        ]);
        typed(&mut v, "abc");
        v.key(key(KeyCode::Left));
        assert_eq!(v.at, 0, "the cursor moved, not the question");
        assert_eq!(v.slot().note.cursor, 2);
        // With the text emptied there is nothing to move through.
        v.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        v.key(key(KeyCode::Right));
        assert_eq!(v.at, 1);
    }

    #[test]
    fn esc_asks_before_it_throws_the_answers_away() {
        let mut v = view(vec![question("Which?", &["a"], false)]);
        typed(&mut v, "half an answer");
        assert!(matches!(v.key(key(KeyCode::Esc)), Action::None));
        assert_eq!(v.confirm, Some(Confirm::Leave));
        // Esc again is the way back, not a second yes.
        assert!(matches!(v.key(key(KeyCode::Esc)), Action::None));
        assert_eq!(v.confirm, None);
        assert_eq!(v.slot().note.text, "half an answer", "nothing was lost");
        v.key(key(KeyCode::Esc));
        assert!(matches!(v.key(key(KeyCode::Enter)), Action::Dismiss));
    }

    #[test]
    fn going_back_from_the_send_changes_nothing() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        v.key(key(KeyCode::Char('1')));
        v.key(key(KeyCode::Enter));
        assert_eq!(v.confirm, Some(Confirm::Send));
        // Second thoughts: back to the question, with the answer still there.
        assert!(matches!(v.key(key(KeyCode::Esc)), Action::None));
        assert!(v.slot().picked[0]);
        v.key(key(KeyCode::Char('2')));
        let a = send(&mut v);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].picked, ["b"]);
    }

    #[test]
    fn editing_keys_reach_the_typed_answer() {
        let mut v = view(vec![question("What?", &[], false)]);
        typed(&mut v, "one two");
        v.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(v.slot().note.text, "one ");
        v.key(key(KeyCode::Backspace));
        assert_eq!(v.slot().note.text, "one");
        v.key(key(KeyCode::Left));
        v.key(key(KeyCode::Char('x')));
        assert_eq!(v.slot().note.text, "onxe");
        v.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(v.slot().note.is_empty());
        v.paste("pasted\nline");
        assert_eq!(v.slot().note.text, "pasted line");
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
        assert!(text.contains("enter sends"), "{text}");
        assert_eq!(cursor, None, "the highlight starts on an option");
        typed(&mut v, "duckdb");
        let (text, cursor) = drawn(&mut v, 80, 24);
        assert!(text.contains("duckdb"), "{text}");
        let (x, y) = cursor.expect("cursor on the typing row");
        // Just past what has been typed.
        assert!(x > 0 && y > 0, "{cursor:?}");
    }

    #[test]
    fn the_typing_row_is_the_last_row_of_the_list() {
        let mut v = view(vec![question("Which?", &["a", "b"], false)]);
        // The number after the last option goes to it, and typing lands there.
        v.key(key(KeyCode::Char('3')));
        assert!(v.on_note());
        assert!(v.unanswered().is_some(), "going to it answers nothing");
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
    fn an_ask_with_no_questions_is_no_box() {
        assert!(View::new(Ask::default()).is_none());
    }
}
