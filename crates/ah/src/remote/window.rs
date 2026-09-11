//! The one session an open window has, offered to a phone.
//!
//! A window is the simple case: one engine, already running, already being
//! watched by somebody at the keyboard. What a phone can do here is what that
//! person can do — say something, stop it, answer a question — and nothing
//! that would need a second engine, which is the daemon's job.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ah_core::abi::{Reply, Usage};
use ah_remote::proto::{SessionInfo, SessionState};

use crate::app::{EngineCmd, Inbox};
use crate::remote::sessions::{Act, Sessions};

/// The ways into the running session, cloned from the channels the window
/// itself uses. A phone reaches the engine by exactly the same means as the
/// keyboard does, which is why the two can answer the same question.
pub struct Reach {
    pub cmd: std::sync::mpsc::Sender<EngineCmd>,
    pub perm: std::sync::mpsc::Sender<bool>,
    pub ask: std::sync::mpsc::Sender<Reply>,
    pub cancel: Arc<AtomicBool>,
    pub inbox: Arc<Inbox>,
}

/// What a phone is told about the session being watched.
#[derive(Debug, Clone, Default)]
pub struct Live {
    pub session: String,
    pub name: Option<String>,
    pub cwd: String,
    pub model: String,
    pub busy: bool,
    pub context_tokens: u64,
    pub context_window: u64,
    pub usage: Usage,
}

/// One window's session.
pub struct Window {
    reach: Reach,
    live: Mutex<Live>,
}

impl Window {
    pub fn new(reach: Reach, live: Live) -> Self {
        Self {
            reach,
            live: Mutex::new(live),
        }
    }

    /// Tell it what the session is now, so a phone that asks is not told what
    /// it was a turn ago.
    pub fn update(&self, live: Live) {
        *lock(&self.live) = live;
    }

    /// Whether a phone naming this session means the one that is open. An
    /// empty name means it, too: a phone that has not attached to anything
    /// yet can still say something to the only session there is.
    fn is_ours(&self, session: &str) -> bool {
        session.is_empty() || session == lock(&self.live).session
    }
}

impl Sessions for Window {
    fn list(&self) -> Vec<SessionInfo> {
        let live = lock(&self.live).clone();
        let mut out: Vec<SessionInfo> = ah_core::session::summaries()
            .into_iter()
            .map(|s| SessionInfo {
                live: s.id == live.session,
                id: s.id,
                name: s.name,
                title: s.title,
                cwd: s.cwd,
                model: s.model,
                started_ms: s.started_ms as u64,
                messages: s.messages as u32,
            })
            .collect();
        // A session nobody has said anything in yet is not on disk to be
        // found, and the open window is the one a phone most wants to see.
        if !out.iter().any(|s| s.live) {
            out.insert(
                0,
                SessionInfo {
                    id: live.session.clone(),
                    name: live.name.clone(),
                    title: String::from("a new session"),
                    cwd: live.cwd.clone(),
                    model: live.model.clone(),
                    started_ms: 0,
                    messages: 0,
                    live: true,
                },
            );
        }
        out
    }

    fn state(&self, session: &str) -> Option<SessionState> {
        if !self.is_ours(session) {
            return None;
        }
        let live = lock(&self.live);
        Some(SessionState {
            session: live.session.clone(),
            busy: live.busy,
            model: live.model.clone(),
            cwd: live.cwd.clone(),
            name: live.name.clone(),
            context_tokens: live.context_tokens,
            context_window: live.context_window,
            usage: live.usage,
        })
    }

    fn act(&self, session: &str, act: Act) -> Result<Option<String>, String> {
        if !self.is_ours(session) {
            return Err("this window has one session, and that is not it".into());
        }
        fn gone<T>(_: std::sync::mpsc::SendError<T>) -> String {
            "the session is no longer listening".to_string()
        }
        match act {
            // Said while the turn is running, it goes to the mailbox the loop
            // reads between requests; said while nothing is running, it
            // starts a turn. The phone does not have to know which, and
            // cannot know it without being wrong about it sometimes.
            Act::Submit { text, images } => {
                if lock(&self.live).busy {
                    self.reach.inbox.push(text);
                } else {
                    self.reach
                        .cmd
                        .send(EngineCmd::Submit { text, images })
                        .map_err(gone)?;
                }
            }
            // The same flag Esc sets, so a turn stopped from a phone stops
            // the way a turn stopped at the keyboard does.
            Act::Interrupt => self.reach.cancel.store(true, Ordering::Relaxed),
            Act::AllowTool(allow) => self.reach.perm.send(allow).map_err(gone)?,
            Act::Answer(reply) => self.reach.ask.send(reply).map_err(gone)?,
            Act::Compact(focus) => self
                .reach
                .cmd
                .send(EngineCmd::Compact(focus))
                .map_err(gone)?,
            Act::Clear => self.reach.cmd.send(EngineCmd::Clear).map_err(gone)?,
            Act::Rename(name) => self.reach.cmd.send(EngineCmd::Rename(name)).map_err(gone)?,
            // A window is one session, already chosen. Opening another is the
            // daemon's to do, and saying so is better than half doing it.
            Act::Resume | Act::Start { .. } => {
                return Err(
                    "this window publishes one session; `ah remote serve` opens others".into(),
                );
            }
        }
        Ok(None)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn window(busy: bool) -> (Window, mpsc::Receiver<EngineCmd>, mpsc::Receiver<bool>) {
        let (cmd, cmd_rx) = mpsc::channel();
        let (perm, perm_rx) = mpsc::channel();
        let (ask, _ask_rx) = mpsc::channel();
        let inbox = Arc::new(Inbox::default());
        let w = Window::new(
            Reach {
                cmd,
                perm,
                ask,
                cancel: Arc::new(AtomicBool::new(false)),
                inbox,
            },
            Live {
                session: "abc".into(),
                busy,
                ..Default::default()
            },
        );
        (w, cmd_rx, perm_rx)
    }

    #[test]
    fn a_message_while_it_is_idle_starts_a_turn() {
        let (w, cmd, _) = window(false);
        w.act(
            "abc",
            Act::Submit {
                text: "go".into(),
                images: vec![],
            },
        )
        .unwrap();
        assert!(matches!(cmd.try_recv(), Ok(EngineCmd::Submit { .. })));
    }

    #[test]
    fn a_message_while_it_is_working_does_not_start_another() {
        let (w, cmd, _) = window(true);
        w.act(
            "abc",
            Act::Submit {
                text: "and also".into(),
                images: vec![],
            },
        )
        .unwrap();
        assert!(cmd.try_recv().is_err(), "it started a second turn");
    }

    #[test]
    fn a_session_this_window_does_not_have_is_refused() {
        let (w, _, _) = window(false);
        assert!(w.act("somebody-elses", Act::Interrupt).is_err());
        assert!(w.state("somebody-elses").is_none());
    }

    #[test]
    fn a_phone_that_has_not_attached_yet_still_reaches_the_only_session() {
        // The name is empty until a phone attaches, and there is only one
        // thing it could mean.
        let (w, _, perm) = window(false);
        w.act("", Act::AllowTool(true)).unwrap();
        assert_eq!(perm.try_recv(), Ok(true));
        assert!(w.state("").is_some());
    }

    #[test]
    fn opening_another_session_is_not_this_windows_to_do() {
        let (w, _, _) = window(false);
        let err = w
            .act(
                "abc",
                Act::Start {
                    cwd: "/tmp".into(),
                    model: None,
                    prompt: None,
                },
            )
            .unwrap_err();
        assert!(err.contains("remote serve"), "{err}");
    }
}
