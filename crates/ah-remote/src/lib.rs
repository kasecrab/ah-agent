//! The link between an `ah` session and a phone.
//!
//! Everything that leaves this machine is sealed here first. The relay in the
//! middle is the user's own, but it is still a third machine, so it is treated
//! as one: it routes ciphertext and counts frames, and that is the whole of
//! what it can do.

/// The wire format, re-exported so nothing else has to name the crate it
/// lives in: whatever talks to a relay does it through here.
pub use ah_remote_proto as proto;

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
