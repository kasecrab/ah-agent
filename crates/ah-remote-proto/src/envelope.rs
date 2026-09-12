//! The outer frame: all the relay ever reads.
//!
//! Everything a person said or the model wrote is inside `ct`, sealed. What is
//! out here is only what a router needs — which link, which number, how far a
//! phone has already read. The relay stores these and forwards them; it can
//! count them and time them, and that is all.

use alloc::string::String;
use serde::{Deserialize, Serialize};

use crate::limits::PROTO;

/// A frame on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Envelope {
    /// Desktop to relay: one sealed payload for whoever is listening.
    Pub {
        v: u8,
        link: String,
        seq: u64,
        ct: String,
    },
    /// Phone to relay: one sealed payload for the desktop. `plink` names the
    /// phone's own link, since each phone seals under a key of its own.
    Cmd {
        v: u8,
        link: String,
        plink: String,
        seq: u64,
        ct: String,
    },
    /// Relay to phone: a `pub` with the log number it was stored under. That
    /// number is the only cursor a phone keeps.
    Evt {
        v: u8,
        link: String,
        seq: u64,
        ct: String,
        n: u64,
    },
    /// Phone to relay: start sending from here. The one control the relay
    /// itself acts on, which is why it is not sealed.
    Sub { v: u8, since: u64, max: u32 },
    /// Desktop to relay: still here.
    ///
    /// A desktop with nothing to say sends nothing, and the relay judges
    /// whether one is still there by when it last heard from it. Protocol
    /// pings do not count — the runtime answers those without waking the hub —
    /// so without this a desktop that had been idle for a couple of minutes
    /// could be pushed aside by anybody holding the pairing code. Nothing is
    /// stored and nothing is forwarded; it exists to be heard.
    Ka { v: u8 },
    /// Relay to phone: what you asked for is older than what is kept. The
    /// phone draws a rule in the transcript and asks for a fresh snapshot.
    Gap { v: u8, from: u64 },
    /// Relay to either: something about the connection, not the conversation.
    Ctl {
        v: u8,
        e: Ctl,
        /// The relay's clock, sent only with [`Ctl::Skew`] so a peer can say
        /// how far out it is instead of retrying forever.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_ms: Option<u64>,
    },
}

/// What the relay has to say about a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ctl {
    /// No desktop is connected, so the command was not delivered and was not
    /// kept. Better to say so than to deliver it an hour later.
    Offline,
    /// The signature was refused because the clocks disagree.
    Skew,
    /// Another desktop took over this pairing.
    Replaced,
    /// The pairing was revoked. Nothing will work again.
    Revoked,
    /// The account's daily allowance is spent.
    Quota,
}

impl Envelope {
    /// Still here. Sent by a desktop that has been quiet.
    pub fn keepalive() -> Self {
        Envelope::Ka { v: PROTO }
    }

    pub fn publish(link: &str, seq: u64, ct: String) -> Self {
        Envelope::Pub {
            v: PROTO,
            link: String::from(link),
            seq,
            ct,
        }
    }

    pub fn command(link: &str, plink: &str, seq: u64, ct: String) -> Self {
        Envelope::Cmd {
            v: PROTO,
            link: String::from(link),
            plink: String::from(plink),
            seq,
            ct,
        }
    }

    pub fn event(link: &str, seq: u64, ct: String, n: u64) -> Self {
        Envelope::Evt {
            v: PROTO,
            link: String::from(link),
            seq,
            ct,
            n,
        }
    }

    pub fn control(e: Ctl) -> Self {
        Envelope::Ctl {
            v: PROTO,
            e,
            server_ms: None,
        }
    }

    /// A skew complaint, carrying the clock the peer should compare against.
    pub fn skew(server_ms: u64) -> Self {
        Envelope::Ctl {
            v: PROTO,
            e: Ctl::Skew,
            server_ms: Some(server_ms),
        }
    }

    /// The version the sender declared. A frame from a future protocol is
    /// refused whole rather than read in part.
    pub fn version(&self) -> u8 {
        match self {
            Envelope::Pub { v, .. }
            | Envelope::Cmd { v, .. }
            | Envelope::Evt { v, .. }
            | Envelope::Sub { v, .. }
            | Envelope::Gap { v, .. }
            | Envelope::Ctl { v, .. }
            | Envelope::Ka { v } => *v,
        }
    }
}

/// Whether this names a hub: 32 lowercase hex characters, which is what the
/// key ladder produces and nothing else. Both halves check with this one
/// function, so the harness cannot mint a name the relay will not route.
pub fn is_hub_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keepalive_is_the_smallest_frame_there_is() {
        let text = serde_json::to_string(&Envelope::keepalive()).unwrap();
        assert_eq!(text, r#"{"t":"ka","v":1}"#);
        assert_eq!(
            serde_json::from_str::<Envelope>(&text).unwrap(),
            Envelope::keepalive()
        );
        assert_eq!(Envelope::keepalive().version(), PROTO);
    }
    use alloc::string::ToString;

    #[test]
    fn a_published_frame_survives_the_round_trip() {
        let e = Envelope::publish("0f".repeat(16).as_str(), 7, "Y2lwaGVy".to_string());
        let text = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Envelope>(&text).unwrap(), e);
    }

    #[test]
    fn the_tag_is_where_a_router_can_see_it() {
        let text = serde_json::to_string(&Envelope::control(Ctl::Offline)).unwrap();
        assert!(text.contains(r#""t":"ctl""#), "{text}");
        assert!(text.contains(r#""e":"offline""#), "{text}");
        // Nothing to say about the clock, so nothing is said.
        assert!(!text.contains("server_ms"), "{text}");
    }

    #[test]
    fn only_a_skew_carries_a_clock() {
        let text = serde_json::to_string(&Envelope::skew(1_700_000)).unwrap();
        assert!(text.contains(r#""server_ms":1700000"#), "{text}");
    }

    #[test]
    fn a_hub_id_is_thirty_two_lowercase_hex_characters() {
        assert!(is_hub_id(&"0123456789abcdef".repeat(2)));
        assert!(!is_hub_id(&"0123456789ABCDEF".repeat(2)), "uppercase");
        assert!(!is_hub_id("0123456789abcdef"), "too short");
        assert!(!is_hub_id(&"0123456789abcdeg".repeat(2)), "not hex");
        assert!(!is_hub_id(""), "empty");
        // A traversal attempt is not 32 hex characters either, which is the
        // point: this runs before anything is opened by that name.
        assert!(!is_hub_id("../../etc/passwd"));
    }

    #[test]
    fn every_frame_declares_the_version_it_speaks() {
        assert_eq!(Envelope::control(Ctl::Quota).version(), PROTO);
        assert_eq!(
            Envelope::event("ab", 1, "x".to_string(), 9).version(),
            PROTO
        );
    }
}
