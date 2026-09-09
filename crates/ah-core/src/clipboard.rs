//! Read an image off the system clipboard through whatever tool is installed:
//! `wl-paste` (Wayland), `xclip` (X11), `pngpaste` (macOS). A configured
//! command overrides detection and must write PNG bytes to stdout.

use std::process::{Command, Stdio};

#[derive(Debug, PartialEq)]
pub enum ClipError {
    /// Nothing usable is installed.
    NoTool,
    /// The clipboard holds no image.
    Empty,
    Failed(String),
}

impl std::fmt::Display for ClipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClipError::NoTool => write!(
                f,
                "no clipboard tool found (install wl-clipboard, xclip or pngpaste, or set layout.image_paste_cmd)"
            ),
            ClipError::Empty => write!(f, "clipboard has no image"),
            ClipError::Failed(e) => write!(f, "clipboard read failed: {e}"),
        }
    }
}

fn run(program: &str, args: &[&str]) -> Result<Vec<u8>, ClipError> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ClipError::NoTool
            } else {
                ClipError::Failed(e.to_string())
            }
        })?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(ClipError::Failed(if msg.is_empty() {
            format!("{program} exited with {}", out.status)
        } else {
            msg
        }));
    }
    Ok(out.stdout)
}

fn has(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

pub(crate) fn looks_like_png(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// PNG bytes from the clipboard. `custom` is a shell command that prints
/// them; empty means detect a tool.
pub fn image(custom: &str) -> Result<Vec<u8>, ClipError> {
    if !custom.trim().is_empty() {
        let bytes = run("sh", &["-c", custom])?;
        return if bytes.is_empty() {
            Err(ClipError::Empty)
        } else {
            Ok(bytes)
        };
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && has("wl-paste") {
        let types = run("wl-paste", &["-l"]).unwrap_or_default();
        if !String::from_utf8_lossy(&types).contains("image/") {
            return Err(ClipError::Empty);
        }
        return run("wl-paste", &["-t", "image/png", "-n"]).and_then(|b| {
            if b.is_empty() {
                Err(ClipError::Empty)
            } else {
                Ok(b)
            }
        });
    }
    if std::env::var_os("DISPLAY").is_some() && has("xclip") {
        let targets =
            run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]).unwrap_or_default();
        if !String::from_utf8_lossy(&targets).contains("image/png") {
            return Err(ClipError::Empty);
        }
        return run(
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        );
    }
    if cfg!(target_os = "macos") && has("pngpaste") {
        return match run("pngpaste", &["-"]) {
            Ok(b) if looks_like_png(&b) => Ok(b),
            Ok(_) => Err(ClipError::Empty),
            Err(ClipError::Failed(_)) => Err(ClipError::Empty),
            Err(e) => Err(e),
        };
    }
    Err(ClipError::NoTool)
}

/// `data:image/png;base64,…` for `bytes`.
pub fn data_url(bytes: &[u8]) -> String {
    let mime = if looks_like_png(bytes) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/png"
    };
    format!("data:{mime};base64,{}", base64(bytes))
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        s.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_data_url() {
        assert_eq!(base64(b"hi"), "aGk=");
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert!(data_url(b"\x89PNG\r\n\x1a\nxx").starts_with("data:image/png;base64,"));
        assert!(data_url(&[0xFF, 0xD8, 0xFF]).starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn custom_command_paths() {
        assert_eq!(image("printf ''"), Err(ClipError::Empty));
        assert_eq!(image("printf 'abc'"), Ok(b"abc".to_vec()));
        assert!(matches!(image("exit 3"), Err(ClipError::Failed(_))));
    }
}
