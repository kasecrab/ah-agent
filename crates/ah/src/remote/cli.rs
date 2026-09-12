//! `ah remote`: pairing, and what the link is doing.

use ah_core::auth;
use ah_remote::{code, crypto, provision, qr};

use crate::RemoteCmd;
use crate::app::AnyError;

pub fn subcommand(cmd: RemoteCmd, o: &crate::Overrides) -> Result<(), AnyError> {
    match cmd {
        RemoteCmd::Pair { url, token } => pair(url, token),
        RemoteCmd::Serve { detach, sudo } => crate::remote::daemon::serve(o, detach, sudo),
        RemoteCmd::Status => status(),
        RemoteCmd::Forget { local } => forget(local),
    }
}

/// Make a new pairing and show it as something to point a camera at.
///
/// The code is generated here and never travels over the network: the relay is
/// told a key derived from it, and the phone is told the code itself, once, by
/// being shown it.
fn pair(url: Option<String>, token: Option<String>) -> Result<(), AnyError> {
    let url = match url.or_else(auth::remote_url) {
        Some(u) => check_url(u)?,
        None => {
            return Err("no relay yet. Deploy one and pass it: \
                        ah remote pair --url https://ah-relay.<you>.workers.dev"
                .into());
        }
    };

    let token = token
        .or_else(|| std::env::var("AH_PROVISION_TOKEN").ok())
        .filter(|t| !t.trim().is_empty())
        .ok_or(
            "this relay wants its provisioning secret, so that its URL alone is not enough to \
             make pairings on it. Pass --token, or set AH_PROVISION_TOKEN; it is the value you \
             gave `npx wrangler secret put AH_PROVISION_TOKEN`.",
        )?;

    let raw = crypto::new_code();
    let shown = code::format(&raw);
    let keys = crypto::Keys::derive(&raw);

    // The relay is told first. Writing the pairing down before it is accepted
    // would leave a machine believing in one the relay has never heard of.
    match provision::provision(&url, &keys, token.trim()) {
        Ok(()) => {}
        // Two codes deriving the same hub is not a thing that happens; a hub
        // that is taken means this code was made before and is being made
        // again, which a fresh one settles.
        Err(provision::Error::Taken) => {
            return Err("that pairing already exists. Try again.".into());
        }
        Err(e) => return Err(format!("{e}").into()),
    }
    auth::save_remote(&shown, &url)?;

    let link = format!(
        "razorback://pair?u={}&c={}",
        escape(&url),
        shown.replace('-', "")
    );
    match qr::Qr::encode(link.as_bytes()) {
        Some(symbol) => {
            println!();
            print!("{}", symbol.to_terminal());
        }
        // Only a relay URL long enough to overflow a version 10 symbol can do
        // this, and the code below still pairs.
        None => println!("\n(the relay URL is too long to fit in a QR code)"),
    }
    println!("  scan it with the phone's camera, or type the code:\n");
    println!("      {shown}\n");
    println!("  relay {url}");
    println!("  hub   {}…", &keys.hub_id[..8]);
    println!();
    println!("Kept in {}.", auth::credentials_path().display());
    println!("Anyone with this code can start a session on this machine, so treat");
    println!("it like a key. `ah remote forget` makes it useless.");
    Ok(())
}

fn status() -> Result<(), AnyError> {
    let (Some(stored), Some(url)) = (auth::remote_code(), auth::remote_url()) else {
        println!("not paired. `ah remote pair --url <relay>` sets one up.");
        return Ok(());
    };
    let Some(raw) = code::parse(&stored) else {
        return Err("the stored pairing code is not a code. `ah remote pair` again.".into());
    };
    let keys = crypto::Keys::derive(&raw);
    println!("paired");
    println!("  relay {url}");
    println!("  hub   {}…", &keys.hub_id[..8]);
    println!(
        "  {}",
        if provision::reachable(&url) {
            "the relay is answering"
        } else {
            "the relay is not answering"
        }
    );
    Ok(())
}

