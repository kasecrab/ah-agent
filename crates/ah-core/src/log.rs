//! File logger, off unless `AH_LOG` is set (`1` or a path). Level via `AH_LOG_LEVEL`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

struct Sink {
    file: Mutex<File>,
    level: u8,
}

static SINK: OnceLock<Option<Sink>> = OnceLock::new();

fn sink() -> Option<&'static Sink> {
    SINK.get_or_init(|| {
        let target = std::env::var("AH_LOG").ok()?;
        let path = if target == "1" || target.is_empty() {
            crate::paths::data_dir().join("ah.log")
        } else {
            std::path::PathBuf::from(target)
        };
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        // It holds whatever was being debugged, which is whatever the session
        // was doing.
        let file = crate::paths::owner_only(OpenOptions::new().create(true).append(true))
            .open(path)
            .ok()?;
        let level = match std::env::var("AH_LOG_LEVEL").as_deref() {
            Ok("error") => 0,
            Ok("warn") => 1,
            Ok("debug") | Ok("trace") => 3,
            _ => 2,
        };
        Some(Sink {
            file: Mutex::new(file),
            level,
        })
    })
    .as_ref()
}

#[inline]
pub fn enabled(level: u8) -> bool {
    matches!(sink(), Some(s) if s.level >= level)
}

pub fn write(level: u8, args: std::fmt::Arguments<'_>) {
    if let Some(s) = sink() {
        if s.level < level {
            return;
        }
        let tag = ["E", "W", "I", "D"][level.min(3) as usize];
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if let Ok(mut f) = s.file.lock() {
            let _ = writeln!(f, "{ts} {tag} {args}");
        }
    }
}

#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::log::write(0, format_args!($($t)*)) }; }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::log::write(1, format_args!($($t)*)) }; }
#[macro_export]
macro_rules! info { ($($t:tt)*) => { if $crate::log::enabled(2) { $crate::log::write(2, format_args!($($t)*)) } }; }
#[macro_export]
macro_rules! debug { ($($t:tt)*) => { if $crate::log::enabled(3) { $crate::log::write(3, format_args!($($t)*)) } }; }
