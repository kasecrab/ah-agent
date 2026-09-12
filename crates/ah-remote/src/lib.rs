//! The link between an `ah` session and a phone.
//!
//! Everything that leaves this machine is sealed here first. The relay in the
//! middle is the user's own, but it is still a third machine, so it is treated
//! as one: it routes ciphertext and counts frames, and that is the whole of
//! what it can do.

/// The wire format, re-exported so nothing else has to name the crate it
/// lives in: whatever talks to a relay does it through here.
pub use ah_remote_proto as proto;

/// Wiping secrets, re-exported for the same reason.
///
/// Half of what has to be wiped is held by whoever drives a link rather than
/// by the link itself — the pairing code as it was read off the disk, the text
/// of it on its way to a screen — and the wiping has to be done the same way
/// on both sides of the boundary. Handing out the one crate that does it is
/// cheaper than every caller declaring a dependency on it and the two drifting
/// on to different versions.
pub use zeroize;

/// Base64 for the one thing that travels outside a sealed frame: a file a
/// phone asked to see.
pub fn base64(bytes: &[u8]) -> String {
    data_encoding::BASE64.encode(bytes)
}

pub mod code;
pub mod crypto;
pub mod link;
pub mod provision;
pub mod qr;
