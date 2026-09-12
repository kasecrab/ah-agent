//! Who is allowed to open a socket.
//!
//! One HMAC, checked with the runtime's own WebCrypto. Not `ring`, which would
//! want a C toolchain and a few hundred kilobytes of bundle to verify a single
//! signature; not a hand-rolled SHA-256, which would put an unaudited
//! primitive on the one boundary that matters here.

use ah_remote_proto::{Role, SKEW_MS, connect_message};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use worker::*;

/// Why a connection was turned away.
pub enum Denied {
    /// The signature does not match, or the query is not a signed one.
    Signature,
    /// The clocks disagree by more than the protocol allows. Worth telling the
    /// caller about, because it is fixable and looks like nothing else.
    Skew,
}

/// What a caller proved, once it has proved it.
pub struct Proof {
    pub role: Role,
    /// The nonce it signed with, which the caller keeps so it can refuse the
    /// same one twice.
    pub nonce: String,
}

/// Check a connect signature. `now` is the relay's clock, in milliseconds.
///
/// `header` is the signature as sent in `x-ah-auth`, which is where it belongs:
/// a URL travels through request logs, proxies and anything that keeps an
/// access record, and a signature in one is a signature anybody reading those
/// can use until it times out. The query is still read, so a peer built before
/// this still connects, and the header is preferred when both are there.
pub async fn verify(
    relay_key: &[u8],
    hub: &str,
    url: &Url,
    header: Option<String>,
    now: u64,
) -> std::result::Result<Proof, Denied> {
    let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    let sig = header
        .filter(|h| !h.is_empty())
        .or_else(|| q.get("h").cloned());
    let (Some(role), Some(ts), Some(nonce), Some(sig)) =
        (q.get("r"), q.get("ts"), q.get("n"), sig.as_ref())
    else {
        return Err(Denied::Signature);
    };
    let (Some(role), Ok(ts)) = (Role::parse(role), ts.parse::<u64>()) else {
        return Err(Denied::Signature);
    };
    // The signature first, and the clock only after it. A clock that is out is
    // worth naming — it is fixable and it looks like nothing else — but only
    // to somebody who has already proved they hold the key. Answered the other
    // way round, this endpoint tells anyone at all what the relay thinks the
    // time is and which timestamps it will take.
    let message = connect_message(hub, role, ts, nonce);
    match hmac_verify(relay_key, message.as_bytes(), sig).await {
        Ok(true) => {}
        _ => return Err(Denied::Signature),
    }
    if now.abs_diff(ts) > SKEW_MS {
        return Err(Denied::Skew);
    }
    Ok(Proof {
        role,
        nonce: nonce.clone(),
    })
}

/// `crypto.subtle.verify`, which compares in constant time so we do not have
/// to.
async fn hmac_verify(key: &[u8], message: &[u8], signature: &str) -> Result<bool> {
    let Some(sig) = base64url(signature) else {
        return Ok(false);
    };
    // Reached through the global rather than by casting it to a browser
    // window type: this runtime has `crypto`, but it is not a DOM global and
    // a cast to one compiles happily and then fails where it cannot be seen.
    let crypto = js_sys::Reflect::get(&js_sys::global(), &"crypto".into())
        .map_err(js_err)?
        .dyn_into::<web_sys::Crypto>()
        .map_err(|_| Error::RustError("no crypto".into()))?;
    let subtle = crypto.subtle();

    let algorithm = js_sys::Object::new();
    js_sys::Reflect::set(&algorithm, &"name".into(), &"HMAC".into())?;
    let hash = js_sys::Object::new();
    js_sys::Reflect::set(&hash, &"name".into(), &"SHA-256".into())?;
    js_sys::Reflect::set(&algorithm, &"hash".into(), &hash)?;

    let usages = js_sys::Array::of1(&"verify".into());
    let imported = JsFuture::from(
        subtle
            .import_key_with_object("raw", &js_sys::Uint8Array::from(key), &algorithm, false, &usages)
            .map_err(js_err)?,
    )
    .await
    .map_err(js_err)?
    .dyn_into::<web_sys::CryptoKey>()
    .map_err(|_| Error::RustError("not a key".into()))?;

    let ok = JsFuture::from(
        subtle
            .verify_with_object_and_u8_array_and_u8_array(
                &algorithm,
                &imported,
                &sig,
                &mut message.to_vec(),
            )
            .map_err(js_err)?,
    )
    .await
    .map_err(js_err)?;
    Ok(ok.as_bool().unwrap_or(false))
}

/// The relay's own key, as it was stored.
pub fn decode_key(encoded: &str) -> Vec<u8> {
    base64url(encoded).unwrap_or_default()
}

/// Base64url without padding, the way every signature on this wire is written.
fn base64url(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    // One string, one byte sequence. Accepting a trailing character whose
    // spare bits are not zero would mean several spellings decoding to the
    // same signature, which is harmless while nothing is deduplicated and a
    // hole the moment something is.
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in s.bytes() {
        let v = ALPHABET.iter().position(|a| *a == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if bits > 0 && acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

fn js_err(e: wasm_bindgen::JsValue) -> Error {
    Error::RustError(format!("{e:?}"))
}
