use std::path::PathBuf;

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `$AH_CONFIG_DIR` or `~/.config/ah`.
pub fn config_dir() -> PathBuf {
    env_path("AH_CONFIG_DIR")
        .or_else(|| dirs::config_dir().map(|d| d.join("ah")))
        .unwrap_or_else(|| PathBuf::from(".ah"))
}

/// `$AH_DATA_DIR` or `~/.local/share/ah`.
pub fn data_dir() -> PathBuf {
    env_path("AH_DATA_DIR")
        .or_else(|| dirs::data_dir().map(|d| d.join("ah")))
        .unwrap_or_else(|| PathBuf::from(".ah/data"))
}

pub fn project_dir() -> PathBuf {
    PathBuf::from(".ah")
}

pub fn user_config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn project_config_file() -> PathBuf {
    project_dir().join("config.toml")
}

/// Stars added from the TUI (`/star`). Config files can also define `model.starred`.
pub fn starred_file() -> PathBuf {
    config_dir().join("starred.toml")
}

pub fn credentials_file() -> PathBuf {
    config_dir().join("credentials.toml")
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

pub fn plugin_state_dir() -> PathBuf {
    data_dir().join("plugins")
}

/// Directories scanned for `*.wasm` plugins, in load order.
pub fn plugin_dirs() -> Vec<PathBuf> {
    vec![config_dir().join("plugins"), project_dir().join("plugins")]
}
