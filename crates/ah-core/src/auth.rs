//! API key discovery and storage.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(default)]
    openrouter_api_key: Option<String>,
    /// Dictation against Deepgram's live socket. Nothing else uses it.
    #[serde(default)]
    deepgram_api_key: Option<String>,
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
    std::env::var("OPENROUTER_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

pub fn stored_key() -> Option<String> {
    read_credentials()
        .and_then(|c| c.openrouter_api_key)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// The Deepgram key, from the environment or the credentials file. It buys
/// one thing — dictation that answers while the sentence is still being said —
/// so nothing else looks for it.
pub fn deepgram_key() -> Option<String> {
    if let Some(k) = std::env::var("DEEPGRAM_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
    {
        return Some(k);
    }
    read_credentials()
        .and_then(|c| c.deepgram_api_key)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// Write the Deepgram key beside the other one, owner-readable only. An
/// empty key removes it.
pub fn save_deepgram_key(key: &str) -> Result<()> {
    let mut creds = read_credentials().unwrap_or_default();
    let key = key.trim();
    creds.deepgram_api_key = (!key.is_empty()).then(|| key.to_string());
    write_credentials(&creds)
}

fn read_credentials() -> Option<Credentials> {
    std::fs::read_to_string(crate::paths::credentials_file())
        .ok()
        .and_then(|t| toml::from_str::<Credentials>(&t).ok())
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
    creds.openrouter_api_key = Some(key.trim().to_string());
    write_credentials(&creds)
}

/// Replace the credentials file. Every key in it is a secret, so the file is
/// the owner's alone and is rewritten whole rather than appended to.
fn write_credentials(creds: &Credentials) -> Result<()> {
    let path = crate::paths::credentials_file();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
        }
    }
    let text = toml::to_string(creds).map_err(|e| Error::Config(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(text.as_bytes())?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    std::fs::write(&path, text)?;
    Ok(())
}

pub fn clear_key() -> Result<()> {
    let path = crate::paths::credentials_file();
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
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
