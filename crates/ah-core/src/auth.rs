//! API key discovery and storage.
//!
//! Every secret this harness keeps passes through here, and it passes through
//! repeatedly: the file is read again for each question asked of it, so the
//! same three keys are spelled out on the heap over and over in processes that
//! run for hours. What is done about that is that each of those copies is
//! overwritten when it has served its purpose rather than simply dropped —
//! see [`wipe`] for why dropping is not enough on its own.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(default)]
    openrouter_api_key: Option<String>,
    /// Dictation against Deepgram's live socket. Nothing else uses it.
    #[serde(default)]
    deepgram_api_key: Option<String>,
    /// The pairing code a phone was given, as typed characters. Everything the
    /// remote link needs is derived from it, so this is the only part worth
    /// keeping and the only part worth guarding.
    #[serde(default)]
    remote_code: Option<String>,
    /// The relay that pairing was made against.
    #[serde(default)]
    remote_url: Option<String>,
}

/// Not the derived one. `{:?}` on this used to print all three keys in full,
/// and a secret only has to reach a log once for it to have reached it.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn held(field: &Option<String>) -> &'static str {
            match field {
                Some(_) => "set",
                None => "unset",
            }
        }
        f.debug_struct("Credentials")
            .field("openrouter_api_key", &held(&self.openrouter_api_key))
            .field("deepgram_api_key", &held(&self.deepgram_api_key))
            .field("remote_code", &held(&self.remote_code))
            // The relay is an address, not a secret.
            .field("remote_url", &self.remote_url)
            .finish()
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        for secret in [
            &mut self.openrouter_api_key,
            &mut self.deepgram_api_key,
            &mut self.remote_code,
        ]
        .into_iter()
        .flatten()
        {
            wipe(secret);
        }
    }
}

/// Overwrite a secret where it stands, before the allocator gets the memory
/// back and hands it to whatever asks next.
///
/// A real wipe rather than `clear()` or `= String::new()`. The difference is
/// not pedantry: at the settings this workspace ships with — full
/// optimisation, fat link-time optimisation — a plain assignment that nothing
/// afterwards reads is a write the compiler is entitled to delete, and it was
/// measured deleting it. `zeroize` writes volatilely and fences afterwards, so
/// the write survives and cannot be moved past the free that follows it.
///
/// The string is left empty afterwards, so the value is gone as well as
/// unreadable.
fn wipe(secret: &mut String) {
    use zeroize::Zeroize as _;
    secret.zeroize();
}

/// The value with its edges taken off, and the copy it came from wiped.
///
/// Trimming allocates a second string; this is the one place that happens, so
/// it is the one place that has to remember to clear the first. An empty
/// result is read as "not set", which is what it has always meant here.
fn trimmed(mut value: String) -> Option<String> {
    let out = value.trim().to_string();
    wipe(&mut value);
    (!out.is_empty()).then_some(out)
}

/// Put a secret in a field, clearing whatever the field held first.
///
/// Assigning over an `Option<String>` drops the old one, and dropping it is
/// not clearing it.
fn set_secret(field: &mut Option<String>, value: Option<String>) {
    if let Some(old) = field {
        wipe(old);
    }
    *field = value;
}

/// Where the active key came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Settings,
    File,
}

impl Source {
    pub fn describe(self) -> String {
        match self {
            Source::Env => "OPENROUTER_API_KEY in the environment".into(),
            Source::Settings => "model.api_key in config".into(),
            Source::File => crate::paths::credentials_file().display().to_string(),
        }
    }
}

/// Resolution order: env, settings, credentials file.
pub fn api_key(settings_key: Option<&str>) -> Option<String> {
    resolve(settings_key).map(|(k, _)| k)
}

pub fn resolve(settings_key: Option<&str>) -> Option<(String, Source)> {
    if let Some(k) = env_key() {
        return Some((k, Source::Env));
    }
    if let Some(k) = settings_key.map(str::trim).filter(|k| !k.is_empty()) {
        return Some((k.to_string(), Source::Settings));
    }
    stored_key().map(|k| (k, Source::File))
}

pub fn env_key() -> Option<String> {
    std::env::var("OPENROUTER_API_KEY").ok().and_then(trimmed)
}

