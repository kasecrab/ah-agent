//! The wire format between an `ah` session, the relay, and a phone.
//!
//! It lives in its own crate because the relay is a Cloudflare Worker compiled
//! to `wasm32-unknown-unknown`, and the harness is a native binary: one crate
//! compiled by both is the only way the two halves cannot disagree about a
//! frame. Nothing here reads a clock, opens a socket or does any crypto —
//! `std::time::SystemTime` panics on that target, and the relay handles only
//! sealed bytes it cannot read. Timestamps arrive as parameters.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod envelope;
pub mod keys;
pub mod limits;
pub mod payload;

pub use envelope::*;
pub use keys::*;
pub use limits::*;
pub use payload::*;
