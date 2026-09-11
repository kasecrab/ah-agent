//! Telling a relay about a pairing.
//!
//! The one request the harness makes over plain HTTP rather than over the
//! socket. It hands the relay a key derived from the pairing code — enough to
//! recognise a connection, not enough to read one — and it is refused if that
//! hub has been provisioned already, so a hub name somebody guessed cannot be
//! taken over by whoever asks next.

use std::time::Duration;

use crate::crypto::Keys;
use data_encoding::BASE64URL_NOPAD;

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
pub fn provision(url: &str, keys: &Keys) -> Result<(), Error> {
    let url = url.trim().trim_end_matches('/');
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into();
    let body = serde_json::json!({
        "relay_key": BASE64URL_NOPAD.encode(&keys.relay_key),
    });
    let response = agent
        .post(format!("{url}/hub/{}/provision", keys.hub_id))
        .send_json(&body)
        .map_err(|e| Error::Unreachable(e.to_string()))?;
    match response.status().as_u16() {
        200..=299 => Ok(()),
        409 => Err(Error::Taken),
        410 => Err(Error::Refused("the pairing was revoked".into())),
        s => Err(Error::Refused(format!("it answered {s}"))),
    }
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
