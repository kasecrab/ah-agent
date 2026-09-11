//! What is inside the sealed part of a frame. The relay never sees any of it.
//!
//! Two enums rather than one, so a direction is a type: a phone cannot send an
//! event and a desktop cannot send a command, and neither has to be checked
//! for at runtime.
//!
//! Agent events travel as opaque JSON rather than as a typed variant. Carrying
//! `AgentEvent` would mean depending on `ah-core`, which would drag a HTTP
//! client, a wasm interpreter and a TOML parser into a Worker; the encoder and
//! decoder for those 18 shapes live in `ah_core::agent` instead, and only the
//! two ends that care link them.

use ah_abi::{Ask, Message, Reply, ToolCall, Usage};
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One of the desktop's sessions, as a phone sees it in a list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// First line of the first thing the user said, for a session never named.
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub started_ms: u64,
    pub messages: u32,
    /// Whether this session is loaded and can be driven right now, as opposed
    /// to sitting on disk waiting to be resumed.
    pub live: bool,
}

/// Where a session stands, sent on attach and whenever it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub session: String,
    pub busy: bool,
    pub model: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub context_tokens: u64,
    /// 0 when the catalogue does not say, which is also when the session will
    /// not compact on its own.
    pub context_window: u64,
    pub usage: Usage,
}

/// Which machine is on the other end, said once when a link opens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub host: String,
    pub os: String,
    pub ah_version: String,
    pub proto: u8,
    /// `tui` or `daemon`: which one currently holds the publishing lock.
    pub holder: String,
    /// Where a phone may start a session, when this machine will start one at
    /// all. Empty from a window, which publishes the session it already has
    /// and opens no others, so a phone can offer the choice or not offer it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
}

/// Why a link is closing, so the other end knows whether to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bye {
    /// A TUI started and took the lock from the daemon. A new link follows.
    TuiTakingOver,
    /// The process is going away.
    Quit,
    /// The pairing was revoked; do not come back.
    Revoked,
}

/// Sealed frames the desktop sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum FromDesk {
    Hello(Hello),
    Sessions {
        list: Vec<SessionInfo>,
    },
    State(SessionState),
    /// Scrollback for a phone that just attached: whole messages, not replayed
    /// events, because a session reloaded from disk has messages and no events.
    Snapshot {
        session: String,
        messages: Vec<Message>,
        /// Whether older messages were left out to keep the frame small.
        truncated: bool,
    },
    /// The only carrier for agent events. A single event is a list of one, so
    /// the reader has one branch instead of two.
    Events {
        session: String,
        evs: Vec<Value>,
    },
    AskPermission {
        session: String,
        id: u64,
        call: ToolCall,
        reason: String,
    },
    AskUser {
        session: String,
        id: u64,
        ask: Ask,
    },
    /// Somebody answered; take the question down. `by` is a device name or
    /// `desk`, for the line the transcript shows.
    Answered {
        session: String,
        id: u64,
        by: String,
    },
    /// Part of a file the phone asked for. Sent only on request, because an
    /// image nobody opens should not cost anything to carry.
    Blob {
        id: String,
        mime: String,
        seq: u32,
        last: bool,
        b64: String,
    },
    /// Every command gets exactly one of these.
    Ack {
        cmd_seq: u64,
        ok: bool,
        /// The session a command made, for `resume` and `new_session`. The
        /// list that follows would say as much, but only by elimination, and
        /// a phone holding a stale list would eliminate the wrong one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Something about the link worth putting in front of a person.
    Notice {
        text: String,
    },
    Bye {
        reason: Bye,
    },
}

