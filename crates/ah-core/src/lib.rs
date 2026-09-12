//! Provider client, tools, agent loop, settings, sessions, wasm plugin host.

/// A lock around the process environment, for tests.
///
/// The environment belongs to the process and cargo runs tests side by side in
/// it, so a test that sets `AH_CONFIG_DIR` or `OPENROUTER_API_KEY` is changing
/// something every other test on every other thread can see. Every test that
/// writes one of these, and every test that reads a value derived from one,
/// takes this first — otherwise the `SAFETY` note on `set_var` saying it is
/// this thread's own business is simply not true.
///
/// It is one lock for the whole crate rather than one per module, because the
/// thing being shared is one environment.
#[doc(hidden)]
pub mod test_env {
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Hold this for as long as the environment is being written or read.
    /// A poisoned lock is not a reason to fail: the environment is whatever
    /// the last test left, and the next one sets what it needs.
    pub fn guard() -> std::sync::MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub mod agent;
pub mod agents;
pub mod auth;
pub mod clipboard;
pub mod docs;
pub mod error;
pub mod image;
pub mod instructions;
pub mod jobs;
pub mod log;
pub mod models;
pub mod paths;
pub mod plan;
pub mod plugins;
pub mod policy;
pub mod provider;
pub mod session;
pub mod settings;
pub mod skills;
pub mod tools;

pub use ah_abi as abi;
pub use error::{Error, Result};
