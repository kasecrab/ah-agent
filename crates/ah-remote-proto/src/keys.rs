//! The byte strings the two halves must agree on, and nothing else.
//!
//! No crypto happens here — `ah-remote` runs this ladder with `ring`, the
//! phone with `javax.crypto`, and the relay never runs it at all. What lives
//! here are the inputs, because a stray space in a salt or a signed message is
//! a 401 or a decrypt failure with nothing to debug.

use alloc::format;
use alloc::string::String;

/// HKDF salt for everything derived from the pairing code.
pub const SALT: &[u8] = b"ah-remote v1";

/// HKDF salt for the per-connection keys derived from those.
pub const LINK_SALT: &[u8] = b"ah-remote link v1";

/// Names the hub the two peers meet at. Derived, not random, so the phone can
/// find the hub from the code alone.
pub const INFO_HUB: &[u8] = b"hub";

/// The one leaf the relay is given. Payload keys cannot be derived from it.
pub const INFO_RELAY: &[u8] = b"relay";

/// Desktop to phone.
pub const INFO_D2P: &[u8] = b"d2p";

/// Phone to desktop.
pub const INFO_P2D: &[u8] = b"p2d";

/// Which way a frame is travelling. Bound into the sealed data, so a frame
/// reflected back at its sender does not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Desktop to phone.
    D2p = 0,
    /// Phone to desktop.
    P2d = 1,
}

/// Which end of the pairing a socket belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Desk,
    Phone,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Desk => "desk",
            Role::Phone => "phone",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "desk" => Some(Role::Desk),
            "phone" => Some(Role::Phone),
            _ => None,
        }
    }
}

/// The message a connect signature covers. Both halves build it here and
/// nowhere else.
pub fn connect_message(hub: &str, role: Role, ts: u64, nonce: &str) -> String {
    format!("ah/v1 connect|{hub}|{}|{ts}|{nonce}", role.as_str())
}

/// Additional data every frame is sealed under: which link, which phone, which
/// way, and which number in the sequence. A relay that reorders frames, or
/// labels one as coming from somewhere else, cannot make it open.
///
/// `plink` is zeros for a frame the desktop sends to everyone.
pub fn aad(link: &[u8; 16], plink: Option<&[u8; 16]>, dir: Dir, seq: u64) -> [u8; 41] {
    let mut out = [0u8; 41];
    out[..16].copy_from_slice(link);
    if let Some(p) = plink {
        out[16..32].copy_from_slice(p);
    }
    out[32] = dir as u8;
    out[33..].copy_from_slice(&seq.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signed_message_is_exactly_this() {
        assert_eq!(
            connect_message(
                "0123456789abcdef0123456789abcdef",
                Role::Desk,
                1700,
                "nOnCe"
            ),
            "ah/v1 connect|0123456789abcdef0123456789abcdef|desk|1700|nOnCe"
        );
    }

    #[test]
    fn a_role_survives_the_round_trip() {
        for r in [Role::Desk, Role::Phone] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Role::parse("relay"), None);
    }

    #[test]
    fn the_two_directions_seal_differently() {
        let link = [7u8; 16];
        assert_ne!(aad(&link, None, Dir::D2p, 1), aad(&link, None, Dir::P2d, 1));
    }

    #[test]
    fn a_frame_for_one_phone_is_not_sealed_as_a_frame_for_all() {
        let link = [7u8; 16];
        let plink = [9u8; 16];
        assert_ne!(
            aad(&link, Some(&plink), Dir::P2d, 4),
            aad(&link, None, Dir::P2d, 4)
        );
    }

    #[test]
    fn the_sequence_number_is_part_of_what_is_sealed() {
        let link = [7u8; 16];
        assert_ne!(aad(&link, None, Dir::D2p, 1), aad(&link, None, Dir::D2p, 2));
    }
}
