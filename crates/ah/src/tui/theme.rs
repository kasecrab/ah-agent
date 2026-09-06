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
    pub border_style: BorderStyle,
    pub user_prefix: String,
    pub assistant_prefix: String,
    pub tool_prefix: String,
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
            border_style: t.border_style,
            user_prefix: t.user_prefix.clone(),
            assistant_prefix: t.assistant_prefix.clone(),
            tool_prefix: t.tool_prefix.clone(),
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

    /// Bordered block (or none) in the configured style.
    pub fn block(&self, focused: bool) -> Block<'static> {
        if !self.has_borders() {
            return Block::default();
        }
        let kind = match self.border_style {
            BorderStyle::Plain => BorderType::Plain,
            BorderStyle::Rounded => BorderType::Rounded,
            BorderStyle::Double => BorderType::Double,
            BorderStyle::Thick => BorderType::Thick,
            BorderStyle::None => BorderType::Plain,
        };
        Block::default()
            .borders(Borders::ALL)
            .border_type(kind)
            .border_style(Style::default().fg(if focused {
                self.border_focus
            } else {
                self.border
            }))
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
