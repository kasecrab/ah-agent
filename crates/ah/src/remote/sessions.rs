//! What a machine has to offer a phone.
//!
//! The publisher knows how to seal something and put it on a socket. It does
//! not know what a session is, how many there are, or what answering a
//! question does — that belongs to whoever owns them, which is a window when
//! one is open and the daemon when one is not.

use ah_core::abi::Reply;
use ah_remote::proto::{SessionInfo, SessionState};

/// Something a phone asked to have done.
///
/// Reading — listing sessions, attaching, scrollback — never reaches here:
/// the publisher answers those itself, because they are the same answer
/// whoever is holding the sessions.
pub enum Act {
    Submit {
        text: String,
        images: Vec<String>,
    },
    Interrupt,
    /// An answer to a tool prompt. The publisher has already checked it is
    /// the question that is actually on screen.
    AllowTool(bool),
    Answer(Reply),
    Compact(String),
    Clear,
    Rename(String),
    /// Load a session that is on disk but not running.
    Resume,
    /// Begin one that does not exist yet. The new session's id comes back.
    Start {
        cwd: String,
        model: Option<String>,
        prompt: Option<String>,
    },
}

impl Act {
    /// What to put in the transcript when this is done, or nothing when it is
    /// not worth a line.
    pub fn said(&self) -> Option<String> {
        Some(match self {
            Act::Submit { text, .. } => format!("remote: \u{201c}{}\u{201d}", first_line(text)),
            Act::Interrupt => "remote: stopped".into(),
            Act::AllowTool(true) => "remote: allowed".into(),
            Act::AllowTool(false) => "remote: denied".into(),
            Act::Answer(_) => "remote: answered".into(),
            Act::Clear => "remote: cleared".into(),
            Act::Start { cwd, .. } => format!("remote: started a session in {cwd}"),
            Act::Compact(_) | Act::Rename(_) | Act::Resume => return None,
        })
    }
}

/// The sessions one machine is offering.
pub trait Sessions: Send + Sync {
    /// Everything worth listing: what is running, and what is on disk.
    fn list(&self) -> Vec<SessionInfo>;

    /// Where one session stands, or `None` if this machine has no such thing.
    fn state(&self, session: &str) -> Option<SessionState>;

    /// Do it. `Ok(Some(id))` names a session that did not exist before.
    fn act(&self, session: &str, act: Act) -> Result<Option<String>, String>;

    /// Where this machine will start a session, if it will start one at all.
    ///
    /// Empty means it will not, which is a window's answer: it has the one
    /// session it was opened with. A phone asks so it can offer the choice
    /// rather than make somebody type a path it may then refuse.
    fn roots(&self) -> Vec<String> {
        Vec::new()
    }
}

/// The first line of something, short enough for a status area.
pub fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 60 {
        format!("{}\u{2026}", line.chars().take(60).collect::<String>())
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_line_is_shortened_for_the_transcript() {
        assert_eq!(first_line("hello\nthere"), "hello");
        assert_eq!(first_line("   spaced   "), "spaced");
        let cut = first_line(&"w".repeat(200));
        assert!(cut.chars().count() <= 61, "{} chars", cut.chars().count());
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn what_is_worth_saying_is_said_and_the_rest_is_not() {
        assert!(Act::Interrupt.said().is_some());
        assert!(Act::AllowTool(false).said().unwrap().contains("denied"));
        // Renaming and compacting show up in the transcript on their own.
        assert!(Act::Rename("x".into()).said().is_none());
        assert!(Act::Compact(String::new()).said().is_none());
    }
}
