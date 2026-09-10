//! Small multi-line editor for the prompt box. Cursor is a char index into
//! `text`; rendering soft-wraps to the box width.

use unicode_width::UnicodeWidthChar;

/// Wrapped input rows, each a list of runs: the text, and whether it is
/// dictation that has not been committed yet.
pub type Rows = Vec<Vec<(String, bool)>>;

#[derive(Default, Debug)]
pub struct Editor {
    pub text: String,
    /// Char offset.
    pub cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    draft: String,
    pastes: Vec<String>,
    /// Attached images as `data:` URLs, shown as `[Image #1: 120 KB]` chips.
    images: Vec<String>,
    /// Entry added by the last `take`, not yet written to disk.
    added: Option<String>,
    /// What the last kill removed, for `yank`.
    killed: String,
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

    /// The word before the cursor, or a whole paste or image chip.
    pub fn delete_word(&mut self) {
        if self.backspace_chip() {
            return;
        }
        let chars: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.kill(i, self.cursor);
    }

    /// Everything typed so far, kept for `yank`.
    pub fn delete_all(&mut self) {
        self.kill(0, self.char_len());
    }

    /// Put back what the last kill took, at the cursor.
    pub fn yank(&mut self) {
        if self.killed.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.killed);
        self.insert_str(&text);
        self.killed = text;
    }

    /// Cut `from..to` out of the text and remember it.
    fn kill(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let (from_b, to_b) = (self.byte_at(from), self.byte_at(to));
        self.killed = self.text[from_b..to_b].to_string();
        self.text.replace_range(from_b..to_b, "");
        self.cursor = from;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.hist_pos = None;
        self.pastes.clear();
        self.images.clear();
    }

    /// Take the text for submission and record it in history. Image chips
    /// are stripped; the images come out of `take_images`.
    pub fn take(&mut self) -> String {
        let raw = std::mem::take(&mut self.text);
        let t = self.strip_image_chips(&self.expand_pastes(&raw));
        self.pastes.clear();
        self.cursor = 0;
        self.hist_pos = None;
        if !t.trim().is_empty() {
            self.remember(&t);
        }
        t
    }

    /// Images attached since the last take.
    pub fn take_images(&mut self) -> Vec<String> {
        std::mem::take(&mut self.images)
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
    /// The same rows, but with `pending` shown at the cursor and each run
    /// marked: false is text that is really there, true is dictation that has
    /// not been committed yet. The cursor sits at the end of the pending run,
    /// so it travels with the words as they arrive rather than sitting still
    /// while they pile up beside it.
    pub fn layout_pending(&self, width: usize, pending: &str) -> (Rows, (usize, usize)) {
        let width = width.max(1);
        let mut rows: Rows = Vec::new();
        let mut cursor_rc = (0, 0);
        let mut cur: Vec<(String, bool)> = Vec::new();
        let mut cur_w = 0usize;
        let head = self.text.chars().take(self.cursor);
        let tail = self.text.chars().skip(self.cursor);
        let chars = head
            .map(|c| (c, false))
            .chain(pending.chars().map(|c| (c, true)))
            .chain(tail.map(|c| (c, false)));
        // Counted over the combined stream, so the caret can be put after the
        // pending run rather than in front of it.
        let target = self.cursor + pending.chars().count();
        let mut i = 0usize;
        let mut placed = false;
        for (c, ghost) in chars {
            if c == '\n' {
                if !placed && i == target {
                    cursor_rc = (rows.len(), cur_w);
                    placed = true;
                }
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
                i += 1;
                continue;
            }
            let w = c.width().unwrap_or(1);
            if cur_w + w > width {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            if !placed && i == target {
                cursor_rc = (rows.len(), cur_w);
                placed = true;
            }
            match cur.last_mut() {
                Some((t, g)) if *g == ghost => t.push(c),
                _ => cur.push((c.to_string(), ghost)),
            }
            cur_w += w;
            i += 1;
        }
        if !placed {
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
    fn a_kill_can_be_put_back() {
        let mut e = Editor::default();
        e.insert_str("write the parser");
        e.delete_word();
        assert_eq!(e.text, "write the ");
        e.yank();
        assert_eq!(e.text, "write the parser");
        assert_eq!(e.cursor, 16);
        // Ctrl-U takes the lot, wherever the cursor is, and Ctrl-Y brings it
        // back; yanking twice pastes it twice.
        e.cursor = 5;
        e.delete_all();
        assert!(e.is_empty());
        assert_eq!(e.cursor, 0);
        e.yank();
        e.yank();
        assert_eq!(e.text, "write the parserwrite the parser");
        // Nothing killed, nothing to put back.
        let mut e = Editor::default();
        e.yank();
        assert!(e.is_empty());
    }

    #[test]
    fn a_word_delete_takes_a_whole_chip() {
        let mut e = Editor::default();
        e.insert_paste("a\nb\nc\nd\n", 2);
        assert!(e.text.starts_with("[Pasted #1:"), "{}", e.text);
        e.delete_word();
        assert!(e.is_empty(), "{}", e.text);
    }

    fn text_rows(e: &Editor, width: usize, pending: &str) -> Vec<String> {
        e.layout_pending(width, pending)
            .0
            .into_iter()
            .map(|r| r.into_iter().map(|(t, _)| t).collect::<String>())
            .collect()
    }

    #[test]
    fn wraps_and_places_cursor() {
        let mut e = Editor::default();
        e.insert_str("abcdefgh");
        let (rows, cur) = e.layout_pending(4, "");
        assert_eq!(text_rows(&e, 4, ""), vec!["abcd", "efgh", ""]); // cursor needs a row of its own
        assert_eq!(rows.len(), 3);
        assert_eq!(cur, (2, 0));
        e.cursor = 5;
        let (_, cur) = e.layout_pending(4, "");
        assert_eq!(cur, (1, 1));
    }

    #[test]
    fn dictation_needs_a_space_only_when_it_would_run_into_something() {
        let mut e = Editor::default();
        assert!(!e.needs_space_at_cursor(), "nothing to run into");
        e.insert_str("fix");
        assert!(e.needs_space_at_cursor(), "would read as `fixthe auth`");
        e.insert_char(' ');
        assert!(!e.needs_space_at_cursor(), "there is already a space");
        e.insert_str("the auth\n");
        assert!(!e.needs_space_at_cursor(), "a new line is a gap of its own");
        // Mid-text: what matters is the character before the cursor, not the
        // end of the line.
        e.insert_str("and then");
        e.cursor = 4;
        assert!(!e.needs_space_at_cursor(), "the cursor sits after a space");
        e.cursor = 3;
        assert!(e.needs_space_at_cursor());
    }

    #[test]
    fn dictation_shows_at_the_cursor_and_is_marked_apart() {
        let mut e = Editor::default();
        e.insert_str("fix ");
        let (rows, cur) = e.layout_pending(40, "the auth bug");
        assert_eq!(text_rows(&e, 40, "the auth bug"), vec!["fix the auth bug"]);
        // The caret follows the words, at the end of the grey rather than in
        // front of it.
        assert_eq!(cur, (0, 16));
        assert_eq!(rows[0][0], ("fix ".to_string(), false));
        assert_eq!(rows[0][1], ("the auth bug".to_string(), true));
    }

    #[test]
    fn the_caret_travels_as_dictation_arrives() {
        let mut e = Editor::default();
        e.insert_str("fix ");
        let at = |p: &str| e.layout_pending(40, p).1;
        assert_eq!(at(""), (0, 4));
        assert_eq!(at("the"), (0, 7));
        assert_eq!(at("the auth"), (0, 12));
        assert_eq!(at("the auth bug"), (0, 16));
    }

    #[test]
    fn the_caret_follows_dictation_onto_the_next_row() {
        let e = Editor::default();
        // Eight characters at a width of four: the caret ends up on the row
        // after the last full one.
        assert_eq!(e.layout_pending(4, "abcdefgh").1, (2, 0));
        assert_eq!(e.layout_pending(4, "abcde").1, (1, 1));
    }

    #[test]
    fn dictation_lands_where_the_cursor_is_not_at_the_end() {
        let mut e = Editor::default();
        e.insert_str("fix now");
        e.cursor = 4;
        assert_eq!(text_rows(&e, 40, "the bug "), vec!["fix the bug now"]);
    }

    #[test]
    fn dictation_wraps_with_the_rest() {
        let e = {
            let mut e = Editor::default();
            e.insert_str("ab");
            e
        };
        // The trailing empty row is where the caret sits, exactly as it does
        // for typed text that fills the last row.
        assert_eq!(text_rows(&e, 4, "cdefgh"), vec!["abcd", "efgh", ""]);
    }
}

// ---- paste chips ---------------------------------------------------------

impl Editor {
    /// Would text inserted at the cursor run straight into the character in
    /// front of it? Dictation lands at the cursor, and words are not typed
    /// with their own leading space the way a person types one.
    pub fn needs_space_at_cursor(&self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.text
            .chars()
            .nth(self.cursor - 1)
            .is_some_and(|c| !c.is_whitespace())
    }

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

    /// Attach an image; the chip stays in the text until submit.
    pub fn insert_image(&mut self, data_url: String, bytes: usize) {
        self.images.push(data_url);
        let size = if bytes >= 1024 * 1024 {
            format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{} KB", bytes.div_ceil(1024))
        };
        let chip = format!("[Image #{}: {size}]", self.images.len());
        if !self.text.is_empty() && !self.text.ends_with([' ', '\n']) {
            self.insert_char(' ');
        }
        self.insert_str(&chip);
    }

    fn strip_image_chips(&self, text: &str) -> String {
        if !text.contains("[Image #") {
            return text.to_string();
        }
        let mut out = text.to_string();
        while let Some(start) = out.find("[Image #")
            && let Some(len) = out[start..].find(']')
        {
            out.replace_range(start..start + len + 1, "");
        }
        out.trim().to_string()
    }

    /// If the cursor sits right after a chip, remove the whole chip. Returns
    /// true when something was removed. Removing an image chip drops the
    /// image as well.
    pub fn backspace_chip(&mut self) -> bool {
        let before: String = self.text.chars().take(self.cursor).collect();
        if !before.ends_with(']') {
            return false;
        }
        let paste = before.rfind("[Pasted #");
        let image = before.rfind("[Image #");
        let Some(start) = paste.max(image) else {
            return false;
        };
        if before[start..].contains('\n') {
            return false;
        }
        if image == Some(start)
            && let Some(n) = before[start + 8..]
                .split(':')
                .next()
                .and_then(|n| n.trim().parse::<usize>().ok())
            && n >= 1
            && n <= self.images.len()
        {
            self.images.remove(n - 1);
            // renumber the chips that follow
            for i in n..=self.images.len() {
                self.text =
                    self.text
                        .replacen(&format!("[Image #{}:", i + 1), &format!("[Image #{i}:"), 1);
            }
        }
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
    fn image_chips_attach_and_detach() {
        let mut e = Editor::default();
        e.insert_str("see");
        e.insert_image("data:image/png;base64,AA==".into(), 2048);
        e.insert_image("data:image/png;base64,BB==".into(), 3 * 1024 * 1024);
        assert_eq!(e.text, "see [Image #1: 2 KB] [Image #2: 3.0 MB]");
        assert!(e.backspace_chip());
        assert_eq!(e.text, "see [Image #1: 2 KB] ");
        assert_eq!(e.images.len(), 1);
        let t = e.take();
        assert_eq!(t, "see");
        assert_eq!(
            e.take_images(),
            vec!["data:image/png;base64,AA==".to_string()]
        );
        assert!(e.take_images().is_empty());
    }

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