/// Sealed frames a phone sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum FromPhone {
    /// Start watching a session. `since` is the last log number this phone
    /// stored, so the desktop knows how much scrollback to send.
    Attach {
        session: String,
        device: String,
        since: u64,
    },
    Detach {
        session: String,
    },
    /// Say something. Mid-turn it reaches the model between requests; idle it
    /// starts a turn. The desktop decides which, so the phone stays simple.
    Submit {
        session: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    Interrupt {
        session: String,
    },
    AnswerPermission {
        session: String,
        id: u64,
        allow: bool,
    },
    AnswerAsk {
        session: String,
        id: u64,
        reply: Reply,
    },
    /// Load a session that is on disk but not running.
    Resume {
        session: String,
    },
    /// Start a new one. `cwd` is checked against `remote.roots` before
    /// anything is opened.
    NewSession {
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// Fetch a file the session produced, by the path an event named.
    GetBlob {
        session: String,
        path: String,
    },
    Compact {
        session: String,
        #[serde(default)]
        focus: String,
    },
    Clear {
        session: String,
    },
    Rename {
        session: String,
        name: String,
    },
    /// Send the session list again.
    List,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn round_trip_desk(p: FromDesk) {
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(
            serde_json::from_str::<FromDesk>(&text).unwrap(),
            p,
            "{text}"
        );
    }

    fn round_trip_phone(p: FromPhone) {
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(
            serde_json::from_str::<FromPhone>(&text).unwrap(),
            p,
            "{text}"
        );
    }

    #[test]
    fn what_the_desktop_says_survives_the_round_trip() {
        round_trip_desk(FromDesk::Events {
            session: "abc".to_string(),
            evs: vec![serde_json::json!({"type": "text", "text": "hi"})],
        });
        round_trip_desk(FromDesk::Ack {
            cmd_seq: 4,
            ok: false,
            session: None,
            error: Some("no such session".to_string()),
        });
        round_trip_desk(FromDesk::Ack {
            cmd_seq: 5,
            ok: true,
            session: Some("01J8".to_string()),
            error: None,
        });
        round_trip_desk(FromDesk::Hello(Hello {
            host: "desk".to_string(),
            os: "linux".to_string(),
            ah_version: "0.1.0".to_string(),
            proto: 1,
            holder: "daemon".to_string(),
            roots: vec!["/home/rabe".to_string()],
        }));
        round_trip_desk(FromDesk::Bye {
            reason: Bye::TuiTakingOver,
        });
    }

    #[test]
    fn what_a_phone_says_survives_the_round_trip() {
        round_trip_phone(FromPhone::Submit {
            session: "abc".to_string(),
            text: "carry on".to_string(),
            images: vec![],
        });
        round_trip_phone(FromPhone::AnswerPermission {
            session: "abc".to_string(),
            id: 42,
            allow: true,
        });
        round_trip_phone(FromPhone::List);
    }

    #[test]
    fn the_kind_is_the_tag() {
        let text = serde_json::to_string(&FromPhone::Interrupt {
            session: "abc".to_string(),
        })
        .unwrap();
        assert!(text.contains(r#""k":"interrupt""#), "{text}");
    }

    #[test]
    fn a_phone_cannot_be_read_as_a_desktop() {
        let text = serde_json::to_string(&FromPhone::List).unwrap();
        assert!(serde_json::from_str::<FromDesk>(&text).is_err());
    }

    #[test]
    fn a_kind_this_build_does_not_know_is_refused_whole() {
        assert!(serde_json::from_str::<FromPhone>(r#"{"k":"self_destruct"}"#).is_err());
    }

    /// A phone built against the older shape has to keep working, and a
    /// desktop built against the older shape has to be readable here.
    #[test]
    fn a_hello_without_roots_is_still_a_hello() {
        let h: Hello = serde_json::from_str(
            r#"{"host":"desk","os":"linux","ah_version":"0.1.0","proto":1,"holder":"tui"}"#,
        )
        .unwrap();
        assert!(h.roots.is_empty());
        let text = serde_json::to_string(&h).unwrap();
        assert!(!text.contains("roots"), "{text}");
    }

    #[test]
    fn an_empty_image_list_is_not_sent() {
        let text = serde_json::to_string(&FromPhone::Submit {
            session: "abc".to_string(),
            text: "hi".to_string(),
            images: vec![],
        })
        .unwrap();
        assert!(!text.contains("images"), "{text}");
    }
}
