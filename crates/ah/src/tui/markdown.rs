//! Markdown to ratatui lines over pulldown-cmark events. Width-aware, styled
//! from the palette, with code block highlighting.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::highlight;
use super::theme::Palette;

#[derive(Clone, Copy)]
pub struct Opts {
    pub width: usize,
    pub highlight: bool,
}

type Piece = (String, Style);

struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Piece>>>,
    cur_row: Vec<Vec<Piece>>,
    cur_cell: Vec<Piece>,
    in_head: bool,
    head_rows: usize,
}

struct R<'p> {
    pal: &'p Palette,
    opts: Opts,
    out: Vec<Line<'static>>,
    inline: Vec<Piece>,
    styles: Vec<Style>,
    /// (first-line prefix, continuation prefix) per open container.
    prefixes: Vec<(String, String)>,
    /// Next ordinal per open list; `None` for bullet lists.
    lists: Vec<Option<u64>>,
    code: Option<(String, String)>,
    table: Option<Table>,
    link: Option<String>,
    heading: Option<HeadingLevel>,
    item_fresh: bool,
}

pub fn render(text: &str, pal: &Palette, opts: Opts) -> Vec<Line<'static>> {
    let mut r = R {
        pal,
        opts,
        out: Vec::new(),
        inline: Vec::new(),
        styles: vec![Style::default().fg(pal.assistant)],
        prefixes: Vec::new(),
        lists: Vec::new(),
        code: None,
        table: None,
        link: None,
        heading: None,
        item_fresh: false,
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(text, options) {
        r.event(ev);
    }
    r.flush_inline();
    while r.out.last().is_some_and(|l| l.width() == 0) {
        r.out.pop();
    }
    r.out
}

