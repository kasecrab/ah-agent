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

/// Favorites added from the TUI (`/favorite`). Config files can also define `model.favorites`.
pub fn favorites_file() -> PathBuf {
    config_dir().join("favorites.toml")
}

/// Choices `ah` was told to remember, as a settings patch. Written by the
/// program, not by hand: keeping them out of `config.toml` means a file the
/// user wrote, comments and all, is never rewritten under them.
pub fn state_file() -> PathBuf {
    config_dir().join("state.toml")
}

pub fn credentials_file() -> PathBuf {
    config_dir().join("credentials.toml")
}

/// Prompt history shared by every session, one JSON string per line.
pub fn history_file() -> PathBuf {
    data_dir().join("history")
}

pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// Root of the generated-image store. Images live under their own tree
/// rather than beside the session files so that listing sessions stays a scan
/// of one flat directory of `*.jsonl`.
pub fn images_dir() -> PathBuf {
    data_dir().join("images")
}

/// Where one session's generated images go. The id is reduced to characters
/// that cannot escape the store, since it is read back out of a file.
pub fn session_images_dir(id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    images_dir().join(if safe.is_empty() {
        "unknown".to_string()
    } else {
        safe
    })
}

pub fn plugin_state_dir() -> PathBuf {
    data_dir().join("plugins")
}

/// Directories scanned for `*.wasm` plugins, in load order.
pub fn plugin_dirs() -> Vec<PathBuf> {
    vec![config_dir().join("plugins"), project_plugin_dir()]
}

/// Where a repository keeps plugins of its own. Read only when the user's own
/// config says to: a `.wasm` in a directory somebody cloned is a program
/// somebody else wrote, and it is loaded before the first turn.
pub fn project_plugin_dir() -> PathBuf {
    project_dir().join("plugins")
}

/// Ask for a file only this user can read.
///
/// Used for everything that holds what the model was shown or what was typed
/// at it — transcripts, prompt history, the debug log. The credentials file is
/// 0600 already; a transcript is the same material by a longer route, since a
/// tool result can be the contents of any file the session read.
///
/// A no-op off unix, where the permission model is not this one.
pub fn owner_only(opts: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts
}
