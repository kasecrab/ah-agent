//! Telling a relay about a pairing.
//!
//! The one request the harness makes over plain HTTP rather than over the
//! socket. It hands the relay a key derived from the pairing code — enough to
//! recognise a connection, not enough to read one — and it is refused if that
//! hub has been provisioned already, so a hub name somebody guessed cannot be
//! taken over by whoever asks next.

use std::time::Duration;

use crate::crypto::Keys;
use ah_remote_proto::Role;
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
pub fn revoke(url: &str, keys: &Keys) -> Result<(), Error> {
    let url = url.trim().trim_end_matches('/');
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
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
    match response.status().as_u16() {
        // Already revoked, or never provisioned: either way the hub is not
        // going to answer anybody, which is what was being asked for.
        200..=299 | 404 | 410 => Ok(()),
        s => Err(Error::Refused(format!("it answered {s}"))),
    }
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
