//! API key discovery and the OpenRouter PKCE login flow.

use std::io::{Read, Write};
use std::net::TcpListener;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(default)]
    openrouter_api_key: Option<String>,
}

/// Resolution order: env, settings, credentials file.
pub fn api_key(settings_key: Option<&str>) -> Option<String> {
    if let Ok(k) = std::env::var("OPENROUTER_API_KEY")
        && !k.trim().is_empty()
    {
        return Some(k.trim().to_string());
    }
    if let Some(k) = settings_key.filter(|k| !k.trim().is_empty()) {
        return Some(k.trim().to_string());
    }
    load_credentials()
        .openrouter_api_key
        .filter(|k| !k.is_empty())
}

fn load_credentials() -> Credentials {
    std::fs::read_to_string(crate::paths::credentials_file())
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_key(key: &str) -> Result<()> {
    let path = crate::paths::credentials_file();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let creds = Credentials {
        openrouter_api_key: Some(key.to_string()),
    };
    let text = toml::to_string(&creds).map_err(|e| Error::Config(e.to_string()))?;
    std::fs::write(&path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn clear_key() -> Result<()> {
    let path = crate::paths::credentials_file();
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Run the OpenRouter PKCE flow. `open_url` is called with the URL the user
/// must visit; the function blocks until the callback arrives or `timeout`.
pub fn login_pkce(
    base_url: &str,
    open_url: &dyn Fn(&str),
    timeout: std::time::Duration,
) -> Result<String> {
    let verifier = random_token(48);
    let challenge = base64url(&sha256(verifier.as_bytes()));
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let callback = format!("http://127.0.0.1:{port}/callback");
    let url = format!(
        "https://openrouter.ai/auth?callback_url={}&code_challenge={}&code_challenge_method=S256",
        urlencode(&callback),
        challenge
    );
    open_url(&url);
    listener.set_nonblocking(false)?;
    let deadline = std::time::Instant::now() + timeout;
    let code = loop {
        if std::time::Instant::now() > deadline {
            return Err(Error::Auth("timed out waiting for browser callback".into()));
        }
        let (mut stream, _) = listener.accept()?;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let first = req.lines().next().unwrap_or("");
        let code = first
            .split_whitespace()
            .nth(1)
            .and_then(|path| path.split_once('?'))
            .map(|(_, q)| q)
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("code=")))
            .map(urldecode);
        match code {
            Some(c) if !c.is_empty() => {
                let body = "<html><body style='font-family:sans-serif'><h3>ah: logged in. You can close this tab.</h3></body></html>";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                break c;
            }
            _ => {
                let _ = write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        }
    };
    let resp = crate::provider::openrouter::OpenRouter::post_json_unauthenticated(
        base_url,
        "/auth/keys",
        &serde_json::json!({ "code": code, "code_verifier": verifier, "code_challenge_method": "S256" }),
    )?;
    let key = resp
        .get("key")
        .and_then(|k| k.as_str())
        .ok_or_else(|| Error::Auth(format!("no key in response: {resp}")))?;
    save_key(key)?;
    Ok(key.to_string())
}

fn random_token(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !ok {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(1)
            ^ (std::process::id() as u128).rotate_left(64);
        let mut x = seed as u64 | 1;
        for b in bytes.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
    }
    base64url(&bytes)
}

pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn base64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

/// SHA-256 (FIPS 180-4).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b, c, d, e, f, g, hh].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        let hex = |b: [u8; 32]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(
            hex(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn base64url_and_urlencode() {
        assert_eq!(base64url(b"hello"), "aGVsbG8");
        assert_eq!(base64url(b"hi"), "aGk");
        assert_eq!(
            urlencode("http://127.0.0.1:1/cb"),
            "http%3A%2F%2F127.0.0.1%3A1%2Fcb"
        );
        assert_eq!(urldecode("a%20b+c"), "a b c");
    }
}