pub fn stored_key() -> Option<String> {
    read_credentials()
        .and_then(|mut c| c.openrouter_api_key.take())
        .and_then(trimmed)
}

/// The Deepgram key, from the environment or the credentials file. It buys
/// one thing — dictation that answers while the sentence is still being said —
/// so nothing else looks for it.
pub fn deepgram_key() -> Option<String> {
    if let Some(k) = std::env::var("DEEPGRAM_API_KEY").ok().and_then(trimmed) {
        return Some(k);
    }
    read_credentials()
        .and_then(|mut c| c.deepgram_api_key.take())
        .and_then(trimmed)
}

/// Write the Deepgram key beside the other one, owner-readable only. An
/// empty key removes it.
pub fn save_deepgram_key(key: &str) -> Result<()> {
    let mut creds = read_credentials().unwrap_or_default();
    let key = key.trim();
    set_secret(
        &mut creds.deepgram_api_key,
        (!key.is_empty()).then(|| key.to_string()),
    );
    write_credentials(&creds)
}

/// Where the secrets are kept, for a command that wants to say so.
pub fn credentials_path() -> std::path::PathBuf {
    crate::paths::credentials_file()
}

/// The pairing code, from the environment or the credentials file.
///
/// Deliberately the same two places the Deepgram key comes from, and
/// deliberately not the settings: a relay named by a repository's own
/// `.ah/config.toml` would be somebody else's relay, and a pairing code found
/// there would be somebody else's code.
pub fn remote_code() -> Option<String> {
    from_env_or_file("AH_REMOTE_CODE", |c| c.remote_code.take())
}

/// The relay URL, from the same two places and for the same reason.
pub fn remote_url() -> Option<String> {
    from_env_or_file("AH_REMOTE_URL", |c| c.remote_url.take())
}

/// Store a pairing. Both halves are written together because neither is any
/// use without the other.
pub fn save_remote(code: &str, url: &str) -> Result<()> {
    let mut creds = read_credentials().unwrap_or_default();
    set_secret(&mut creds.remote_code, Some(code.trim().to_string()));
    creds.remote_url = Some(url.trim().to_string());
    write_credentials(&creds)
}

/// Forget the pairing. The relay keeps nothing readable, so this is the whole
/// of what revoking leaves behind on this machine.
pub fn clear_remote() -> Result<()> {
    let mut creds = read_credentials().unwrap_or_default();
    set_secret(&mut creds.remote_code, None);
    creds.remote_url = None;
    write_credentials(&creds)
}

/// Forget the code but remember where it was made.
///
/// What a pairing *is*, is the code. The relay is an address, and it is still
/// the address the next pairing will be made against — so `ah remote pair`,
/// which revokes what it replaces before it makes anything new, puts the code
/// down here rather than clearing the pair of them and leaving the next
/// command with no relay to reach for.
pub fn clear_remote_code() -> Result<()> {
    let mut creds = read_credentials().unwrap_or_default();
    set_secret(&mut creds.remote_code, None);
    write_credentials(&creds)
}

fn from_env_or_file(
    var: &str,
    pick: impl Fn(&mut Credentials) -> Option<String>,
) -> Option<String> {
    if let Some(v) = std::env::var(var).ok().and_then(trimmed) {
        return Some(v);
    }
    // Taken out of the credentials rather than read from them, so that the
    // struct is left holding nothing to wipe twice; what is not taken goes
    // when the struct does.
    let mut creds = read_credentials()?;
    pick(&mut creds).and_then(trimmed)
}

fn read_credentials() -> Option<Credentials> {
    read_credentials_at(&crate::paths::credentials_file())
}

fn read_credentials_at(path: &std::path::Path) -> Option<Credentials> {
    let mut text = std::fs::read_to_string(path).ok()?;
    let parsed = toml::from_str::<Credentials>(&text).ok();
    // The whole file, every key in it, as one string on the heap. It has been
    // parsed; there is nothing left to want it for.
    wipe(&mut text);
    parsed
}

