//! Key binding strings (`ctrl-shift-x`, `alt-enter`, `pageup`, `f5`) to
//! crossterm key events.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

pub fn parse(spec: &str) -> Option<Chord> {
    let mut mods = KeyModifiers::NONE;
    let parts: Vec<&str> = spec.split(['-', '+']).collect();
    let (key, modifiers) = parts.split_last()?;
    for m in modifiers {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "c" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" | "opt" | "option" | "m" => mods |= KeyModifiers::ALT,
            "shift" | "s" => mods |= KeyModifiers::SHIFT,
            "super" | "cmd" | "win" => mods |= KeyModifiers::SUPER,
            _ => return None,
        }
    }
    let k = key.to_ascii_lowercase();
    let code = match k.as_str() {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "space" | "spc" => KeyCode::Char(' '),
        "minus" | "dash" => KeyCode::Char('-'),
        "plus" => KeyCode::Char('+'),
        f if f.starts_with('f') && f.len() > 1 && f[1..].chars().all(|c| c.is_ascii_digit()) => {
            KeyCode::F(f[1..].parse().ok()?)
        }
        c if c.chars().count() == 1 => KeyCode::Char(c.chars().next()?),
        _ => return None,
    };
    if code == KeyCode::Tab && mods.contains(KeyModifiers::SHIFT) {
        return Some(Chord {
            code: KeyCode::BackTab,
            mods: mods - KeyModifiers::SHIFT,
        });
    }
    Some(Chord { code, mods })
}

pub fn parse_all(specs: &[String]) -> Vec<Chord> {
    specs.iter().filter_map(|s| parse(s)).collect()
}

/// Does `ev` match `chord`? Character keys compare case-insensitively and
/// ignore an implicit SHIFT unless the binding asked for it.
pub fn matches(chord: &Chord, ev: &KeyEvent) -> bool {
    if !matches!(ev.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }
    match (chord.code, ev.code) {
        (KeyCode::Char(a), KeyCode::Char(b)) => {
            if !a.eq_ignore_ascii_case(&b) {
                return false;
            }
            let want_shift = chord.mods.contains(KeyModifiers::SHIFT)
                || (a.is_ascii_uppercase() && !a.is_ascii_lowercase());
            let ev_mods = ev.modifiers - KeyModifiers::SHIFT;
            let chord_mods = chord.mods - KeyModifiers::SHIFT;
            ev_mods == chord_mods
                && (!want_shift
                    || ev.modifiers.contains(KeyModifiers::SHIFT)
                    || b.is_ascii_uppercase())
        }
        (a, b) => a == b && chord.mods == ev.modifiers,
    }
}

pub fn any_match(chords: &[Chord], ev: &KeyEvent) -> bool {
    chords.iter().any(|c| matches(c, ev))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn parses_and_matches() {
        let c = parse("ctrl-shift-x").unwrap();
        assert!(matches(
            &c,
            &ev(
                KeyCode::Char('X'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
        ));
        assert!(!matches(&c, &ev(KeyCode::Char('x'), KeyModifiers::CONTROL)));
        let c = parse("ctrl-c").unwrap();
        assert!(matches(&c, &ev(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!matches(&c, &ev(KeyCode::Char('c'), KeyModifiers::NONE)));
        assert_eq!(parse("shift-tab").unwrap().code, KeyCode::BackTab);
        assert_eq!(parse("F5").unwrap().code, KeyCode::F(5));
        assert!(matches(
            &parse("enter").unwrap(),
            &ev(KeyCode::Enter, KeyModifiers::NONE)
        ));
        assert!(!matches(
            &parse("enter").unwrap(),
            &ev(KeyCode::Enter, KeyModifiers::SHIFT)
        ));
        assert!(parse("bogus-key").is_none());
    }
}
