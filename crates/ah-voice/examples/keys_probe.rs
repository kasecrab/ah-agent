//! What does this terminal actually report when a key is held?
//!
//! Dictation wants to know when the talk key goes down and when it comes back
//! up. A terminal only says so under the kitty keyboard protocol; without it
//! a held key looks like a stream of presses with no end, which is a very
//! different thing to react to.
//!
//! Run it, hold the space bar for a second, let go, then press `q`.
//!
//!   cargo run -p ah-voice --example keys_probe

use std::io::Write;
use std::time::Instant;

fn main() {
    // The same flags `ah` pushes at startup.
    print!("\x1b[>1u\x1b[=15;1u");
    let _ = std::io::stdout().flush();
    println!("hold SPACE for a second, let go, then press q to finish\n");

    let start = Instant::now();
    let mut press = 0u32;
    let mut repeat = 0u32;
    let mut release = 0u32;

    let raw = raw_mode();
    let mut buf = [0u8; 64];
    loop {
        let n = match std::io::Read::read(&mut std::io::stdin(), &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let seen = String::from_utf8_lossy(&buf[..n]).to_string();
        let at = start.elapsed().as_millis();
        // Kitty reports an event kind in the last numeric field before `u`:
        // 1 press, 2 repeat, 3 release. A plain terminal sends the bare byte.
        let kind = if seen.contains('u') && seen.starts_with('\x1b') {
            if seen.contains(":3") {
                release += 1;
                "RELEASE"
            } else if seen.contains(":2") {
                repeat += 1;
                "repeat"
            } else {
                press += 1;
                "press"
            }
        } else {
            press += 1;
            "press (plain byte, no event kind)"
        };
        println!("{at:>6} ms  {kind:<34} {:?}", seen);
        if seen.contains('q') {
            break;
        }
    }
    drop(raw);

    print!("\x1b[<u");
    let _ = std::io::stdout().flush();
    println!("\n  presses {press} · repeats {repeat} · releases {release}");
    if release > 0 {
        println!("  This terminal reports releases: hold-to-talk works properly.");
    } else if repeat > 0 {
        println!("  Releases are not reported, but repeats are: a gap in the");
        println!("  repeats can stand in for letting go.");
    } else {
        println!("  Neither releases nor repeats are reported. A held key is");
        println!("  indistinguishable from the same key pressed again and again,");
        println!("  which is the whole problem.");
        println!("  In WezTerm: add `enable_kitty_keyboard = true` to wezterm.lua.");
    }
}

/// Put the terminal in raw mode with `stty`, and put it back afterwards.
struct Raw(String);

fn raw_mode() -> Raw {
    let saved = std::process::Command::new("stty")
        .arg("-g")
        .stdin(std::process::Stdio::inherit())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let _ = std::process::Command::new("stty")
        .args(["raw", "-echo"])
        .stdin(std::process::Stdio::inherit())
        .status();
    Raw(saved)
}

impl Drop for Raw {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            let _ = std::process::Command::new("stty")
                .arg(&self.0)
                .stdin(std::process::Stdio::inherit())
                .status();
        }
    }
}
