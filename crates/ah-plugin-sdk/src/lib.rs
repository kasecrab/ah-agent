//! SDK for `ah` plugins. Build with `--target wasm32-unknown-unknown` as a
//! `cdylib`.
//!
//! ```ignore
//! use ah_plugin_sdk::prelude::*;
//!
//! fn manifest() -> Manifest {
//!     Manifest { name: "hello".into(), hooks: vec![Hook::OnLoad], ..Default::default() }
//! }
//!
//! fn handle(hook: Hook, input: Value) -> Result<Value, String> {
//!     match hook {
//!         Hook::OnLoad => Ok(json!({"settings_patch": {"theme": {"accent": "magenta"}}})),
//!         _ => Ok(Value::Null),
//!     }
//! }
//!
//! ah_plugin_sdk::plugin!(manifest, handle);
//! ```

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

pub use ah_abi;
pub use serde_json;

pub mod prelude {
    pub use crate::{
        LogLevel, command_matches, command_segments, host_call, kv_get, kv_set, log, settings_get,
    };
    pub use ah_abi::*;
    pub use alloc::borrow::ToOwned;
    pub use alloc::format;
    pub use alloc::string::{String, ToString};
    pub use alloc::vec;
    pub use alloc::vec::Vec;
    pub use serde_json::{self, Value, json};
}

pub use ah_abi::LogLevel;

mod raw {
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "ah")]
    unsafe extern "C" {
        pub fn log(level: i32, ptr: i32, len: i32);
        pub fn host_call(name_ptr: i32, name_len: i32, in_ptr: i32, in_len: i32) -> i32;
        pub fn host_read(dst: i32, cap: i32) -> i32;
    }

    // native stubs so the crate builds off-wasm
    #[cfg(not(target_arch = "wasm32"))]
    pub unsafe fn log(_level: i32, _ptr: i32, _len: i32) {}
    #[cfg(not(target_arch = "wasm32"))]
    pub unsafe fn host_call(_np: i32, _nl: i32, _ip: i32, _il: i32) -> i32 {
        -1
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub unsafe fn host_read(_dst: i32, _cap: i32) -> i32 {
        -1
    }
}

/// Allocate `len` bytes for the host to write into.
#[unsafe(no_mangle)]
pub extern "C" fn ah_alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = Vec::with_capacity(len.max(1) as usize);
    let ptr = v.as_mut_ptr();
    core::mem::forget(v);
    ptr as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn ah_free(ptr: i32, len: i32) {
    if ptr == 0 {
        return;
    }
    unsafe {
        drop(Vec::from_raw_parts(
            ptr as *mut u8,
            len.max(1) as usize,
            len.max(1) as usize,
        ));
    }
}

/// Take ownership of a buffer the host wrote with `ah_alloc`.
///
/// # Safety
/// `ptr`/`len` must come from the host for the current call.
pub unsafe fn take_input(ptr: i32, len: i32) -> Vec<u8> {
    if ptr == 0 {
        return Vec::new();
    }
    unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len.max(1) as usize) }
}

/// Leak a buffer to the host as a packed `(ptr << 32) | len`. Host frees it.
///
/// Nothing to say is a null pointer, not a pointer to nothing. An empty `Vec`
/// owns no allocation, and the address it reports is the dangling one every
/// `Vec<u8>` starts life with — 1. Packing that address would have the host
/// read zero bytes from it, which works, and then hand it back to `ah_free`,
/// which does not: `ah_free` rounds a zero length up to one and so frees a
/// one-byte block at address 1 that nobody ever allocated, leaving the
/// plugin's own heap quietly wrong. A null pointer is what both the host and
/// `ah_free` already read as "nothing to take and nothing to give back".
pub fn give_output(mut bytes: Vec<u8>) -> i64 {
    bytes.shrink_to_fit();
    if bytes.is_empty() {
        // `shrink_to_fit` has already handed back whatever this was holding,
        // so there is nothing here to leak and nothing for the host to free.
        return 0;
    }
    let len = bytes.len() as i64;
    let ptr = bytes.as_mut_ptr() as i64;
    core::mem::forget(bytes);
    (ptr << 32) | (len & 0xFFFF_FFFF)
}

/// Write a log line to the host log (visible with `AH_LOG=1`).
pub fn log_str(level: LogLevel, msg: &str) {
    unsafe { raw::log(level as i32, msg.as_ptr() as i32, msg.len() as i32) }
}

#[macro_export]
macro_rules! log {
    ($lvl:expr, $($t:tt)*) => { $crate::log_str($lvl, &$crate::prelude::format!($($t)*)) };
}

