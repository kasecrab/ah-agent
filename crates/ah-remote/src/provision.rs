//! Telling a relay about a pairing.
//!
//! The one request the harness makes over plain HTTP rather than over the
//! socket. It hands the relay a key derived from the pairing code — enough to
//! recognise a connection, not enough to read one — and it is refused if that
//! hub has been provisioned already, so a hub name somebody guessed cannot be
//! taken over by whoever asks next.

use std::time::Duration;

use crate::crypto::Keys;
use ah_remote_proto::{Role, SKEW_MS};
use data_encoding::BASE64URL_NOPAD;
use zeroize::Zeroize;

/// Why a relay would not take a pairing.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// That hub already belongs to a pairing. Make a new code.
    Taken,
    /// The relay is not answering, or not answering like a relay.
    Unreachable(String),
    Refused(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Taken => write!(f, "that pairing already exists on this relay"),
            Error::Unreachable(m) => write!(f, "the relay did not answer: {m}"),
            Error::Refused(m) => write!(f, "the relay refused the pairing: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// What ending a pairing at the relay turned out to mean.
///
/// Worth telling apart because the two want different words said to the
/// person who asked. One of them is "it is over now"; the other is "there was
/// nothing there to end", which is the same outcome and a different story.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revocation {
    /// The relay held this pairing and has ended it: the log and the device
    /// list are gone, both sockets are closed, and the hub is marked revoked.
    Ended,
    /// The relay has nothing under this code to end. It was revoked already,
    /// or the relay has been redeployed since the pairing was made, or the hub
    /// deleted itself after a month with nobody using it. Either way the old
    /// code opens nothing on it.
    NothingToEnd,
}

/// Hand a relay the one key it is allowed to have.
///
/// `token` is the relay's own provisioning secret, set by whoever deployed it.
/// Without it the endpoint would make storage out of nothing on a URL anybody
/// can reach, and a script would own every hub name it could think of.
pub fn provision(url: &str, keys: &Keys, token: &str) -> Result<(), Error> {
    let url = url.trim().trim_end_matches('/');
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into();
    // This body is the one place a key leaves the machine as text, so the copy
    // made to send it is cleared as soon as it has been sent. What the HTTP
    // client kept of it on the way out is beyond reach from here; what is in
    // reach is not leaving a second copy lying about.
    let mut body = serde_json::json!({
        "relay_key": BASE64URL_NOPAD.encode(&keys.relay_key),
    });
    let sent = agent
        .post(format!("{url}/hub/{}/provision", keys.hub_id))
        .header("x-ah-provision", token)
        .send_json(&body);
    if let Some(serde_json::Value::String(encoded)) = body.get_mut("relay_key") {
        encoded.zeroize();
    }
    let response = sent.map_err(|e| Error::Unreachable(e.to_string()))?;
    match response.status().as_u16() {
        200..=299 => Ok(()),
        401 => Err(Error::Refused(
            "the relay did not recognise the provisioning secret. It is the one set with \
             `npx wrangler secret put AH_PROVISION_TOKEN`"
                .into(),
        )),
        409 => Err(Error::Taken),
        410 => Err(Error::Refused("the pairing was revoked".into())),
        503 => Err(Error::Refused(
            "that relay has no provisioning secret set, so it will pair with nobody. Set one \
             with `npx wrangler secret put AH_PROVISION_TOKEN` and deploy again"
                .into(),
        )),
        s => Err(Error::Refused(format!("it answered {s}"))),
    }
}

/// End a pairing at the relay, not merely on this machine.
///
/// Clearing the local credential stops this machine from dialling; it does
/// nothing about the hub, which goes on holding days of ciphertext and goes on
/// accepting whoever still has the code — as the desktop, able to say anything
/// it likes in this machine\'s name. So forgetting a pairing has to reach the
/// relay, and has to say so when it cannot.
///
/// Signed the same way a socket is, because it is the same proof: knowing the
/// code. A `ts` in the signature keeps a copied URL from being useful for
/// longer than the skew window.
///
/// The relay answers 401 for a hub it holds no key for — one that was never
/// provisioned, one on a relay that has been deployed fresh since, one that
/// deleted itself after thirty idle days — and it answers 401 in exactly the
/// same words for a signature it will not accept, on purpose: a relay that
/// distinguished the two would tell anybody with a list of guessed hub names
/// which of them are real. So 401 is read here as "there is nothing on this
/// relay that this code opens", which is what forgetting a pairing is asking
/// for, and it is read that way for the socket as much as for this request —
/// the same key, checked by the same function, decides both.
///
/// The one way that reading can be wrong is a clock: a machine more than
/// [`SKEW_MS`] out signs a timestamp the relay will not take, and would be
/// told its pairing is gone when it is very much still there. So a 401 is
/// checked against the clock in the relay's own answer before it is believed,
/// and a machine whose clock is out is told that instead.
pub fn revoke(url: &str, keys: &Keys) -> Result<Revocation, Error> {
    let url = url.trim().trim_end_matches('/');
    let ts = now_ms();
    let nonce = crate::crypto::new_nonce();
    let sig = keys.sign_connect(Role::Desk, ts, &nonce);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into();
    let response = agent
        .post(format!(
            "{url}/hub/{}/revoke?r=desk&ts={ts}&n={}",
            keys.hub_id,
            esc(&nonce)
        ))
        .header("x-ah-auth", &sig)
        .send_empty()
        .map_err(|e| Error::Unreachable(e.to_string()))?;
    let dated = response
        .headers()
        .get("date")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    what_it_means(response.status().as_u16(), dated.as_deref(), now_ms())
}

/// What the relay's answer to a revocation says about the pairing.
///
/// Apart from the request itself, so that what each status means can be
/// checked without a relay to say it. `theirs` is the `Date` header of the
/// answer and `ours` this machine's clock, both as milliseconds since the
/// epoch; they matter only for a 401, and only to catch the one case where
/// reading a 401 as "nothing to end" would be wrong.
fn what_it_means(status: u16, theirs: Option<&str>, ours: u64) -> Result<Revocation, Error> {
    match status {
        200..=299 => Ok(Revocation::Ended),
        // Already revoked, or a path this relay does not route at all: either
        // way the hub is not going to answer anybody, which is what was being
        // asked for.
        404 | 410 => Ok(Revocation::NothingToEnd),
        401 => match theirs.and_then(http_date_ms).map(|t| t.abs_diff(ours)) {
            Some(gap) if gap > SKEW_MS => Err(Error::Refused(format!(
                "this machine's clock is {} out from the relay's, which is further than a \
                 signature is allowed to be, so the relay turned the revocation away rather \
                 than the pairing being gone. Set the clock and try again",
                how_long(gap)
            ))),
            _ => Ok(Revocation::NothingToEnd),
        },
        s => Err(Error::Refused(format!("it answered {s}"))),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A gap between two clocks, in words rather than in milliseconds.
fn how_long(ms: u64) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..=90 => format!("{seconds} seconds"),
        _ => format!("{} minutes", seconds / 60),
    }
}

/// The `Date` header of an HTTP response, as milliseconds since the epoch.
///
/// Only the one spelling every HTTP server is required to send —
/// `Tue, 15 Nov 1994 08:12:31 GMT` — and anything else reads as `None`, which
/// is treated the same as the header not being there. It is used for one
/// thing, comparing two clocks that are either seconds or minutes apart, so a
/// spelling nobody sends is not worth the code to accept.
fn http_date_ms(date: &str) -> Option<u64> {
    let rest = date.trim().split_once(", ")?.1;
    let mut fields = rest.split(' ');
    let day: i64 = fields.next()?.parse().ok()?;
    let named = fields.next()?;
    let month = 1 + [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == named)? as i64;
    let year: i64 = fields.next()?.parse().ok()?;
    let mut clock = fields.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if fields.next()? != "GMT" || clock.next().is_some() {
        return None;
    }
    // A leap second is spelled 60, and is a real thing to be handed.
    if !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let seconds = days_from_civil(year, month, day)
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + minute * 60 + second)?;
    u64::try_from(seconds).ok()?.checked_mul(1000)
}