/// `sk-or-v1-…c3d4`: enough to recognise a key, not enough to use it.
pub fn masked(key: &str) -> String {
    let n = key.chars().count();
    if n <= 12 {
        return "…".repeat(3);
    }
    let head: String = key.chars().take(8).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// Write the key to the credentials file, readable by the owner only.
pub fn save_key(key: &str) -> Result<()> {
    let mut creds = read_credentials().unwrap_or_default();
    set_secret(&mut creds.openrouter_api_key, Some(key.trim().to_string()));
    write_credentials(&creds)
}

/// Replace the credentials file. Every key in it is a secret, so the file is
/// the owner's alone and is rewritten whole rather than appended to.
fn write_credentials(creds: &Credentials) -> Result<()> {
    write_credentials_at(&crate::paths::credentials_file(), creds)
}

fn write_credentials_at(path: &std::path::Path, creds: &Credentials) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut text = toml::to_string(creds).map_err(|e| Error::Config(e.to_string()))?;
    // Written beside it and renamed over it, never into it. Truncating first
    // means a crash or a full disk between the truncate and the write leaves
    // an empty file where three secrets were, and there is no second copy of a
    // pairing code.
    let tmp = path.with_extension("toml.new");
    let written = write_whole(&tmp, &text);
    // Every secret in the file, spelled out in one string. Wiped here rather
    // than after the `?`, so that a write that failed does not leave it behind
    // on the way out.
    wipe(&mut text);
    written?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The bytes onto the disk and no further, so that the caller is left with one
/// place to clear the text from however the write went.
#[cfg(unix)]
fn write_whole(tmp: &std::path::Path, text: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(tmp)?;
    f.write_all(text.as_bytes())?;
    // The rename is only atomic once what it is renaming is on the disk.
    f.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_whole(tmp: &std::path::Path, text: &str) -> Result<()> {
    std::fs::write(tmp, text)?;
    Ok(())
}

/// What clearing the API key was not allowed to take with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kept {
    /// Whether a pairing code is still written down.
    pub pairing: bool,
    /// The relay that pairing was made against, when it is known: worth
    /// naming, because ending the pairing means reaching that relay and not
    /// another one.
    pub relay: Option<String>,
    /// Whether the dictation key is still here. Nothing to do with logging
    /// out of the model provider, and taken away by the same command until
    /// now.
    pub deepgram_key: bool,
}

impl Kept {
    /// Whether anything at all was kept, which is the same question as whether
    /// the credentials file is still there.
    pub fn anything(&self) -> bool {
        self.pairing || self.deepgram_key
    }
}

/// Take away the OpenRouter key, and nothing else.
///
/// This used to delete the credentials file whole, which took the pairing code
/// with it — and the pairing code is the only thing on this machine that can
/// revoke the pairing. A hub stays provisioned on the relay until it is told
/// otherwise; it goes on holding days of ciphertext for whoever has the code,
/// and goes on accepting them as this desktop. So logging out of a model
/// provider was quietly making a pairing permanent, by destroying the one
/// thing that could have ended it, and calling that "removed".
///
/// What is returned says what survived, so that whoever asked can be told. The
/// file itself is removed only when there was nothing in it but the key, which
/// keeps the ordinary case — one key, no phone, no dictation — exactly as it
/// was.
pub fn clear_key() -> Result<Kept> {
    clear_key_at(&crate::paths::credentials_file())
}

fn clear_key_at(path: &std::path::Path) -> Result<Kept> {
    let Some(mut creds) = read_credentials_at(path) else {
        // Nothing there, or nothing that parses. There is no pairing to be
        // careful of in either case, and a file that is not readable as
        // credentials is not one anything else will read either.
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        return Ok(Kept::default());
    };
    set_secret(&mut creds.openrouter_api_key, None);
    let kept = Kept {
        pairing: creds.remote_code.is_some(),
        relay: creds.remote_url.clone(),
        deepgram_key: creds.deepgram_api_key.is_some(),
    };
    if kept.anything() {
        write_credentials_at(path, &creds)?;
    } else if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(kept)
}

/// What OpenRouter reports for a key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyInfo {
    pub label: String,
    pub usage: f64,
    pub limit: Option<f64>,
    pub free_tier: bool,
}

