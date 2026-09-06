//! Theme settings to ratatui styles.

use ah_core::abi::{BorderStyle, Theme};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders};

#[derive(Debug, Clone)]
pub struct Palette {
    pub fg: Color,
    pub bg: Color,
    pub accent: Color,
    pub user: Color,
    pub assistant: Color,
    pub reasoning: Color,
    pub tool: Color,
    pub tool_output: Color,
    pub error: Color,
    pub dim: Color,
    pub border: Color,
    pub border_focus: Color,
    pub status_fg: Color,
    pub status_bg: Color,
    pub input_fg: Color,
    pub input_bg: Color,
    pub heading: Color,
    pub link: Color,
    pub quote: Color,
    pub code: Color,
    pub code_bg: Color,
    pub rule: Color,
    pub syn_keyword: Style,
    pub syn_string: Style,
    pub syn_comment: Style,
    pub syn_number: Style,
    pub syn_type: Style,
    pub syn_function: Style,
    pub syn_builtin: Style,
    pub syn_attr: Style,
    pub diff_add: Color,
    pub diff_del: Color,
    pub border_style: BorderStyle,
    pub user_prefix: String,
    pub assistant_prefix: String,
    pub tool_prefix: String,
    pub input_prefix: String,
    pub spinner: Vec<String>,
}

pub fn parse_color(s: &str) -> Color {
    let t = s.trim();
    if t.is_empty() {
        return Color::Reset;
    }
    if let Ok(n) = t.parse::<u8>() {
        return Color::Indexed(n);
    }
    t.parse::<Color>().unwrap_or(Color::Reset)
}

/// A colour optionally followed by `dim`, `bold`, `italic` or `underline`:
/// `"cyan dim"`.
pub fn parse_style(s: &str) -> Style {
    let mut words = s.split_whitespace();
    let mut st = Style::default().fg(parse_color(words.next().unwrap_or("")));
    for w in words {
        st = match w {
            "dim" => st.add_modifier(Modifier::DIM),
            "bold" => st.add_modifier(Modifier::BOLD),
            "italic" => st.add_modifier(Modifier::ITALIC),
            "underline" => st.add_modifier(Modifier::UNDERLINED),
            _ => st,
        };
    }
    st
}

impl Palette {
    pub fn from_theme(t: &Theme) -> Self {
        Self {
            fg: parse_color(&t.fg),
            bg: parse_color(&t.bg),
            accent: parse_color(&t.accent),
            user: parse_color(&t.user),
            assistant: parse_color(&t.assistant),
            reasoning: parse_color(&t.reasoning),
            tool: parse_color(&t.tool),
            tool_output: parse_color(&t.tool_output),
            error: parse_color(&t.error),
            dim: parse_color(&t.dim),
            border: parse_color(&t.border),
            border_focus: parse_color(&t.border_focus),
            status_fg: parse_color(&t.status_fg),
            status_bg: parse_color(&t.status_bg),
            input_fg: parse_color(&t.input_fg),
            input_bg: parse_color(&t.input_bg),
            heading: parse_color(&t.heading),
            link: parse_color(&t.link),
            quote: parse_color(&t.quote),
            code: parse_color(&t.code),
            code_bg: parse_color(&t.code_bg),
            rule: parse_color(&t.rule),
            syn_keyword: parse_style(&t.syn_keyword),
            syn_string: parse_style(&t.syn_string),
            syn_comment: parse_style(&t.syn_comment),
            syn_number: parse_style(&t.syn_number),
            syn_type: parse_style(&t.syn_type),
            syn_function: parse_style(&t.syn_function),
            syn_builtin: parse_style(&t.syn_builtin),
            syn_attr: parse_style(&t.syn_attr),
            diff_add: parse_color(&t.diff_add),
            diff_del: parse_color(&t.diff_del),
            border_style: t.border_style,
            user_prefix: t.user_prefix.clone(),
            assistant_prefix: t.assistant_prefix.clone(),
            tool_prefix: t.tool_prefix.clone(),
            input_prefix: t.input_prefix.clone(),
            spinner: if t.spinner.is_empty() {
                vec!["|".into(), "/".into(), "-".into(), "\\".into()]
            } else {
                t.spinner.clone()
            },
        }
    }

    pub fn base(&self) -> Style {
        Style::default().fg(self.fg).bg(self.bg)
    }

    pub fn has_borders(&self) -> bool {
        self.border_style != BorderStyle::None
    }

    pub fn has_side_borders(&self) -> bool {
        !matches!(self.border_style, BorderStyle::None | BorderStyle::Lines)
    }

    fn border_type(&self) -> BorderType {
        match self.border_style {
            BorderStyle::Double => BorderType::Double,
            BorderStyle::Thick => BorderType::Thick,
            BorderStyle::Rounded => BorderType::Rounded,
            BorderStyle::Plain | BorderStyle::Lines | BorderStyle::None => BorderType::Plain,
        }
    }

    fn border_color(&self, focused: bool) -> Style {
        Style::default().fg(if focused {
            self.border_focus
        } else {
            self.border
        })
    }

    /// Popup/overlay block: always fully bordered (rounded for the `lines` style).
    pub fn block(&self, focused: bool) -> Block<'static> {
        let kind = match self.border_style {
            BorderStyle::Lines | BorderStyle::None => BorderType::Rounded,
            _ => self.border_type(),
        };
        Block::default()
            .borders(Borders::ALL)
            .border_type(kind)
            .border_style(self.border_color(focused))
    }

    /// Input box block in the configured style.
    pub fn input_block(&self, focused: bool) -> Block<'static> {
        match self.border_style {
            BorderStyle::None => Block::default(),
            BorderStyle::Lines => Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_type(BorderType::Plain)
                .border_style(self.border_color(focused)),
            _ => Block::default()
                .borders(Borders::ALL)
                .border_type(self.border_type())
                .border_style(self.border_color(focused)),
        }
    }

    pub fn dim(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn bold(&self, c: Color) -> Style {
        Style::default().fg(c).add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#bd93f9"), Color::Rgb(0xbd, 0x93, 0xf9));
        assert_eq!(parse_color("dark_gray"), Color::DarkGray);
        assert_eq!(parse_color("123"), Color::Indexed(123));
        assert_eq!(parse_color("reset"), Color::Reset);
        assert_eq!(parse_color("nonsense"), Color::Reset);
    }
}
