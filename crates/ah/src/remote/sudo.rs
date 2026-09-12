//! Asking for the password once, here, so nothing has to ask for it later.
//!
//! `sudo` reads a password from the terminal it was started from. A session
//! the daemon started for a phone has no such terminal — or worse, it has the
//! one the daemon was launched in, which nobody is reading — so a command
//! needing a password does not fail. It waits, silently, until the tool gives
//! up on it, and the phone is shown nothing the whole time.
//!
//! The way out is to have the password already given. `sudo -v` asks for it
//! while somebody is still standing there, and `sudo -n -v` every minute keeps
//! that from expiring for as long as the daemon runs. Every `sudo` a session
//! runs after that finds the credentials already cached and asks nothing.
//!
//! This is a real grant and it is worth being plain about it: for as long as
//! the daemon is serving, anything a paired phone starts can become root
//! without anybody being asked again. That is why it is off unless asked for.

use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often the timestamp is refreshed. `sudo` forgets after fifteen minutes
/// by default, and a minute leaves room for a laptop that was asleep.
const REFRESH: Duration = Duration::from_secs(60);

/// How long the loop sleeps between looks at whether it should stop.
const STEP: Duration = Duration::from_secs(2);

/// Ask for the password now, while there is somebody to ask.
///
/// Returns what went wrong rather than a bare `false`: "there is no terminal
/// here" and "the password was wrong" want different things done about them,
/// and the person reading the daemon's output is the one who has to do it.
pub fn cache() -> Result<(), String> {
    if !std::io::stdin().is_terminal() {
        return Err(
            "there is no terminal here to type a password into. Start `ah remote serve --sudo` \
             in one, without --detach"
                .into(),
        );
    }
    println!("sudo needs your password once, now, so no session has to ask for it later");
    match Command::new("sudo").arg("-v").status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err("sudo would not take the password".into()),
        Err(e) => Err(format!("sudo could not be run: {e}")),
    }
}

/// Keep the cached password from expiring for as long as the daemon serves.
///
/// The thread stops when `cancel` is set. If a refresh is ever refused — a
/// `sudoers` with `timestamp_timeout=0` never caches anything, and the machine
/// waking from sleep can lose the record — it says so once and stops trying,
/// because a `sudo` that has to ask is exactly what this was for.
pub fn keep_fresh(cancel: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("ah-remote-sudo".into())
        .spawn(move || {
            let mut waited = Duration::ZERO;
            while !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(STEP);
                waited += STEP;
                if waited < REFRESH {
                    continue;
                }
                waited = Duration::ZERO;
                let fresh = Command::new("sudo")
                    .args(["-n", "-v"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !fresh {
                    println!(
                        "sudo will not stay cached on this machine, so a session that needs it \
                         would wait on a prompt nobody can answer. Restart with --sudo to try again"
                    );
                    return;
                }
            }
        })
        .expect("spawn sudo refresher")
}

/// Ask the person starting the daemon, once, and take silence for no.
///
/// Only asked when there is a terminal to ask at. Started from `systemd` or
/// with `--detach` there is nobody to answer, and a daemon that blocked
/// waiting for an answer would never come up at all.
pub fn ask() -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("may sessions driven from a phone run sudo? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes")
}
