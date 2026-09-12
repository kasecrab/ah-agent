//! Driving this session from a phone.
//!
//! Everything that leaves the machine is sealed in `ah-remote` before it gets
//! here; this half is the part the user sees — pairing, and saying what state
//! the link is in.

pub mod cli;
pub mod daemon;
pub mod lock;
pub mod publisher;
pub mod sessions;
pub mod sudo;
pub mod window;

/// The environment belongs to the process, and cargo runs these tests side by
/// side in it. Anything that points `AH_DATA_DIR` somewhere of its own waits
/// here first.
#[cfg(test)]
pub fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}
