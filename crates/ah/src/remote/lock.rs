//! Which process is publishing, and who is waiting to.
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
//!
//! There are two files and they do different jobs. `remote.lock` is held by
//! whoever is publishing. `remote.claim` is held by whoever is waiting for
//! them to stand down — because `flock` has no way to tell a holder that
//! somebody is queued behind it, so the queue is a second lock the holder can
//! look at. Both are locks rather than flags, and that is the whole point:
//!
//! * A claim lasts exactly as long as the process making it. A window killed
//!   between asking and being given the pairing — a closed terminal, the
//!   out-of-memory killer, a `kill -9` — releases its claim on the way out,
//!   and the daemon picks the pairing back up. The flag file this replaced
//!   was written by the asker and deleted by the asker, so a window that died
//!   in between left the daemon standing down forever over somebody who was
//!   no longer there.
//! * Creating the file is not claiming it. `touch remote.claim` from any
//!   process on the machine used to take the link off the air; now it does
//!   nothing at all, because nothing holds the lock.
//! * A claim that is held but never followed through on — a live process
//!   asking for the pairing and then not taking it — stops mattering after
//!   [`CLAIM_LASTS`], which is comfortably longer than the hand-over a real
//!   window waits out. The daemon is off the air for those few seconds and no
//!   longer. Nothing here can do better than that against a process running
//!   as this user: one of those can simply hold `remote.lock` itself, which
//!   is indistinguishable from a second window, and is the reason the files
//!   live in a directory of this user's own.

use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

/// How long a claim nobody has followed through on goes on being honoured.
///
/// Long enough for a hand-over that is really happening — the window waits
/// three seconds for it — and short enough that a process which asked for the
/// pairing and then did nothing with it costs a daemon a few seconds of
/// publishing rather than all of it.
pub const CLAIM_LASTS: Duration = Duration::from_secs(10);

/// Held for as long as this process is the one publishing. Dropping it hands
/// the pairing to whoever is waiting.
pub struct Lock {
    _file: File,
}

impl Lock {
    /// Take it, or find out somebody else has. `None` is an ordinary outcome,
    /// not a failure: it means another window is already publishing, or one
    /// is waiting for the pairing and this process is not it.
    ///
    /// Standing aside for a claim is what keeps a hand-over from being a
    /// race. Without it a daemon that had just stood down would take the
    /// pairing straight back — it looks free, because the window that asked
    /// for it has not managed to grab it yet — and then stand down again on
    /// its next look at the claim, over and over, with which of the two ends
    /// up publishing decided by who happened to call first.
    pub fn take() -> Option<Self> {
        if Claim::outstanding() {
            return None;
        }
        Self::grab()
    }

    /// Take it as the process that asked for it, which is not kept out by its
    /// own claim.
    pub fn take_as_asked(claim: &Claim) -> Option<Self> {
        // The claim is the caller's, held across this call, and the reason
        // this function exists rather than `take` being used for both.
        let _ = claim;
        Self::grab()
    }

    fn grab() -> Option<Self> {
        let file = open(&lock_path())?;
        flock(&file, LOCK_EXCLUSIVE).then_some(Self { _file: file })
    }
}

/// Held by a process that wants the pairing and is waiting for whoever has it
/// to stand down. Dropping it — or dying, which drops it just the same — is
/// the whole of taking it back.
pub struct Claim {
    _file: File,
}

impl Claim {
    /// Say that this process wants the pairing.
    ///
    /// `None` means somebody else is asking at this moment, which is worth
    /// trying again for: the look [`Claim::outstanding`] takes holds the file
    /// for a few microseconds, and landing in one of those is the ordinary
    /// reason to be turned away here.
    pub fn make() -> Option<Self> {
        let path = claim_path();
        let file = open(&path)?;
        if !flock(&file, LOCK_EXCLUSIVE) {
            return None;
        }
        if !ours(&path) {
            // Somebody else's file, in a directory that should not have let
            // them make one. Claiming through it would mean trusting a
            // timestamp they control, so nothing is claimed at all.
            return None;
        }
        // Writing to it moves its modification time, which is what says the
        // claim is recent. The pid is for whoever reads the directory
        // wondering what is going on; nothing reads it back.
        write_pid(&file);
        Some(Self { _file: file })
    }

