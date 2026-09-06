//! Types shared between the host, the plugin SDK and plugins.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod message;
pub mod plugin;
pub mod settings;
pub mod tool;

pub use message::*;
pub use plugin::*;
pub use settings::*;
pub use tool::*;

/// ABI version. Host refuses plugins whose manifest declares a different major.
pub const ABI_VERSION: u32 = 1;
