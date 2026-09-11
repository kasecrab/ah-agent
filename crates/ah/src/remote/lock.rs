//! Which process is publishing.
//!
//! A pairing has room for one desktop. Two windows open on the same machine
//! would otherwise both dial, and the relay would turn one of them away with
//! an error the user never asked to see — so they settle it here, locally,
//! before anyone dials.
//!
//! `flock` rather than a file with a pid in it: the lock belongs to the open
//! file, so it is released when the process ends however it ends. A window
//! that was killed leaves nothing to clean up and no stale pid to mistake for
//! a live one.

use std::fs::File;
use std::path::PathBuf;

/// Held for as long as this process is the one publishing. Dropping it hands
/// the pairing to whoever is waiting.
pub struct Lock {
    _file: File,
}

impl Lock {
    /// Take it, or find out somebody else has. `None` is an ordinary outcome,
    /// not a failure: it means another window is already publishing.
    pub fn take() -> Option<Self> {
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .ok()?;
        flock(&file).then_some(Self { _file: file })
    }
}

fn path() -> PathBuf {
    ah_core::paths::data_dir().join("remote.lock")
}

/// Take an exclusive lock without waiting for one.
#[cfg(unix)]
fn flock(file: &File) -> bool {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    // LOCK_EX | LOCK_NB, spelled out rather than pulled from a crate for two
    // constants.
    unsafe { flock(file.as_raw_fd(), 2 | 4) == 0 }
}

/// Nothing to co-ordinate with: without `flock` there is no cheap way to tell
/// a live holder from a dead one, and being wrong the other way would stop a
/// window publishing for no reason.
#[cfg(not(unix))]
fn flock(_file: &File) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn one_at_a_time_and_the_next_one_gets_it() {
        let _env = crate::remote::env_guard();
        // The lock is keyed to a path, so the test needs a data dir of its
        // own or it fights whatever else is running on this machine.
        let dir = std::env::temp_dir().join(format!("ah-lock-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let old = std::env::var_os("AH_DATA_DIR");
        // SAFETY: the guard above keeps every other test that reads this
        // variable out while it is moved, and it is put back below.
        unsafe { std::env::set_var("AH_DATA_DIR", &dir) };

        let first = Lock::take();
        assert!(first.is_some(), "nobody was holding it");
        assert!(Lock::take().is_none(), "two publishers at once");
        drop(first);
        assert!(Lock::take().is_some(), "it was not handed on");

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            match old {
                Some(v) => std::env::set_var("AH_DATA_DIR", v),
                None => std::env::remove_var("AH_DATA_DIR"),
            }
        }
    }
}
