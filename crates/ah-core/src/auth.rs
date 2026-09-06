//! API key discovery and storage.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(default)]
    openrouter_api_key: Option<String>,
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
    std::fs::read_to_string(crate::paths::credentials_file())
        .ok()
        .and_then(|t| toml::from_str::<Credentials>(&t).ok())
        .and_then(|c| c.openrouter_api_key)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
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
    let path = crate::paths::credentials_file();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
        }
    }
    let creds = Credentials {
        openrouter_api_key: Some(key.trim().to_string()),
    };
    let text = toml::to_string(&creds).map_err(|e| Error::Config(e.to_string()))?;
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
