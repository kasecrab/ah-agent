//! Transcript blocks with a per-block wrapped-line cache.

use ah_core::abi::{ToolCall, ToolResult};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use super::theme::Palette;

pub enum Block {
    User(String),
    Assistant {
        text: String,
        reasoning: String,
        streaming: bool,
    },
    Tool {
        call: ToolCall,
        result: Option<ToolResult>,
        duration_ms: u64,
        expanded: Option<bool>,
    },
    Notice(String),
    Error(String),
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
                | (view.wrap as u64) << 2,
        );
        mix(&mut k, view.tool_output_lines as u64);
        match &self.block {
            Block::User(s) | Block::Notice(s) | Block::Error(s) => mix(&mut k, s.len() as u64),
            Block::Assistant {
                text,
                reasoning,
                streaming,
            } => {
                mix(&mut k, text.len() as u64);
                mix(&mut k, reasoning.len() as u64);
                mix(&mut k, *streaming as u64);
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
                mix(&mut k, *duration_ms);
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
        } => {
            if view.show_reasoning && !reasoning.is_empty() {
                out.extend(with_prefix(
                    reasoning,
                    width,
                    "… ",
                    pal.dim(),
                    Style::default().fg(pal.reasoning),
                ));
            }
            if text.is_empty() && *streaming && (reasoning.is_empty() || !view.show_reasoning) {
                out.push(Line::from(Span::styled("…", pal.dim())));
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
            let args = crate::cli::compact_args(&call.function.arguments);
            let status = match result {
                None => " …".to_string(),
                Some(r) if r.is_error => format!(" ✗ {duration_ms} ms"),
                Some(_) => format!(" ✓ {duration_ms} ms"),
            };
            let header = format!("{}{} {args}{status}", pal.tool_prefix, call.function.name);
            let hstyle = match result {
                Some(r) if r.is_error => Style::default().fg(pal.error),
                _ => Style::default().fg(pal.tool),
            };
            out.extend(styled(wrap(&header, width), hstyle));
            if let Some(r) = result {
                let show = expanded.unwrap_or(view.show_tool_output || r.is_error);
                let total = r.output.lines().count();
                if show {
                    let max = view.tool_output_lines.max(1) as usize;
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
        }
        Block::Notice(s) => out.extend(styled(wrap(s, width), pal.dim())),
        Block::Error(s) => out.extend(with_prefix(
            s,
            width,
            "✗ ",
            pal.bold(pal.error),
            Style::default().fg(pal.error),
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_words_and_long_tokens() {
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("x\n\ny", 10), vec!["x", "", "y"]);
        assert_eq!(wrap("", 10), vec![""]);
    }
}
