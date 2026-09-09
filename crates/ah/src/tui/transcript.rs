//! Transcript blocks with a per-block wrapped-line cache.

use ah_core::abi::{ToolCall, ToolResult};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::image::{ImageData, ImageLayout};
use super::theme::Palette;

pub enum Block {
    User(String),
    Assistant {
        text: String,
        reasoning: String,
        streaming: bool,
        /// Time spent streaming reasoning; 0 when unknown (replayed history).
        think_ms: u64,
    },
    Tool {
        call: ToolCall,
        result: Option<ToolResult>,
        /// How long the call took, or `None` for a call replayed from a
        /// session file, where the timing was never recorded.
        duration_ms: Option<u64>,
        expanded: Option<bool>,
    },
    /// A compaction: the model's summary, folded away behind a one-line header
    /// so a long conversation does not turn into a wall of text.
    Summary {
        text: String,
        /// Tokens before and after, or `None` for a summary replayed from a
        /// session file, where the counts were never recorded.
        tokens: Option<(u64, u64)>,
        /// Per-block override of `show_tool_output`.
        expanded: Option<bool>,
    },
    Notice(String),
    Error(String),
    /// A picture the model drew. Boxed because the payload dwarfs every other
    /// variant and they would all grow to match.
    Image(Box<ImageData>),
}

pub struct Entry {
    pub block: Block,
    cache_width: u16,
    cache_key: u64,
    lines: Vec<Line<'static>>,
}

/// Render-affecting options snapshot, so caches invalidate when they change.
#[derive(Clone, Copy, PartialEq)]
pub struct View {
    pub show_tool_output: bool,
    pub tool_output_lines: u16,
    pub show_reasoning: bool,
    pub wrap: bool,
    pub markdown: bool,
    pub code_highlight: bool,
    pub image: ImageView,
}

/// Everything a picture's layout depends on, so that a resize or a change of
/// terminal invalidates the wrapped-line cache like any other option.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ImageView {
    /// False when the terminal draws no pictures: only the chip is laid out.
    pub inline: bool,
    /// Terminal cell size in pixels, `(0, 0)` when it would not say.
    pub cell_px: (u16, u16),
    pub max_cols: u16,
    pub max_rows: u16,
}

impl Default for ImageView {
    fn default() -> Self {
        Self {
            inline: false,
            cell_px: (0, 0),
            max_cols: 0,
            max_rows: 20,
        }
    }
}

/// Where the picture sits inside an image block's lines, or `None` when it is
/// shown as a one-line chip: no protocol, or no pixel size to lay it out with.
pub fn image_layout(block: &Block, width: u16, view: &View) -> Option<ImageLayout> {
    let Block::Image(d) = block else {
        return None;
    };
    if !view.image.inline {
        return None;
    }
    let cell = if view.image.cell_px.0 > 0 && view.image.cell_px.1 > 0 {
        view.image.cell_px
    } else {
        super::image::CELL_FALLBACK
    };
    let room = width.saturating_sub(2).max(1);
    let max_cols = if view.image.max_cols > 0 {
        room.min(view.image.max_cols)
    } else {
        room
    };
    let (cols, rows) = super::image::cells(d.px, cell, max_cols, view.image.max_rows.max(1))?;
    Some(ImageLayout {
        id: d.id,
        first: 0,
        cols,
        rows,
        px: d.px,
    })
}

impl Entry {
    pub fn new(block: Block) -> Self {
        Self {
            block,
            cache_width: 0,
            cache_key: 0,
            lines: Vec::new(),
        }
    }