impl R<'_> {
    fn style(&self) -> Style {
        *self.styles.last().expect("style stack")
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let s = f(self.style());
        self.styles.push(s);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn prefix(&self, first: bool) -> String {
        let mut p = String::new();
        for (i, (f, rest)) in self.prefixes.iter().enumerate() {
            let last = i + 1 == self.prefixes.len();
            p.push_str(if first && last { f } else { rest });
        }
        p
    }

    fn blank(&mut self) {
        if self.out.last().is_some_and(|l| l.width() > 0) {
            let trimmed = self.prefix(false).trim_end().to_string();
            let dim = self.pal.dim();
            self.out.push(if trimmed.is_empty() {
                Line::default()
            } else {
                Line::from(Span::styled(trimmed, dim))
            });
        }
    }

    fn text(&mut self, s: &str) {
        let st = self.style();
        if let Some(t) = self.table.as_mut() {
            t.cur_cell.push((s.to_string(), st));
            return;
        }
        self.inline.push((s.to_string(), st));
    }

    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let pieces = std::mem::take(&mut self.inline);
        let first = self.prefix(self.item_fresh);
        self.item_fresh = false;
        let rest = self.prefix(false);
        let width = self.opts.width;
        let quote_style = self.pal.dim();
        let lines = wrap_styled(
            &pieces,
            width.saturating_sub(first.width().max(rest.width())).max(8),
        );
        for (i, spans) in lines.into_iter().enumerate() {
            let p = if i == 0 { first.clone() } else { rest.clone() };
            let mut v = Vec::with_capacity(spans.len() + 1);
            if !p.is_empty() {
                v.push(Span::styled(p, quote_style));
            }
            v.extend(spans);
            self.out.push(Line::from(v));
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some((_, buf)) = self.code.as_mut() {
                    buf.push_str(&t);
                } else {
                    self.text(&t);
                }
            }
            Event::Code(c) => {
                let (code, code_bg) = (self.pal.code, self.pal.code_bg);
                let st = self.style().fg(code).bg(code_bg);
                if let Some(t) = self.table.as_mut() {
                    t.cur_cell.push((c.to_string(), st));
                } else {
                    self.inline.push((c.to_string(), st));
                }
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.text("\n"),
            Event::Rule => {
                self.flush_inline();
                self.blank();
                let w = self
                    .opts
                    .width
                    .saturating_sub(self.prefix(false).width())
                    .max(1);
                let line = "─".repeat(w);
                self.out.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(self.pal.rule),
                )));
                self.blank();
            }
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
            Event::Html(h) | Event::InlineHtml(h) => self.text(&h),
            Event::FootnoteReference(f) => self.text(&format!("[^{f}]")),
            Event::InlineMath(m) | Event::DisplayMath(m) => self.text(&m),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if !self.item_fresh {
                    self.blank();
                }
            }
            Tag::Heading { level, .. } => {
                self.blank();
                self.heading = Some(level);
                let base = self.pal.heading;
                let accent = self.pal.accent;
                self.push_style(move |s| {
                    let s = s.add_modifier(Modifier::BOLD);
                    match level {
                        HeadingLevel::H1 | HeadingLevel::H2 => s.fg(accent),
                        _ => s.fg(base),
                    }
                });
                let marks = match level {
                    HeadingLevel::H1 => "# ",
                    HeadingLevel::H2 => "## ",
                    HeadingLevel::H3 => "### ",
                    _ => "",
                };
                if !marks.is_empty() {
                    let st = self.style();
                    self.inline.push((marks.into(), st));
                }
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.blank();
                self.prefixes.push(("│ ".into(), "│ ".into()));
                let q = self.pal.quote;
                self.push_style(move |s| s.fg(q).add_modifier(Modifier::ITALIC));
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                self.blank();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush_inline();
                if self.lists.is_empty() {
                    self.blank();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush_inline();
                let depth = self.lists.len();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => {
                        if depth % 2 == 1 {
                            "• ".to_string()
                        } else {
                            "◦ ".to_string()
                        }
                    }
                };
                let cont = " ".repeat(marker.width());
                self.prefixes.push((marker, cont));
                self.item_fresh = true;
            }
            Tag::Emphasis => self.push_style(|s| s.add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_style(|s| s.add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.link = Some(dest_url.to_string());
                let l = self.pal.link;
                self.push_style(move |s| s.fg(l).add_modifier(Modifier::UNDERLINED));
            }
            Tag::Table(aligns) => {
                self.flush_inline();
                self.blank();
                self.table = Some(Table {
                    aligns,
                    rows: Vec::new(),
                    cur_row: Vec::new(),
                    cur_cell: Vec::new(),
                    in_head: false,
                    head_rows: 0,
                });
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.in_head = true;
                }
            }
            Tag::TableRow | Tag::TableCell => {}
            Tag::HtmlBlock | Tag::MetadataBlock(_) | Tag::FootnoteDefinition(_) => {}
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_inline(),
            TagEnd::Heading(_) => {
                self.flush_inline();
                self.pop_style();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.prefixes.pop();
                self.pop_style();
            }
            TagEnd::CodeBlock => {
                if let Some((lang, code)) = self.code.take() {
                    self.code_block(&lang, &code);
                }
            }
            TagEnd::List(_) => {
                self.flush_inline();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                if self.item_fresh {
                    // Empty item: still print the marker.
                    let p = self.prefix(true);
                    self.out.push(Line::from(Span::styled(p, self.pal.dim())));
                    self.item_fresh = false;
                }
                self.prefixes.pop();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link | TagEnd::Image => {
                self.pop_style();
                if let Some(url) = self.link.take() {
                    let shown = self.inline.last().map(|(s, _)| s.as_str()).unwrap_or("");
                    if !shown
                        .trim_end_matches('/')
                        .ends_with(url.trim_end_matches('/'))
                        && !url.is_empty()
                    {
                        let d = self.pal.dim();
                        self.inline.push((format!(" ({url})"), d));
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    let cell = std::mem::take(&mut t.cur_cell);
                    t.cur_row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.cur_row);
                    t.rows.push(row);
                    if t.in_head {
                        t.head_rows = t.rows.len();
                        t.in_head = false;
                    }
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.table_block(t);
                }
                self.blank();
            }
            _ => {}
        }
    }

    fn code_block(&mut self, lang: &str, code: &str) {
        let width = self
            .opts
            .width
            .saturating_sub(self.prefix(false).width())
            .max(8);
        let base = self.style().fg(self.pal.code).bg(self.pal.code_bg);
        let padded = self.pal.code_bg != Color::Reset;
        let hl = if self.opts.highlight {
            highlight::lang_for(lang)
        } else {
            None
        };
        let mut state = highlight::State::default();
        let cont = self.prefix(false);
        let inner = if padded {
            width.saturating_sub(1)
        } else {
            width
        };
        for raw in code.trim_end_matches('\n').split('\n') {
            let line = raw.replace('\t', "    ");
            let pieces: Vec<Piece> = match &hl {
                Some(l) => highlight::highlight_line(l, &line, self.pal, base, &mut state),
                None => vec![(line.clone(), base)],
            };
            for chunk in chunk_styled(&pieces, inner) {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(chunk.len() + 2);
                if !cont.is_empty() {
                    spans.push(Span::styled(cont.clone(), self.pal.dim()));
                }
                let mut w = 0;
                if padded {
                    spans.push(Span::styled(" ", base));
                    w = 1;
                }
                for (s, st) in chunk {
                    w += s.width();
                    spans.push(Span::styled(s, st));
                }
                if padded && w < width {
                    spans.push(Span::styled(" ".repeat(width - w), base));
                }
                self.out.push(Line::from(spans));
            }
        }
        self.blank();
    }

    fn table_block(&mut self, t: Table) {
        let ncols = t.rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        let cell_w = |c: &Vec<Piece>| c.iter().map(|(s, _)| s.width()).sum::<usize>();
        let mut widths = vec![0usize; ncols];
        for r in &t.rows {
            for (i, c) in r.iter().enumerate() {
                widths[i] = widths[i].max(cell_w(c));
            }
        }
        // Shrink the widest columns until the table fits.
        let prefix = self.prefix(false);
        let avail = self.opts.width.saturating_sub(prefix.width()).max(8);
        let sep = 3;
        while widths.iter().sum::<usize>() + sep * (ncols - 1) > avail {
            let (i, _) = widths.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
            if widths[i] <= 4 {
                break;
            }
            widths[i] -= 1;
        }
        let dim = self.pal.dim();
        for (ri, row) in t.rows.iter().enumerate() {
            let head = ri < t.head_rows;
            let mut spans: Vec<Span<'static>> = Vec::new();
            if !prefix.is_empty() {
                spans.push(Span::styled(prefix.clone(), dim));
            }
            for (ci, &col_w) in widths.iter().enumerate() {
                let cell = row.get(ci).cloned().unwrap_or_default();
                let mut used = 0;
                let mut cell_spans: Vec<Span<'static>> = Vec::new();
                'outer: for (s, st) in cell {
                    let mut acc = String::new();
                    for ch in s.chars() {
                        let cw = ch.width().unwrap_or(0);
                        if used + cw > col_w {
                            if !acc.is_empty() {
                                cell_spans.push(Span::styled(
                                    std::mem::take(&mut acc),
                                    if head {
                                        st.add_modifier(Modifier::BOLD)
                                    } else {
                                        st
                                    },
                                ));
                            }
                            break 'outer;
                        }
                        acc.push(ch);
                        used += cw;
                    }
                    if !acc.is_empty() {
                        cell_spans.push(Span::styled(
                            acc,
                            if head {
                                st.add_modifier(Modifier::BOLD)
                            } else {
                                st
                            },
                        ));
                    }
                }
                let pad = col_w.saturating_sub(used);
                let align = t.aligns.get(ci).copied().unwrap_or(Alignment::None);
                let (l, r) = match align {
                    Alignment::Right => (pad, 0),
                    Alignment::Center => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                if l > 0 {
                    spans.push(Span::raw(" ".repeat(l)));
                }
                spans.extend(cell_spans);
                if r > 0 {
                    spans.push(Span::raw(" ".repeat(r)));
                }
                if ci + 1 < ncols {
                    spans.push(Span::styled(" │ ", dim));
                }
            }
            self.out.push(Line::from(spans));
            if head && ri + 1 == t.head_rows {
                let rule: String = widths
                    .iter()
                    .map(|w| "─".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("─┼─");
                let mut s = Vec::new();
                if !prefix.is_empty() {
                    s.push(Span::styled(prefix.clone(), dim));
                }
                s.push(Span::styled(rule, dim));
                self.out.push(Line::from(s));
            }
        }
    }
}

