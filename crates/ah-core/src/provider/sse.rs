//! Minimal Server-Sent Events line parser. Feed it lines (without the trailing
//! newline); it yields the `data` payload of each complete event.

#[derive(Default)]
pub struct SseParser {
    data: String,
    has_data: bool,
}

pub enum SseItem<'a> {
    Data(&'a str),
    Done,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `Some(payload)` when `line` completes an event.
    pub fn push_line(&mut self, line: &str) -> Option<SseItem<'_>> {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if !self.has_data {
                return None;
            }
            self.has_data = false;
            if self.data.trim_end_matches('\n') == "[DONE]" {
                self.data.clear();
                return Some(SseItem::Done);
            }
            // buffer is cleared on the next `data:` line, not here
            return Some(SseItem::Data(self.data.trim_end_matches('\n')));
        }
        if line.starts_with(':') {
            return None; // comment, e.g. ": OPENROUTER PROCESSING"
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        if field == "data" {
            if !self.has_data {
                self.data.clear();
                self.has_data = true;
            } else {
                self.data.push('\n');
            }
            self.data.push_str(value);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(input: &str) -> (Vec<String>, bool) {
        let mut p = SseParser::new();
        let mut out = Vec::new();
        let mut done = false;
        for line in input.split('\n') {
            match p.push_line(line) {
                Some(SseItem::Data(d)) => out.push(d.to_string()),
                Some(SseItem::Done) => done = true,
                None => {}
            }
        }
        (out, done)
    }

    #[test]
    fn parses_events_comments_and_done() {
        let (d, done) = collect(
            ": OPENROUTER PROCESSING\n\ndata: {\"a\":1}\n\n: ping\ndata: {\"b\":2}\r\n\r\ndata: [DONE]\n\n",
        );
        assert_eq!(d, vec!["{\"a\":1}", "{\"b\":2}"]);
        assert!(done);
    }

    #[test]
    fn multiline_data_joined() {
        let (d, _) = collect("data: x\ndata: y\n\n");
        assert_eq!(d, vec!["x\ny"]);
    }

    #[test]
    fn ignores_other_fields() {
        let (d, _) = collect("event: msg\nid: 3\ndata: z\n\n");
        assert_eq!(d, vec!["z"]);
    }
}