    /// Whether somebody is waiting for the pairing right now.
    ///
    /// Three questions, cheapest first: is the file this user's own, was it
    /// claimed recently enough to still count, and is anybody actually
    /// holding it. Only the last of those is the real one, and only the last
    /// of those cannot be faked by anything that can write the file.
    pub fn outstanding() -> bool {
        let path = claim_path();
        if !ours(&path) {
            return false;
        }
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > CLAIM_LASTS);
        if stale {
            return false;
        }
        let Some(file) = open(&path) else {
            return false;
        };
        // Taken and let go again in the same breath: there is no way to ask
        // `flock` whether a lock is held without taking it. A shared one, so
        // that two processes looking at once do not turn each other away.
        !flock(&file, LOCK_SHARED)
    }
}

fn lock_path() -> PathBuf {
    ah_core::paths::data_dir().join("remote.lock")
}

fn claim_path() -> PathBuf {
    ah_core::paths::data_dir().join("remote.claim")
}

fn open(path: &std::path::Path) -> Option<File> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .ok()
}

fn write_pid(file: &File) {
    use std::io::Write as _;
    let _ = file.set_len(0);
    let mut handle: &File = file;
    let _ = handle.write_all(format!("{}\n", std::process::id()).as_bytes());
}

/// Whether a file is this user's own.
///
/// The data directory is this user's, so on an ordinary machine the answer is
/// always yes. It is asked for the machine that is not ordinary, where a
/// world-writable directory would otherwise let anything on the host leave a
/// file that takes a session off the air.
#[cfg(unix)]
fn ours(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: no arguments, no allocation, cannot fail.
    let me = unsafe { geteuid() };
    // A file that is not there yet is about to be made by this process, so it
    // will be this user's.
    std::fs::metadata(path)
        .map(|m| m.uid() == me)
        .unwrap_or(true)
}

#[cfg(not(unix))]
fn ours(_path: &std::path::Path) -> bool {
    true
}

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
}

/// `LOCK_SH` and `LOCK_EX`, spelled out rather than pulled from a crate for
/// two constants. `LOCK_NB` is added to both: nothing here ever waits in the
/// kernel, because every caller has something better to do with the wait.
const LOCK_SHARED: i32 = 1;
const LOCK_EXCLUSIVE: i32 = 2;

/// Take a lock without waiting for one.
#[cfg(unix)]
fn flock(file: &File, operation: i32) -> bool {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }
    // The non-blocking bit, so a lock somebody else holds is an answer rather
    // than a wait.
    unsafe { flock(file.as_raw_fd(), operation | 4) == 0 }
}