/// Greedy word wrap over styled pieces. `\n` inside a piece forces a break.
pub fn wrap_styled(pieces: &[Piece], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut line: Vec<Piece> = Vec::new();
    let mut line_w = 0usize;
    let mut word: Vec<Piece> = Vec::new();
    let mut word_w = 0usize;

    fn push_piece(v: &mut Vec<Piece>, s: &str, st: Style) {
        if s.is_empty() {
            return;
        }
        if let Some((last, ls)) = v.last_mut()
            && *ls == st
        {
            last.push_str(s);
        } else {
            v.push((s.to_string(), st));
        }
    }
    fn finish(lines: &mut Vec<Vec<Span<'static>>>, line: &mut Vec<Piece>) {
        let spans = line.drain(..).map(|(s, st)| Span::styled(s, st)).collect();
        lines.push(spans);
    }

    let place_word = |lines: &mut Vec<Vec<Span<'static>>>,
                      line: &mut Vec<Piece>,
                      line_w: &mut usize,
                      word: &mut Vec<Piece>,
                      word_w: &mut usize| {
        if word.is_empty() {
            return;
        }
        let space = usize::from(*line_w > 0);
        if *line_w + space + *word_w > width && *line_w > 0 {
            finish(lines, line);
            *line_w = 0;
        }
        if *word_w > width {
            // Break an over-long token by characters.
            for (s, st) in word.drain(..) {
                for ch in s.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if *line_w + cw > width && *line_w > 0 {
                        finish(lines, line);
                        *line_w = 0;
                    }
                    let mut buf = [0u8; 4];
                    push_piece(line, ch.encode_utf8(&mut buf), st);
                    *line_w += cw;
                }
            }
            *word_w = 0;
            return;
        }
        if *line_w > 0 {
            let st = line.last().map(|(_, s)| *s).unwrap_or_default();
            push_piece(line, " ", st);
            *line_w += 1;
        }
        for (s, st) in word.drain(..) {
            push_piece(line, &s, st);
        }
        *line_w += *word_w;
        *word_w = 0;
    };

    for (text, st) in pieces {
        for ch in text.chars() {
            match ch {
                '\n' => {
                    place_word(&mut lines, &mut line, &mut line_w, &mut word, &mut word_w);
                    finish(&mut lines, &mut line);
                    line_w = 0;
                }
                ' ' => place_word(&mut lines, &mut line, &mut line_w, &mut word, &mut word_w),
                c => {
                    let mut buf = [0u8; 4];
                    push_piece(&mut word, c.encode_utf8(&mut buf), *st);
                    word_w += c.width().unwrap_or(0);
                }
            }
        }
    }
    place_word(&mut lines, &mut line, &mut line_w, &mut word, &mut word_w);
    if !line.is_empty() || lines.is_empty() {
        finish(&mut lines, &mut line);
    }
    lines
}