    fn key(&self, view: &View, pal_gen: u64) -> u64 {
        // content fingerprint for cache invalidation
        let mut k: u64 = pal_gen.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mix = |k: &mut u64, v: u64| *k = (*k ^ v).wrapping_mul(0x100_0000_01B3);
        mix(
            &mut k,
            view.show_tool_output as u64
                | (view.show_reasoning as u64) << 1
                | (view.wrap as u64) << 2
                | (view.markdown as u64) << 3
                | (view.code_highlight as u64) << 4
                | (view.image.inline as u64) << 5,
        );
        mix(&mut k, view.tool_output_lines as u64);
        mix(
            &mut k,
            view.image.cell_px.0 as u64
                | (view.image.cell_px.1 as u64) << 16
                | (view.image.max_cols as u64) << 32
                | (view.image.max_rows as u64) << 48,
        );
        match &self.block {
            Block::User(s) | Block::Notice(s) | Block::Error(s) => mix(&mut k, s.len() as u64),
            Block::Summary {
                text,
                tokens,
                expanded,
            } => {
                mix(&mut k, text.len() as u64);
                mix(&mut k, tokens.map_or(0, |(b, a)| b ^ (a << 1)));
                mix(&mut k, expanded.map(|e| e as u64 + 1).unwrap_or(0));
            }
            Block::Assistant {
                text,
                reasoning,
                streaming,
                think_ms,
            } => {
                mix(&mut k, text.len() as u64);
                mix(&mut k, reasoning.len() as u64);
                mix(&mut k, *streaming as u64);
                mix(&mut k, *think_ms);
            }
            // The id is handed out once per picture and the file behind it
            // never changes, so it is a complete fingerprint on its own; the
            // size only guards against an id reused after `/clear`.
            Block::Image(d) => {
                mix(&mut k, d.id as u64);
                mix(&mut k, d.bytes as u64);
            }
            Block::Tool {
                call,
                result,
                duration_ms,
                expanded,
            } => {
                mix(&mut k, call.function.arguments.len() as u64);
                mix(
                    &mut k,
                    result
                        .as_ref()
                        .map(|r| r.output.len() as u64 + 1)
                        .unwrap_or(0),
                );
                mix(&mut k, duration_ms.map_or(0, |d| d + 1));
                mix(&mut k, expanded.map(|e| e as u64 + 1).unwrap_or(0));
            }
        }
        k
    }

    pub fn lines(
        &mut self,
        width: u16,
        view: &View,
        pal: &Palette,
        pal_gen: u64,
    ) -> &[Line<'static>] {
        let key = self.key(view, pal_gen);
        if self.cache_width != width || self.cache_key != key {
            self.lines = render(&self.block, width as usize, view, pal);
            self.cache_width = width;
            self.cache_key = key;
        }
        &self.lines
    }

    /// What the last [`Entry::lines`] call produced, without re-wrapping.
    pub fn cached(&self) -> &[Line<'static>] {
        &self.lines
    }
}

/// Calls quicker than this are not worth a number in the header.
const SLOW_MS: u64 = 1000;

/// `1.4s`, `2m 05s`.
fn took(ms: u64) -> String {
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let secs = ms / 1000;
    format!("{}m {:02}s", secs / 60, secs % 60)
}

/// How a line of the plan tool's answer should look, or `None` when the line
/// is not a task at all.
fn task_style(line: &str, pal: &Palette) -> Option<Style> {
    if !line.trim_start().starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let (_, rest) = line.split_once('[')?;
    if rest.get(1..2) != Some("]") {
        return None;
    }
    Some(match rest.as_bytes().first()? {
        b' ' => Style::default().fg(pal.tool_output),
        b'>' => Style::default().fg(pal.accent),
        _ => pal.dim(),
    })
}

/// The plan tool answers with the whole plan; the transcript keeps the tasks
/// and drops the prose, which the row above the input already carries.
fn plan_lines(
    name: &str,
    result: Option<&ToolResult>,
    pal: &Palette,
) -> Option<Vec<Line<'static>>> {
    if name != "plan" {
        return None;
    }
    let r = result?;
    if r.is_error {
        return None;
    }
    Some(
        r.output
            .lines()
            .filter_map(|l| {
                let style = task_style(l, pal)?;
                Some(Line::from(Span::styled(format!("  {l}"), style)))
            })
            .collect(),
    )
}

/// Word-wrap to `width` columns. Never returns an empty vector.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut line_w = 0usize;
        for word in para.split(' ') {
            let word_w: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();
            if line_w > 0 && line_w + 1 + word_w > width {
                out.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if word_w > width {
                for c in word.chars() {
                    let cw = c.width().unwrap_or(0);
                    if line_w + cw > width && line_w > 0 {
                        out.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push(c);
                    line_w += cw;
                }
                continue;
            }
            if line_w > 0 {
                line.push(' ');
                line_w += 1;
            }
            line.push_str(word);
            line_w += word_w;
        }
        out.push(line);
    }
    out
}

