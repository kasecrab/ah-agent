//! The link between an `ah` session and a phone.
//!
//! Everything that leaves this machine is sealed here first. The relay in the
//! middle is the user's own, but it is still a third machine, so it is treated
//! as one: it routes ciphertext and counts frames, and that is the whole of
//! what it can do.

pub mod code;
pub mod crypto;
pub mod qr;