/// Ask the API whether `key` is valid. `Error::Api { status: 401, .. }` means
/// it is not; other errors are network trouble.
pub fn verify(base_url: &str, key: &str) -> Result<KeyInfo> {
    let client = crate::provider::openrouter::OpenRouter::new(base_url, key);
    let v = client.get_json("/auth/key")?;
    let d = v.get("data").unwrap_or(&v);
    Ok(KeyInfo {
        label: d
            .get("label")
            .and_then(|l| l.as_str())
            .unwrap_or_default()
            .to_string(),
        usage: d.get("usage").and_then(|u| u.as_f64()).unwrap_or(0.0),
        limit: d.get("limit").and_then(|l| l.as_f64()),
        free_tier: d
            .get("is_free_tier")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking() {
        assert_eq!(masked("sk-or-v1-0123456789abcdef"), "sk-or-v1…cdef");
        assert_eq!(masked("short"), "………");
    }

    #[test]
    fn a_wiped_secret_is_gone_from_the_memory_it_was_in() {
        let mut secret = String::from("sk-or-v1-0123456789abcdef");
        let how_long = secret.len();
        wipe(&mut secret);
        assert!(secret.is_empty(), "and the value itself is gone too");
        // `wipe` empties the string but leaves it holding its allocation, so
        // what is written there now can be looked at by reaching back over it.
        let bytes = unsafe { secret.as_mut_vec() };
        unsafe { bytes.set_len(how_long) };
        assert!(bytes.iter().all(|b| *b == 0), "{bytes:?}");
        unsafe { bytes.set_len(0) };
    }

    #[test]
    fn a_field_given_a_new_secret_keeps_the_new_one_and_only_that() {
        let mut field = Some(String::from("the-old-pairing-code"));
        set_secret(&mut field, Some(String::from("the-new-pairing-code")));
        assert_eq!(field.as_deref(), Some("the-new-pairing-code"));
        set_secret(&mut field, None);
        assert_eq!(field, None);
    }

    /// A credentials file of this test's own. The path is passed in rather
    /// than found through the environment, so these run beside every other
    /// test in the process without any of them agreeing about a variable.
    fn a_file_of_its_own(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ah-auth-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("credentials.toml")
    }

    #[test]
    fn logging_out_of_the_model_provider_leaves_the_pairing_where_it_is() {
        let path = a_file_of_its_own("pairing");
        let _ = std::fs::remove_file(&path);
        write_credentials_at(
            &path,
            &Credentials {
                openrouter_api_key: Some("sk-or-v1-0123456789abcdef".into()),
                deepgram_api_key: None,
                remote_code: Some("ABCD-EFGH-IJKL-MNOP".into()),
                remote_url: Some("https://relay.example.com".into()),
            },
        )
        .unwrap();

        let kept = clear_key_at(&path).unwrap();
        assert!(
            kept.pairing,
            "the code is the only thing that can revoke it"
        );
        assert_eq!(kept.relay.as_deref(), Some("https://relay.example.com"));
        assert!(path.exists(), "the file went and took the pairing with it");

        let left = read_credentials_at(&path).unwrap();
        assert_eq!(left.openrouter_api_key, None, "that was the thing to clear");
        assert_eq!(left.remote_code.as_deref(), Some("ABCD-EFGH-IJKL-MNOP"));
        assert_eq!(
            left.remote_url.as_deref(),
            Some("https://relay.example.com")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn logging_out_with_nothing_else_in_the_file_takes_the_file_too() {
        let path = a_file_of_its_own("key-only");
        let _ = std::fs::remove_file(&path);
        write_credentials_at(
            &path,
            &Credentials {
                openrouter_api_key: Some("sk-or-v1-0123456789abcdef".into()),
                deepgram_api_key: None,
                remote_code: None,
                remote_url: None,
            },
        )
        .unwrap();

        let kept = clear_key_at(&path).unwrap();
        assert!(!kept.anything(), "{kept:?}");
        assert!(!path.exists(), "nothing was left in it to keep");
        // And doing it again, with no file at all, is not an error.
        assert_eq!(clear_key_at(&path).unwrap(), Kept::default());
    }

    #[test]
    fn the_dictation_key_is_not_the_model_providers_to_take() {
        let path = a_file_of_its_own("deepgram");
        let _ = std::fs::remove_file(&path);
        write_credentials_at(
            &path,
            &Credentials {
                openrouter_api_key: Some("sk-or-v1-0123456789abcdef".into()),
                deepgram_api_key: Some("dg-0123456789abcdef".into()),
                remote_code: None,
                remote_url: None,
            },
        )
        .unwrap();

        let kept = clear_key_at(&path).unwrap();
        assert!(kept.deepgram_key && !kept.pairing, "{kept:?}");
        let left = read_credentials_at(&path).unwrap();
        assert_eq!(left.openrouter_api_key, None);
        assert_eq!(
            left.deepgram_api_key.as_deref(),
            Some("dg-0123456789abcdef")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn printing_the_credentials_does_not_print_the_credentials() {
        let creds = Credentials {
            openrouter_api_key: Some("sk-or-v1-0123456789abcdef".into()),
            deepgram_api_key: None,
            remote_code: Some("ABCD-EFGH-IJKL-MNOP".into()),
            remote_url: Some("https://relay.example.com".into()),
        };
        let shown = format!("{creds:?}");
        assert!(!shown.contains("0123456789abcdef"), "{shown}");
        assert!(!shown.contains("ABCD"), "{shown}");
        // The relay is an address and is worth seeing.
        assert!(shown.contains("relay.example.com"), "{shown}");
    }
}

/// Account-level numbers OpenRouter exposes for a key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Account {
    pub label: String,
    /// Credits bought minus credits spent.
    pub balance: Option<f64>,
    pub total_credits: Option<f64>,
    pub total_usage: Option<f64>,
    pub usage_daily: Option<f64>,
    pub usage_weekly: Option<f64>,
    pub usage_monthly: Option<f64>,
    pub limit: Option<f64>,
    pub limit_remaining: Option<f64>,
    pub free_tier: bool,
}

/// `/auth/key` plus `/credits`; the second is optional (keys without
/// credit access still get the first half).
pub fn account(base_url: &str, key: &str) -> Result<Account> {
    let client = crate::provider::openrouter::OpenRouter::new(base_url, key);
    let v = client.get_json("/auth/key")?;
    let d = v.get("data").unwrap_or(&v);
    let num = |k: &str| d.get(k).and_then(|x| x.as_f64());
    let mut a = Account {
        label: d
            .get("label")
            .and_then(|l| l.as_str())
            .unwrap_or_default()
            .to_string(),
        usage_daily: num("usage_daily"),
        usage_weekly: num("usage_weekly"),
        usage_monthly: num("usage_monthly"),
        limit: num("limit"),
        limit_remaining: num("limit_remaining"),
        free_tier: d
            .get("is_free_tier")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        ..Account::default()
    };
    if let Ok(c) = client.get_json("/credits") {
        let d = c.get("data").unwrap_or(&c);
        a.total_credits = d.get("total_credits").and_then(|x| x.as_f64());
        a.total_usage = d.get("total_usage").and_then(|x| x.as_f64());
        if let (Some(t), Some(u)) = (a.total_credits, a.total_usage) {
            a.balance = Some(t - u);
        }
    }
    Ok(a)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelSpend {
    pub model: String,
    pub cost: f64,
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// Spend per model over the last 30 days from `/activity`, most expensive
/// first. OpenRouter only serves this to some keys, so callers should treat
/// an error as "not available".
pub fn activity(base_url: &str, key: &str) -> Result<Vec<ModelSpend>> {
    let client = crate::provider::openrouter::OpenRouter::new(base_url, key);
    let v = client.get_json("/activity")?;
    let rows = v
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let mut by: std::collections::BTreeMap<String, ModelSpend> = Default::default();
    for r in rows {
        let Some(model) = r.get("model").and_then(|m| m.as_str()) else {
            continue;
        };
        let num = |k: &str| r.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let e = by.entry(model.to_string()).or_insert_with(|| ModelSpend {
            model: model.to_string(),
            ..Default::default()
        });
        e.cost += num("usage");
        e.requests += num("requests") as u64;
        e.prompt_tokens += num("prompt_tokens") as u64;
        e.completion_tokens += num("completion_tokens") as u64;
    }
    let mut out: Vec<ModelSpend> = by.into_values().collect();
    out.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    Ok(out)
}