/// Generic host capability call. See `ah-core::plugins::host_call` for names.
pub fn host_call(name: &str, input: &serde_json::Value) -> Result<serde_json::Value, String> {
    let body = serde_json::to_vec(input).map_err(|e| e.to_string())?;
    let n = unsafe {
        raw::host_call(
            name.as_ptr() as i32,
            name.len() as i32,
            body.as_ptr() as i32,
            body.len() as i32,
        )
    };
    let len = n.unsigned_abs() as usize;
    let mut buf: Vec<u8> = alloc::vec![0u8; len];
    let got = unsafe { raw::host_read(buf.as_mut_ptr() as i32, len as i32) };
    if got < 0 {
        return Err("host_read failed".into());
    }
    buf.truncate(got as usize);
    if n < 0 {
        return Err(String::from_utf8_lossy(&buf).into_owned());
    }
    serde_json::from_slice(&buf).map_err(|e| e.to_string())
}

/// Read from the merged settings by JSON pointer (`"/theme/accent"`), or the
/// whole tree with `""`.
pub fn settings_get(pointer: &str) -> serde_json::Value {
    host_call("settings_get", &serde_json::Value::String(pointer.into()))
        .unwrap_or(serde_json::Value::Null)
}

/// A shell command read the way the harness reads it: split on `;`, `|`, `&`
/// and newlines, whitespace-normalised, with a leading `sudo`, `env`, `nohup`,
/// `time` or `VAR=value` dropped.
///
/// Match on these rather than on the raw text. A substring match over the text
/// refuses `git commit -m "the rm -rf / case"` and allows `rm -fr /`.
pub fn command_segments(command: &str) -> Vec<String> {
    match host_call(
        "command_segments",
        &serde_json::Value::String(command.into()),
    ) {
        Ok(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(alloc::string::ToString::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// The first of `rules` that matches `command`, by the harness's own rule
/// syntax: a rule matches a segment that equals it or starts with it followed
/// by a space, and a rule ending in `*` matches any continuation.
pub fn command_matches(command: &str, rules: &[String]) -> Option<String> {
    match host_call(
        "command_matches",
        &serde_json::json!({"command": command, "rules": rules}),
    ) {
        Ok(serde_json::Value::String(s)) => Some(s),
        _ => None,
    }
}

pub fn kv_get(key: &str) -> Option<String> {
    match host_call("kv_get", &serde_json::Value::String(key.into())) {
        Ok(serde_json::Value::String(s)) => Some(s),
        _ => None,
    }
}

pub fn kv_set(key: &str, value: Option<&str>) {
    let _ = host_call("kv_set", &serde_json::json!({"key": key, "value": value}));
}

/// Declare the plugin entry points. `$manifest: fn() -> Manifest`,
/// `$handler: fn(Hook, Value) -> Result<Value, String>`.
#[macro_export]
macro_rules! plugin {
    ($manifest:path, $handler:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn ah_manifest() -> i64 {
            let m: $crate::ah_abi::Manifest = $manifest();
            $crate::give_output($crate::serde_json::to_vec(&m).unwrap_or_default())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn ah_call(hook_ptr: i32, hook_len: i32, in_ptr: i32, in_len: i32) -> i64 {
            let hook_bytes = unsafe { $crate::take_input(hook_ptr, hook_len) };
            let input_bytes = unsafe { $crate::take_input(in_ptr, in_len) };
            let out = $crate::dispatch(&hook_bytes, &input_bytes, $handler);
            $crate::give_output(out)
        }
    };
}

/// Shared by the `plugin!` macro: decode, run, encode the `{ok}`/`{err}` envelope.
pub fn dispatch(
    hook: &[u8],
    input: &[u8],
    handler: fn(ah_abi::Hook, serde_json::Value) -> Result<serde_json::Value, String>,
) -> Vec<u8> {
    install_panic_hook();
    let name = core::str::from_utf8(hook).unwrap_or("");
    let result = match ah_abi::Hook::parse(name) {
        None => Err(alloc::format!("unknown hook {name}")),
        Some(h) => match serde_json::from_slice::<serde_json::Value>(if input.is_empty() {
            b"null"
        } else {
            input
        }) {
            Ok(v) => handler(h, v),
            Err(e) => Err(alloc::format!("input json: {e}")),
        },
    };
    let env = match result {
        Ok(v) => serde_json::json!({ "ok": v }),
        Err(e) => serde_json::json!({ "err": e }),
    };
    serde_json::to_vec(&env).unwrap_or_default()
}

/// Forward panic messages to the host log before the abort trap.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            log_str(LogLevel::Error, &alloc::format!("panic: {info}"));
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host frees exactly what it is handed, so what it is handed for an
    /// empty answer has to be a pointer it will leave alone.
    #[test]
    fn an_empty_buffer_is_handed_over_as_nothing_at_all() {
        assert_eq!(give_output(Vec::new()), 0);
        // The same for a buffer that has room in it but nothing to say: the
        // room is given back rather than leaked, and the host still gets 0.
        assert_eq!(give_output(Vec::with_capacity(64)), 0);
    }
}