fn styled(lines: Vec<String>, style: Style) -> impl Iterator<Item = Line<'static>> {
    lines
        .into_iter()
        .map(move |s| Line::from(Span::styled(s, style)))
}

fn with_prefix(
    text: &str,
    width: usize,
    prefix: &str,
    prefix_style: Style,
    body_style: Style,
) -> Vec<Line<'static>> {
    let pw: usize = prefix.chars().map(|c| c.width().unwrap_or(0)).sum();
    let inner = width.saturating_sub(pw).max(1);
    let pad = " ".repeat(pw);
    wrap(text, inner)
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            Line::from(vec![
                Span::styled(
                    if i == 0 {
                        prefix.to_string()
                    } else {
                        pad.clone()
                    },
                    prefix_style,
                ),
                Span::styled(l, body_style),
            ])
        })
        .collect()
}

/// Diff of an `edit_file` call's own arguments, for the moment before the
/// result arrives (and for edits replayed from a session file).
fn edit_preview(call: &ToolCall) -> Option<String> {
    if call.function.name != "edit_file" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&call.function.arguments).ok()?;
    let old = v.get("old_string")?.as_str()?;
    let new = v.get("new_string")?.as_str()?;
    let d = ah_core::tools::diff::unified(old, new, 1);
    // hunk numbers are meaningless outside the file
    Some(
        d.lines()
            .filter(|l| !l.starts_with("@@"))
            .map(|l| format!("{l}\n"))
            .collect(),
    )
}

/// Colour unified-diff lines, clipped to `width` and at most `max` lines.
fn diff_lines(diff: &str, width: usize, max: usize, pal: &Palette) -> Vec<Line<'static>> {
    let total = diff.lines().count();
    let mut out = Vec::with_capacity(total.min(max) + 1);
    for l in diff.lines().take(max) {
        let (prefix, style) = match l.as_bytes().first() {
            Some(b'+') => ("+", Style::default().fg(pal.diff_add)),
            Some(b'-') => ("-", Style::default().fg(pal.diff_del)),
            Some(b'@') => ("", pal.dim()),
            _ => (" ", Style::default().fg(pal.tool_output)),
        };
        let body = if prefix.is_empty() { l } else { &l[1..] };
        let mut text: String = body.chars().take(width.saturating_sub(4)).collect();
        if text.chars().count() < body.chars().count() {
            text.push('…');
        }
        out.push(Line::from(Span::styled(format!("  {prefix}{text}"), style)));
    }
    if total > max {
        out.push(Line::from(Span::styled(
            format!("  … {} more lines (Ctrl-T)", total - max),
            pal.dim(),
        )));
    }
    out
}