/// End the pairing, at the relay first and here second.
///
/// The order matters. Clearing the credential first would leave nothing to
/// sign the revocation with, and a hub that is still provisioned still answers
/// whoever holds the old code — as this machine, not merely as a listener. So
/// the relay is told while there is still something to tell it with, and a
/// relay that cannot be told stops the command rather than being skipped over.
fn forget(local: bool) -> Result<(), AnyError> {
    let pairing = auth::remote_code()
        .and_then(|c| code::parse(&c))
        .zip(auth::remote_url());
    match (local, pairing) {
        (false, Some((raw, url))) => {
            let keys = crypto::Keys::derive(&raw);
            if let Err(why) = provision::revoke(&url, &keys) {
                return Err(format!(
                    "the relay still holds this pairing, so forgetting it here would leave it \
                     open to whoever has the code: {why}\n\
                     Try again when the relay answers, or `ah remote forget --local` to forget \
                     it here anyway."
                )
                .into());
            }
            println!("the pairing is revoked at the relay: the old code now opens nothing");
        }
        (false, None) => println!("nothing paired here"),
        (true, _) => println!(
            "forgotten here only. The relay still holds this pairing, and the old code still \
             opens it"
        ),
    }
    auth::clear_remote()?;
    println!("the code and relay are gone from this machine");
    Ok(())
}

/// Refuse a relay a phone could not reach, or could reach in the clear.
///
/// The app refuses plain HTTP outright, so a `http://` relay would pair and
/// then silently never connect. Loopback is let through because that is what
/// `wrangler dev` serves.
fn check_url(url: String) -> Result<String, AnyError> {
    let url = url.trim().trim_end_matches('/').to_string();
    if !url.starts_with("https://") && !is_loopback(&url) {
        return Err(format!("the relay has to be https, or loopback for testing: {url}").into());
    }
    Ok(url)
}

/// Whether a plain-HTTP URL points at this machine.
///
/// By host, not by prefix. `http://localhost.example.com/` starts with
/// `http://localhost` and is somewhere else entirely, which is the sort of
/// thing that turns a testing convenience into a way to have a pairing code
/// typed into somebody else's relay in the clear.
fn is_loopback(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    // Up to the first `/`, `?` or `#`, then drop any port.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        // `[::1]:8787`
        Some(v6) => return v6.split(']').next() == Some("::1"),
        None => authority.split(':').next().unwrap_or_default(),
    };
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// Percent-encode what a URL means inside another URL's query string.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_that_is_not_https_is_refused() {
        assert!(check_url("http://relay.example.com".into()).is_err());
        assert!(check_url("ftp://relay.example.com".into()).is_err());
        assert!(check_url("relay.example.com".into()).is_err());
        assert!(check_url("https://relay.example.com".into()).is_ok());
        // What `wrangler dev` serves, so testing does not need a certificate.
        assert!(check_url("http://127.0.0.1:8787".into()).is_ok());
        assert!(check_url("http://localhost:8787".into()).is_ok());
    }

    #[test]
    fn a_trailing_slash_is_not_part_of_the_relay() {
        assert_eq!(
            check_url("https://relay.example.com/ ".into()).unwrap(),
            "https://relay.example.com"
        );
    }

    #[test]
    fn only_this_machine_counts_as_loopback() {
        assert!(is_loopback("http://127.0.0.1:8799"));
        assert!(is_loopback("http://localhost:8787/"));
        assert!(is_loopback("http://[::1]:8787"));
        // Every one of these is somebody else's host, spelled to look like
        // this one.
        assert!(!is_loopback("http://localhost.attacker.tld"));
        assert!(!is_loopback("http://127.0.0.1.attacker.tld"));
        assert!(!is_loopback("http://attacker.tld/?x=http://localhost"));
        assert!(!is_loopback("http://user@attacker.tld"));
        assert!(!is_loopback("https://ah-relay.example.workers.dev"));
        // And the check that matters is the one on the whole URL.
        assert!(check_url("http://localhost.attacker.tld".into()).is_err());
        assert!(check_url("https://ah-relay.example.workers.dev".into()).is_ok());
        assert!(check_url("http://127.0.0.1:8799".into()).is_ok());
    }

    #[test]
    fn a_url_inside_a_url_survives_it() {
        assert_eq!(
            escape("https://ah-relay.x.workers.dev"),
            "https%3A%2F%2Fah-relay.x.workers.dev"
        );
    }

    #[test]
    fn the_deep_link_a_phone_gets_is_one_a_phone_can_read() {
        let raw = crypto::new_code();
        let shown = code::format(&raw);
        let link = format!(
            "razorback://pair?u={}&c={}",
            escape("https://ah-relay.x.workers.dev"),
            shown.replace('-', "")
        );
        // The app parses `c` back with the same reader that accepts a typed
        // code, so the two paths cannot drift apart.
        let c = link.split("&c=").nth(1).unwrap();
        assert_eq!(code::parse(c), Some(raw));
        assert!(qr::Qr::encode(link.as_bytes()).is_some(), "it has to fit");
    }
}