/// Hard-cut styled pieces into rows of at most `width` columns.
fn chunk_styled(pieces: &[Piece], width: usize) -> Vec<Vec<Piece>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Piece>> = vec![Vec::new()];
    let mut w = 0;
    for (s, st) in pieces {
        for ch in s.chars() {
            let cw = ch.width().unwrap_or(0);
            if w + cw > width {
                rows.push(Vec::new());
                w = 0;
            }
            let row = rows.last_mut().unwrap();
            let mut buf = [0u8; 4];
            let c = ch.encode_utf8(&mut buf);
            if let Some((last, ls)) = row.last_mut()
                && ls == st
            {
                last.push_str(c);
            } else {
                row.push((c.to_string(), *st));
            }
            w += cw;
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use ah_core::abi::Theme;

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn render_w(md: &str, width: usize) -> Vec<String> {
        let pal = Palette::from_theme(&Theme::default());
        plain(&render(
            md,
            &pal,
            Opts {
                width,
                highlight: true,
            },
        ))
    }

    #[test]
    fn headings_lists_and_wrapping() {
        let out = render_w(
            "# Title\n\nSome **bold** and `code` text that wraps around.\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n",
            20,
        );
        assert_eq!(out[0], "# Title");
        assert_eq!(out[1], "");
        assert!(out[2].starts_with("Some bold and code"), "{out:?}");
        assert!(out.iter().any(|l| l == "• one"), "{out:?}");
        assert!(out.iter().any(|l| l == "  ◦ nested"), "{out:?}");
        assert!(out.iter().any(|l| l == "1. first"), "{out:?}");
        assert!(out.iter().all(|l| l.width() <= 20), "{out:?}");
    }

    #[test]
    fn code_block_is_highlighted_without_label_or_padding() {
        let pal = Palette::from_theme(&Theme::default());
        let opts = Opts {
            width: 30,
            highlight: true,
        };
        let lines = render("```rust\nlet x = 1;\n```\n", &pal, opts);
        let code = &lines[0];
        assert_eq!(code.spans[0].content, "let");
        assert_eq!(code.spans[0].style.fg, pal.syn_keyword.fg);
        assert_eq!(code.width(), "let x = 1;".len());
        let boxed = Palette::from_theme(&Theme {
            code_bg: "235".into(),
            ..Theme::default()
        });
        let lines = render("```rust\nlet x = 1;\n```\n", &boxed, opts);
        let code = &lines[0];
        assert_eq!(code.width(), 30);
        assert!(code.spans.iter().all(|s| s.style.bg == Some(boxed.code_bg)));
    }

    #[test]
    fn unclosed_fence_streams_as_code() {
        let out = render_w("text\n\n```py\nprint(1)\n", 40);
        assert!(out.iter().any(|l| l.contains("print(1)")), "{out:?}");
    }

    #[test]
    fn quote_table_link() {
        let out = render_w(
            "> quoted\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n[docs](https://x.y)\n",
            40,
        );
        assert!(out.iter().any(|l| l.starts_with("│ quoted")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("a │ b")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("─┼─")), "{out:?}");
        assert!(
            out.iter().any(|l| l.contains("docs (https://x.y)")),
            "{out:?}"
        );
    }

    #[test]
    fn wrap_styled_breaks_long_tokens() {
        let st = Style::default();
        let lines = wrap_styled(&[("abcdefghijkl mn".into(), st)], 5);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts, vec!["abcde", "fghij", "kl mn"]);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use ah_core::abi::Theme;

    #[test]
    #[ignore]
    fn render_throughput() {
        let pal = Palette::from_theme(&Theme::default());
        let doc = "## Section\n\nParagraph with **bold** and `code` that is long enough to wrap a few times across the width of the terminal.\n\n- a\n- b\n\n```rust\nfn f(x: u32) -> u32 { x + 1 } // c\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n".repeat(20);
        let n = 200;
        let t = std::time::Instant::now();
        let mut lines = 0;
        for _ in 0..n {
            lines += render(
                &doc,
                &pal,
                Opts {
                    width: 100,
                    highlight: true,
                },
            )
            .len();
        }
        let dt = t.elapsed();
        eprintln!(
            "doc {} bytes: {:.1} us/render, {} lines, {:.1} MB/s",
            doc.len(),
            dt.as_micros() as f64 / n as f64,
            lines / n,
            (doc.len() * n) as f64 / dt.as_secs_f64() / 1e6
        );
    }
}