fn render(block: &Block, width: usize, view: &View, pal: &Palette) -> Vec<Line<'static>> {
    let width = width.max(4);
    let mut out: Vec<Line<'static>> = Vec::new();
    match block {
        Block::User(s) => {
            out.extend(with_prefix(
                s,
                width,
                &pal.user_prefix,
                pal.bold(pal.user),
                Style::default().fg(pal.user),
            ));
            out.push(Line::default());
        }
        Block::Assistant {
            text,
            reasoning,
            streaming,
            think_ms,
        } => {
            if !reasoning.is_empty() {
                let took = if *think_ms >= 1000 {
                    format!(" for {:.1}s", *think_ms as f64 / 1000.0)
                } else if *think_ms > 0 {
                    format!(" for {think_ms}ms")
                } else {
                    String::new()
                };
                let header = if *streaming && text.is_empty() {
                    "∴ thinking…".to_string()
                } else if view.show_reasoning {
                    format!("∴ thought{took}")
                } else {
                    format!("∴ thought{took} · Ctrl-R shows it")
                };
                out.push(Line::from(Span::styled(header, pal.dim())));
                if view.show_reasoning {
                    out.extend(with_prefix(
                        reasoning.trim_end(),
                        width,
                        "  ",
                        pal.dim(),
                        Style::default()
                            .fg(pal.reasoning)
                            .add_modifier(ratatui::style::Modifier::ITALIC),
                    ));
                    if !text.is_empty() || !*streaming {
                        out.push(Line::default());
                    }
                }
            }
            if text.is_empty() && *streaming && reasoning.is_empty() {
                out.push(Line::from(Span::styled("…", pal.dim())));
            } else if !text.is_empty() && view.markdown {
                out.extend(super::markdown::render(
                    text,
                    pal,
                    super::markdown::Opts {
                        width,
                        highlight: view.code_highlight,
                    },
                ));
            } else if !text.is_empty() {
                out.extend(with_prefix(
                    text,
                    width,
                    &pal.assistant_prefix,
                    pal.bold(pal.accent),
                    Style::default().fg(pal.assistant),
                ));
            }
            if !*streaming {
                out.push(Line::default());
            }
        }
        Block::Tool {
            call,
            result,
            duration_ms,
            expanded,
        } => {
            let max = view.tool_output_lines.max(1) as usize;
            let full = expanded.unwrap_or(view.show_tool_output);
            // file changes show as a diff; while an edit is still running the
            // diff comes from its arguments
            let diff = match result {
                Some(r) if !r.is_error => r.diff.clone(),
                Some(_) => None,
                None => edit_preview(call),
            }
            .filter(|d| !d.is_empty());
            // a plain-English line for the built-ins, the raw arguments for
            // anything else (plugin tools)
            let said = serde_json::from_str(&call.function.arguments)
                .ok()
                .and_then(|v| ah_core::tools::describe::describe(&call.function.name, &v))
                .unwrap_or_else(|| {
                    format!(
                        "{}({})",
                        call.function.name,
                        crate::cli::compact_args(&call.function.arguments)
                    )
                });
            let head = match &diff {
                // the diff already says what changed; the header just counts it
                Some(d) => {
                    let (add, del) =
                        d.lines()
                            .fold((0, 0), |(a, r), l| match l.as_bytes().first() {
                                Some(b'+') => (a + 1, r),
                                Some(b'-') => (a, r + 1),
                                _ => (a, r),
                            });
                    format!("{said} +{add} -{del}")
                }
                None => said,
            };
            let took = match duration_ms {
                Some(d) if *d >= SLOW_MS => format!(" {}", took(*d)),
                _ => String::new(),
            };
            let status = match result {
                None => " …".to_string(),
                Some(r) if r.is_error => format!(" ✗{took}"),
                Some(_) => format!(" ✓{took}"),
            };
            let header = format!("{}{head}{status}", pal.tool_prefix);
            let hstyle = match result {
                Some(r) if r.is_error => Style::default().fg(pal.error),
                _ => Style::default().fg(pal.tool),
            };
            out.extend(styled(wrap(&header, width), hstyle));
            if let Some(d) = diff {
                out.extend(diff_lines(
                    &d,
                    width,
                    if full { usize::MAX } else { max },
                    pal,
                ));
            } else if let Some(lines) = plan_lines(&call.function.name, result.as_ref(), pal) {
                out.extend(lines);
            } else if let Some(r) = result {
                let show = expanded.unwrap_or(view.show_tool_output || r.is_error);
                let total = r.output.lines().count();
                if show {
                    let body: Vec<&str> = r.output.lines().take(max).collect();
                    for l in body {
                        out.extend(styled(
                            wrap(l, width.saturating_sub(2))
                                .into_iter()
                                .map(|s| format!("  {s}"))
                                .collect(),
                            Style::default().fg(pal.tool_output),
                        ));
                    }
                    if total > max {
                        out.push(Line::from(Span::styled(
                            format!("  … {} more lines", total - max),
                            pal.dim(),
                        )));
                    }
                } else if total > 0 {
                    let first: String = r
                        .output
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(width.saturating_sub(4))
                        .collect();
                    let more = if total > 1 {
                        format!(" (+{} lines)", total - 1)
                    } else {
                        String::new()
                    };
                    out.push(Line::from(Span::styled(
                        format!("  {first}{more}"),
                        Style::default().fg(pal.tool_output),
                    )));
                }
            }
            // Every call is its own paragraph.
            out.push(Line::default());
        }
        Block::Summary {
            text,
            tokens,
            expanded,
        } => {
            let full = expanded.unwrap_or(view.show_tool_output);
            let head = match tokens {
                Some((before, after)) => format!(
                    "≡ context compacted: {} → ~{} tokens",
                    super::usage::tokens(*before),
                    super::usage::tokens(*after)
                ),
                None => "≡ context compacted earlier in this session".to_string(),
            };
            let hint = if full {
                " · Ctrl-T hides it"
            } else {
                " · Ctrl-T shows the summary"
            };
            out.extend(styled(wrap(&format!("{head}{hint}"), width), pal.dim()));
            if full {
                out.extend(with_prefix(
                    text.trim(),
                    width,
                    "  ",
                    pal.dim(),
                    Style::default().fg(pal.tool_output),
                ));
            }
            out.push(Line::default());
        }
        Block::Notice(s) => {
            out.extend(styled(wrap(s, width), pal.dim()));
            out.push(Line::default());
        }
        Block::Error(s) => out.extend(with_prefix(
            s,
            width,
            "✗ ",
            pal.bold(pal.error),
            Style::default().fg(pal.error),
        )),
        Block::Image(d) => {
            // The picture itself is drawn by escape codes after the frame.
            // These rows only reserve the room and blank what was there, so
            // the transcript's line arithmetic does not depend on whether the
            // terminal actually painted anything.
            if let Some(l) = image_layout(block, width as u16, view) {
                out.extend(std::iter::repeat_with(Line::default).take(l.rows as usize));
            }
            // The key hint belongs on a chip that stands in for the picture,
            // not under one the terminal has already drawn.
            let key = if view.image.inline { "" } else { &d.open_key };
            out.push(Line::from(Span::styled(
                super::image::chip(d, key),
                pal.dim(),
            )));
            out.push(Line::default());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_header(duration_ms: Option<u64>) -> String {
        use ah_core::abi::ToolFunction;
        let block = Block::Tool {
            call: ToolCall {
                id: "1".into(),
                kind: "function".into(),
                function: ToolFunction {
                    name: "read_file".into(),
                    arguments: r#"{"path": "a.rs"}"#.into(),
                },
            },
            result: Some(ToolResult::ok("one line")),
            duration_ms,
            expanded: Some(false),
        };
        let pal = Palette::from_theme(&ah_core::abi::Theme::default());
        let view = View {
            show_tool_output: false,
            tool_output_lines: 5,
            show_reasoning: false,
            wrap: true,
            markdown: false,
            code_highlight: false,
            image: ImageView::default(),
        };
        render(&block, 80, &view, &pal)[0].to_string()
    }

    #[test]
    fn only_a_slow_call_is_timed() {
        // A quick call is quick; the number would only be noise.
        let quick = tool_header(Some(12));
        assert!(quick.contains("Read(a.rs) ✓"), "{quick}");
        assert!(!quick.contains("12"), "{quick}");
        assert!(tool_header(Some(1400)).contains("Read(a.rs) ✓ 1.4s"));
        assert_eq!(took(95_000), "1m 35s");
        // Session files keep no durations, so replayed calls show none.
        let replayed = tool_header(None);
        assert!(replayed.contains("Read(a.rs) ✓"), "{replayed}");
    }

    /// The rendered summary block, one string per line.
    fn summary_lines(show: bool) -> Vec<String> {
        let block = Block::Summary {
            text: "did a thing\nthen another".into(),
            tokens: Some((48_000, 3_200)),
            expanded: None,
        };
        let pal = Palette::from_theme(&ah_core::abi::Theme::default());
        let view = View {
            show_tool_output: show,
            tool_output_lines: 5,
            show_reasoning: false,
            wrap: true,
            markdown: false,
            code_highlight: false,
            image: ImageView::default(),
        };
        render(&block, 80, &view, &pal)
            .iter()
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn a_compaction_folds_to_one_line_until_asked() {
        let folded = summary_lines(false);
        assert_eq!(
            folded[0],
            "≡ context compacted: 48.0k → ~3.2k tokens · Ctrl-T shows the summary"
        );
        // Nothing of the summary itself, only the header and the blank line.
        assert_eq!(folded.len(), 2, "{folded:?}");
        let open = summary_lines(true);
        assert!(open[0].ends_with("Ctrl-T hides it"), "{open:?}");
        assert!(open.iter().any(|l| l.contains("did a thing")), "{open:?}");
        assert!(open.iter().any(|l| l.contains("then another")), "{open:?}");
    }

    fn image_block(px: (u32, u32)) -> Block {
        Block::Image(Box::new(ImageData {
            id: 3,
            path: std::path::PathBuf::from("/tmp/a.png"),
            mime: "image/png".into(),
            px,
            bytes: 640 * 1024,
            open_key: "Ctrl-O".into(),
        }))
    }

    fn image_view(inline: bool) -> View {
        View {
            show_tool_output: false,
            tool_output_lines: 5,
            show_reasoning: false,
            wrap: true,
            markdown: false,
            code_highlight: false,
            image: ImageView {
                inline,
                cell_px: (10, 20),
                max_cols: 0,
                max_rows: 20,
            },
        }
    }

    #[test]
    fn a_terminal_that_draws_nothing_gets_one_line_and_the_key() {
        let pal = Palette::from_theme(&ah_core::abi::Theme::default());
        let lines = render(&image_block((1024, 768)), 80, &image_view(false), &pal);
        // The chip and the blank line after it.
        assert_eq!(lines.len(), 2, "{lines:?}");
        let chip = lines[0].to_string();
        assert!(chip.contains("1024×768"), "{chip}");
        assert!(chip.contains("Ctrl-O opens it"), "{chip}");
    }

    #[test]
    fn the_rows_reserved_are_the_rows_the_picture_is_placed_on() {
        let pal = Palette::from_theme(&ah_core::abi::Theme::default());
        let view = image_view(true);
        for px in [(1024, 768), (100, 4000), (4000, 100), (1, 1), (33, 47)] {
            for width in [20u16, 40, 100] {
                let block = image_block(px);
                let lines = render(&block, width as usize, &view, &pal);
                let l = image_layout(&block, width, &view).expect("a layout");
                // Reserved rows, then the caption, then a blank line. If this
                // ever drifts, pictures land off the rows they were given.
                assert_eq!(lines.len(), l.rows as usize + 2, "{px:?} at {width}");
                for line in &lines[..l.rows as usize] {
                    assert_eq!(line.to_string(), "", "{px:?} at {width}");
                }
                // Under a picture the caption does not repeat the key.
                assert!(!lines[l.rows as usize].to_string().contains("opens it"));
            }
        }
        // No pixel size: nothing to lay out, so the chip stands in.
        let block = image_block((0, 0));
        assert!(image_layout(&block, 80, &view).is_none());
        assert_eq!(render(&block, 80, &view, &pal).len(), 2);
    }

    #[test]
    fn an_image_is_re_wrapped_when_the_terminal_changes_and_not_otherwise() {
        let pal = Palette::from_theme(&ah_core::abi::Theme::default());
        let mut e = Entry::new(image_block((1024, 768)));
        let mut view = image_view(true);
        // Room enough that the shape of the picture, not the cap, sets the
        // row count — otherwise both cell sizes hit the cap and say nothing.
        view.image.max_rows = 200;
        let first = e.lines(80, &view, &pal, 1).len();
        assert_eq!(e.lines(80, &view, &pal, 1).len(), first);
        // A different cell size is a different layout.
        let mut wider = view;
        wider.image.cell_px = (20, 20);
        assert_ne!(e.lines(80, &wider, &pal, 1).len(), first);
        // And a terminal that draws nothing is the chip again.
        assert_eq!(e.lines(80, &image_view(false), &pal, 1).len(), 2);
    }

    #[test]
    fn wrap_words_and_long_tokens() {
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("x\n\ny", 10), vec!["x", "", "y"]);
        assert_eq!(wrap("", 10), vec![""]);
    }
}