/// Days from 1970-01-01 to a date on the proleptic Gregorian calendar.
///
/// Howard Hinnant's algorithm, which is the one everybody's date library is
/// built out of: shift the year so that March is the first month, which puts
/// the leap day at the end where it stops being a special case, then count
/// whole 400-year eras and the days within one.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Percent-encode a signature for a query string.
fn esc(s: &str) -> String {
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

/// Whether a relay is there at all, for a command that wants to say so.
pub fn reachable(url: &str) -> bool {
    let url = url.trim().trim_end_matches('/');
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();
    agent
        .get(format!("{url}/health"))
        .call()
        .is_ok_and(|r| r.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noon on a day the tests can name, as a header and as a number, so a
    /// clock can be put a chosen distance from the relay's.
    const RELAY_SAID: &str = "Sat, 12 Sep 2026 12:00:00 GMT";
    const RELAY_MEANT: u64 = 1_789_214_400_000;

    #[test]
    fn a_relay_with_no_key_for_this_hub_has_nothing_to_revoke() {
        // 401 is the relay's answer for a hub it holds no key for: one that
        // was never provisioned, one on a relay deployed fresh since the
        // pairing was made, one that deleted itself after a month idle. It
        // used to stop `ah remote forget` with "it answered 401" and tell the
        // user to try again, which no amount of trying would help — there is
        // nothing there to end, and that is the state being asked for.
        for status in [200, 204, 401, 404, 410] {
            let meaning = what_it_means(status, Some(RELAY_SAID), RELAY_MEANT);
            assert!(meaning.is_ok(), "{status} became {meaning:?}");
            assert_eq!(
                meaning.unwrap() == Revocation::Ended,
                status < 300,
                "{status} was read as the wrong kind of over"
            );
        }
    }

    #[test]
    fn a_clock_that_is_out_is_not_mistaken_for_a_pairing_that_is_gone() {
        // The one way "401 means there is nothing there" is wrong: a machine
        // whose clock is beyond the skew window signs a timestamp the relay
        // will not take, and is told its pairing is gone while it is very
        // much still there.
        let out = RELAY_MEANT + SKEW_MS + 60_000;
        let Err(Error::Refused(why)) = what_it_means(401, Some(RELAY_SAID), out) else {
            panic!("a clock six minutes out was read as a pairing that had gone");
        };
        assert!(why.contains("clock"), "{why}");
        // Inside the window, the signature would have been taken, so the 401
        // really is the relay having no key for this hub.
        let near = RELAY_MEANT + SKEW_MS - 60_000;
        assert_eq!(
            what_it_means(401, Some(RELAY_SAID), near),
            Ok(Revocation::NothingToEnd)
        );
        // And a relay that sent no clock at all leaves nothing to compare, so
        // the answer is read the way it reads without one.
        assert_eq!(what_it_means(401, None, out), Ok(Revocation::NothingToEnd));
    }

    #[test]
    fn anything_else_the_relay_says_is_still_a_refusal() {
        assert!(what_it_means(500, Some(RELAY_SAID), RELAY_MEANT).is_err());
        assert!(what_it_means(429, Some(RELAY_SAID), RELAY_MEANT).is_err());
    }

    #[test]
    fn the_clock_in_a_relays_answer_is_read_the_way_every_server_writes_it() {
        // The example from the HTTP specification itself, and the epoch.
        assert_eq!(
            http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777_000)
        );
        assert_eq!(http_date_ms("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        // A leap day, on a year that has one and a century that does not.
        assert_eq!(
            http_date_ms("Sat, 29 Feb 2020 00:00:00 GMT"),
            Some(1_582_934_400_000)
        );
        assert_eq!(
            http_date_ms("Tue, 29 Feb 2000 00:00:00 GMT"),
            Some(951_782_400_000)
        );
        // Before the epoch there is no millisecond count to give, and a relay
        // answering with one is a relay whose clock says nothing useful.
        assert_eq!(http_date_ms("Thu, 01 Mar 1900 00:00:00 GMT"), None);
    }

    #[test]
    fn a_clock_this_reader_cannot_make_sense_of_is_not_guessed_at() {
        // Every one of these is a header that is there and says nothing this
        // can be sure of, and being unsure has to read as "no idea" rather
        // than as a number that would decide whether a pairing is live.
        for header in [
            "",
            "Sun Nov 6 08:49:37 1994",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun, 06 Nov 1994 08:49:37 PST",
            "Sun, 06 Xxx 1994 08:49:37 GMT",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:49:37 GMT",
            "Sun, 06 Nov 1994 08:60:37 GMT",
            "Sun, 06 Nov 1994 08:49 GMT",
            "Sun, 06 Nov 1994 08:49:37:11 GMT",
            "Sun, 06 Nov -994 08:49:37 GMT",
        ] {
            assert_eq!(
                http_date_ms(header),
                None,
                "it made something of {header:?}"
            );
        }
    }

    #[test]
    fn a_gap_between_two_clocks_is_said_in_words_a_person_can_act_on() {
        assert_eq!(how_long(0), "0 seconds");
        assert_eq!(how_long(45_000), "45 seconds");
        assert_eq!(how_long(20 * 60 * 1000), "20 minutes");
    }
}