/// Nothing to co-ordinate with: without `flock` there is no cheap way to tell
/// a live holder from a dead one, and being wrong the other way would stop a
/// window publishing for no reason. Every lock is given, which means nobody
/// is ever found to be publishing and no claim is ever found to be
/// outstanding — the same as it was before there were two files.
#[cfg(not(unix))]
fn flock(_file: &File, _operation: i32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A data directory of this test's own: the locks are keyed to paths, so
    /// without one the tests fight each other and whatever else is running on
    /// this machine.
    struct Borrowed {
        dir: PathBuf,
        old: Option<std::ffi::OsString>,
        _env: std::sync::MutexGuard<'static, ()>,
    }

    impl Borrowed {
        fn new(name: &str) -> Self {
            let env = crate::remote::env_guard();
            let dir = std::env::temp_dir().join(format!("ah-lock-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::create_dir_all(&dir);
            let old = std::env::var_os("AH_DATA_DIR");
            // SAFETY: the guard keeps every other test that reads this
            // variable out while it is moved, and it is put back on drop.
            unsafe { std::env::set_var("AH_DATA_DIR", &dir) };
            Self {
                dir,
                old,
                _env: env,
            }
        }
    }

    impl Drop for Borrowed {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            // SAFETY: as above; the guard is still held.
            unsafe {
                match self.old.take() {
                    Some(v) => std::env::set_var("AH_DATA_DIR", v),
                    None => std::env::remove_var("AH_DATA_DIR"),
                }
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn one_at_a_time_and_the_next_one_gets_it() {
        let _borrowed = Borrowed::new("one-at-a-time");
        let first = Lock::take();
        assert!(first.is_some(), "nobody was holding it");
        assert!(Lock::take().is_none(), "two publishers at once");
        drop(first);
        assert!(Lock::take().is_some(), "it was not handed on");
    }

    #[test]
    #[cfg(unix)]
    fn a_claim_stops_the_holder_taking_the_pairing_straight_back() {
        let _borrowed = Borrowed::new("claim-wins");
        let publishing = Lock::take().expect("nobody was holding it");
        let claim = Claim::make().expect("nobody else was asking");
        assert!(Claim::outstanding(), "the claim is there to be seen");

        // The holder stands down. Without the claim it would be free to take
        // the pairing back before the window that asked for it managed to,
        // and the two would trade it back and forth.
        drop(publishing);
        assert!(
            Lock::take().is_none(),
            "the pairing was taken back over somebody's head"
        );
        let mine = Lock::take_as_asked(&claim).expect("the one who asked gets it");
        drop(claim);
        assert!(!Claim::outstanding(), "letting go is the whole hand-back");
        drop(mine);
        assert!(Lock::take().is_some(), "and then it is anybody's again");
    }

    #[test]
    #[cfg(unix)]
    fn a_file_nobody_is_holding_asks_for_nothing() {
        let _borrowed = Borrowed::new("touched");
        // What `touch` does, and what any process on this machine could do to
        // the flag file this replaced. It took the link off the air until
        // somebody found the file and deleted it; now it does nothing.
        std::fs::write(claim_path(), b"").unwrap();
        assert!(
            !Claim::outstanding(),
            "creating the file was taken for claiming it"
        );
        assert!(
            Lock::take().is_some(),
            "and it took the pairing off the air"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_claim_from_a_process_that_is_gone_is_not_waited_on() {
        let _borrowed = Borrowed::new("dead-claimant");
        // A window killed between asking for the pairing and being given it.
        // The kernel drops the lock as the process goes, so there is nothing
        // to find and nothing to clean up — which is the difference between a
        // lock and a file somebody was supposed to delete.
        //
        // Another process really has to hold it for this to prove anything,
        // and holding a lock from a second thread of this one would not: the
        // lock belongs to the open file, and this process closing that file is
        // not the same event as a process dying. So: a shell, which says
        // `ready` once it has the lock and then becomes a `sleep` still
        // holding it. A machine without `flock(1)` says nothing and the test
        // steps aside rather than asserting something it did not set up.
        let Ok(mut claimant) = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "exec 9>>'{}'; flock -x 9 || exit 1; echo ready; exec sleep 20",
                claim_path().display()
            ))
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };
        let mut said = [0u8; 5];
        let ready = claimant
            .stdout
            .as_mut()
            .map(|out| {
                use std::io::Read as _;
                out.read(&mut said).unwrap_or(0)
            })
            .unwrap_or(0);
        if &said[..ready] != b"ready" {
            let _ = claimant.kill();
            let _ = claimant.wait();
            return;
        }
        assert!(
            Claim::outstanding(),
            "a lock another process is holding is what a claim is"
        );
        assert!(Lock::take().is_none(), "and it is waited out, not ignored");

        claimant.kill().unwrap();
        claimant.wait().unwrap();
        assert!(
            !Claim::outstanding(),
            "a claim outlived the process that made it"
        );
        assert!(Lock::take().is_some(), "the pairing never came back");
    }

    #[test]
    #[cfg(unix)]
    fn a_claim_nobody_follows_through_on_stops_being_honoured() {
        let _borrowed = Borrowed::new("stale-claim");
        let claim = Claim::make().expect("nobody else was asking");
        assert!(Claim::outstanding());
        // Held, still, by a live process — but asked for so long ago that a
        // hand-over that was really happening would have happened. The daemon
        // is entitled to publish again rather than stay off the air for as
        // long as somebody cares to hold a lock.
        let long_ago = std::time::SystemTime::now() - (CLAIM_LASTS + Duration::from_secs(60));
        File::options()
            .write(true)
            .open(claim_path())
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        assert!(!Claim::outstanding(), "it was honoured forever");
        assert!(Lock::take().is_some(), "and the daemon never came back");
        drop(claim);
    }
}
