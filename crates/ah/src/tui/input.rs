//! Small multi-line editor for the prompt box. Cursor is a char index into
//! `text`; rendering soft-wraps to the box width.

use unicode_width::UnicodeWidthChar;

#[derive(Default, Debug)]
pub struct Editor {
    pub text: String,
    /// Char offset.
    pub cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    draft: String,
    pastes: Vec<String>,
    /// Entry added by the last `take`, not yet written to disk.
    added: Option<String>,
}

/// Entries kept in memory and in the history file after compaction.
pub const HISTORY_KEEP: usize = 1000;

impl Editor {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    pub fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn insert_char(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let b = self.byte_at(self.cursor);
        self.text.insert_str(b, &s);
        self.cursor += s.chars().count();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let b = self.byte_at(self.cursor - 1);
        self.text.remove(b);
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let b = self.byte_at(self.cursor);
        self.text.remove(b);
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_len());
    }

    /// (line index, column in chars) of the cursor.
    pub fn line_col(&self) -> (usize, usize) {
        let mut line = 0;
        let mut col = 0;
        for (i, c) in self.text.chars().enumerate() {
            if i == self.cursor {
                break;
            }
            if c == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for (i, c) in self.text.chars().enumerate() {
            if c == '\n' {
                starts.push(i + 1);
            }
        }
        starts
    }

    pub fn line_count(&self) -> usize {
        self.text.chars().filter(|&c| c == '\n').count() + 1
    }

    /// Returns false when already on the first line (caller may use history).
    pub fn up(&mut self) -> bool {
        let (line, col) = self.line_col();
        if line == 0 {
            return false;
        }
        let starts = self.line_starts();
        let prev_len = starts[line] - starts[line - 1] - 1;
        self.cursor = starts[line - 1] + col.min(prev_len);
        true
    }

    pub fn down(&mut self) -> bool {
        let (line, col) = self.line_col();
        let starts = self.line_starts();
        if line + 1 >= starts.len() {
            return false;
        }
        let next_len = starts
            .get(line + 2)
            .map(|s| s - starts[line + 1] - 1)
            .unwrap_or(self.char_len() - starts[line + 1]);
        self.cursor = starts[line + 1] + col.min(next_len);
        true
    }

    pub fn home(&mut self) {
        let (line, _) = self.line_col();
        self.cursor = self.line_starts()[line];
    }

    pub fn end(&mut self) {
        let (line, _) = self.line_col();
        let starts = self.line_starts();
        self.cursor = starts
            .get(line + 1)
            .map(|s| s - 1)
            .unwrap_or(self.char_len());
    }

    pub fn delete_word(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start_b = self.byte_at(i);
        let end_b = self.byte_at(self.cursor);
        self.text.replace_range(start_b..end_b, "");
        self.cursor = i;
    }

    pub fn delete_to_line_start(&mut self) {
        let (line, _) = self.line_col();
        let start = self.line_starts()[line];
        let start_b = self.byte_at(start);
        let end_b = self.byte_at(self.cursor);
        self.text.replace_range(start_b..end_b, "");
        self.cursor = start;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.hist_pos = None;
        self.pastes.clear();
    }

    /// Take the text for submission and record it in history.
    pub fn take(&mut self) -> String {
        let raw = std::mem::take(&mut self.text);
        let t = self.expand_pastes(&raw);
        self.pastes.clear();
        self.cursor = 0;
        self.hist_pos = None;
        if !t.trim().is_empty() {
            self.remember(&t);
        }
        t
    }

    /// True while Up/Down are walking history (the text is a recalled entry).
    pub fn browsing_history(&self) -> bool {
        self.hist_pos.is_some()
    }

    /// Append to history; an earlier identical entry moves to the end.
    pub fn remember(&mut self, entry: &str) {
        if self.history.last().map(String::as_str) == Some(entry) {
            return;
        }
        self.history.retain(|h| h != entry);
        self.history.push(entry.to_string());
        self.added = Some(entry.to_string());
        if self.history.len() > HISTORY_KEEP {
            self.history.remove(0);
        }
    }

    /// Entry recorded by the last `take`, once.
    pub fn history_added(&mut self) -> Option<String> {
        self.added.take()
    }

    /// Replace history, keeping the last occurrence of repeated entries.
    pub fn set_history(&mut self, items: Vec<String>) {
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<String> = items
            .into_iter()
            .rev()
            .filter(|e| seen.insert(e.clone()))
            .collect();
        out.reverse();
        self.history = out;
        self.hist_pos = None;
    }

    /// Read the shared history file; rewrites it when it has grown past twice
    /// the keep limit so it never needs more than one pass.
    pub fn load_history_file(path: &std::path::Path) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let all: Vec<String> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<String>(l).ok())
            .collect();
        let start = all.len().saturating_sub(HISTORY_KEEP);
        let kept = all[start..].to_vec();
        // rewrite once it doubles so reads stay one short pass
        if all.len() > 2 * HISTORY_KEEP {
            let body: String = kept
                .iter()
                .map(|e| format!("{}\n", serde_json::to_string(e).unwrap_or_default()))
                .collect();
            let _ = std::fs::write(path, body);
        }
        kept
    }

    pub fn append_history_file(path: &std::path::Path, entry: &str) {
        use std::io::Write as _;
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{}", serde_json::to_string(entry).unwrap_or_default());
        }
    }

    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let pos = match self.hist_pos {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return true,
            Some(p) => p - 1,
        };
        self.hist_pos = Some(pos);
        self.text = self.history[pos].clone();
        self.cursor = self.char_len();
        true
    }

    pub fn history_next(&mut self) -> bool {
        let Some(p) = self.hist_pos else { return false };
        if p + 1 >= self.history.len() {
            self.hist_pos = None;
            self.text = std::mem::take(&mut self.draft);
        } else {
            self.hist_pos = Some(p + 1);
            self.text = self.history[p + 1].clone();
        }
        self.cursor = self.char_len();
        true
    }

    /// Soft-wrapped rows for `width`, plus the cursor's (row, col).
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(1);
        let mut rows: Vec<String> = Vec::new();
        let mut cursor_rc = (0, 0);
        let mut cur = String::new();
        let mut cur_w = 0usize;
        let total = self.char_len();
        for (i, c) in self.text.chars().enumerate() {
            if i == self.cursor {
                cursor_rc = (rows.len(), cur_w);
            }
            if c == '\n' {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
                continue;
            }
            let w = c.width().unwrap_or(1);
            if cur_w + w > width {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
                if i == self.cursor {
                    cursor_rc = (rows.len(), 0);
                }
            }
            cur.push(c);
            cur_w += w;
        }
        if self.cursor >= total {
            if cur_w >= width {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cursor_rc = (rows.len(), cur_w);
        }
        rows.push(cur);
        (rows, cursor_rc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_and_lines() {
        let mut e = Editor::default();
        e.insert_str("hello\nworld");
        assert_eq!(e.line_col(), (1, 5));
        assert!(e.up());
        assert_eq!(e.line_col(), (0, 5));
        e.home();
        assert_eq!(e.cursor, 0);
        e.end();
        assert_eq!(e.cursor, 5);
        e.insert_char('!');
        assert_eq!(e.text, "hello!\nworld");
        e.delete_word();
        assert_eq!(e.text, "\nworld");
        e.down();
        e.end();
        e.backspace();
        assert_eq!(e.text, "\nworl");
        let t = e.take();
        assert_eq!(t, "\nworl");
        assert!(e.history_prev());
        assert_eq!(e.text, "\nworl");
        assert!(e.history_next());
        assert!(e.is_empty());
    }

    #[test]
    fn wraps_and_places_cursor() {
        let mut e = Editor::default();
        e.insert_str("abcdefgh");
        let (rows, cur) = e.layout(4);
        assert_eq!(rows, vec!["abcd", "efgh", ""]); // cursor needs a row of its own
        assert_eq!(cur, (2, 0));
        e.cursor = 5;
        let (_, cur) = e.layout(4);
        assert_eq!(cur, (1, 1));
    }
}

// ---- paste chips ---------------------------------------------------------

impl Editor {
    /// Insert pasted text. Long pastes become a chip like `[Pasted #1: 40 lines]`
    /// that expands back to the full text on submit.
    pub fn insert_paste(&mut self, s: &str, collapse_lines: usize) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let lines = s.lines().count();
        if lines <= collapse_lines.max(1) && s.chars().count() <= 400 {
            self.insert_str(&s);
            return;
        }
        self.pastes.push(s);
        let chip = format!("[Pasted #{}: {} lines]", self.pastes.len(), lines);
        self.insert_str(&chip);
    }

    /// Expand chips back into their text.
    pub fn expand_pastes(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (i, p) in self.pastes.iter().enumerate() {
            let n = p.lines().count();
            let chip = format!("[Pasted #{}: {} lines]", i + 1, n);
            out = out.replace(&chip, p);
        }
        out
    }

    /// If the cursor sits right after a chip, remove the whole chip. Returns
    /// true when something was removed.
    pub fn backspace_chip(&mut self) -> bool {
        let before: String = self.text.chars().take(self.cursor).collect();
        if !before.ends_with(" lines]") {
            return false;
        }
        let Some(start) = before.rfind("[Pasted #") else {
            return false;
        };
        let chip_chars = before[start..].chars().count();
        let start_b = self.byte_at(self.cursor - chip_chars);
        let end_b = self.byte_at(self.cursor);
        self.text.replace_range(start_b..end_b, "");
        self.cursor -= chip_chars;
        true
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    #[test]
    fn long_paste_collapses_and_expands() {
        let mut e = Editor::default();
        e.insert_str("see: ");
        e.insert_paste("a\nb\nc\nd\ne", 3);
        assert_eq!(e.text, "see: [Pasted #1: 5 lines]");
        let expanded = e.expand_pastes(&e.text);
        assert_eq!(expanded, "see: a\nb\nc\nd\ne");
        assert!(e.backspace_chip());
        assert_eq!(e.text, "see: ");
        let mut e = Editor::default();
        e.insert_paste("x\ny", 3);
        assert_eq!(e.text, "x\ny");
    }
}
