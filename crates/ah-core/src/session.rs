//! Append-only JSONL session log. One line per message, plus a header line.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use ah_abi::Message;
use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    id: String,
    started_ms: u128,
    cwd: String,
    model: String,
}

/// Marker lines sit between messages in the log and carry session
/// metadata that changes after the header was written.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    _name: Option<String>,
}

fn marker(line: &str) -> Option<Marker> {
    line.contains("\"_name\"")
        .then(|| serde_json::from_str::<Marker>(line).ok())
        .flatten()
}

pub struct Session {
    pub id: String,
    pub name: Option<String>,
    path: PathBuf,
    file: Option<File>,
    pub messages: Vec<Message>,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn new_id() -> String {
    let t = now_ms();
    let pid = std::process::id() as u128;
    let mixed = (t << 16) ^ (pid.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    format!("{:x}", mixed & 0xFFFF_FFFF_FFFF_FFFF)
}

impl Session {
    /// In-memory only (used by one-shot CLI mode and tests).
    pub fn ephemeral() -> Self {
        Self {
            id: new_id(),
            name: None,
            path: PathBuf::new(),
            file: None,
            messages: Vec::new(),
        }
    }

    pub fn create(cwd: &str, model: &str) -> Result<Self> {
        let dir = crate::paths::sessions_dir();
        std::fs::create_dir_all(&dir)?;
        let id = new_id();
        let path = dir.join(format!("{id}.jsonl"));
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let header = Header {
            id: id.clone(),
            started_ms: now_ms(),
            cwd: cwd.into(),
            model: model.into(),
        };
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        Ok(Self {
            id,
            name: None,
            path,
            file: Some(file),
            messages: Vec::new(),
        })
    }

    pub fn open(id: &str) -> Result<Self> {
        let path = crate::paths::sessions_dir().join(format!("{id}.jsonl"));
        let reader = BufReader::new(File::open(&path)?);
        let mut messages = Vec::new();
        let mut name = None;
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if i == 0 || line.trim().is_empty() {
                continue;
            }
            if let Some(m) = marker(&line) {
                name = m._name.filter(|n| !n.is_empty());
                continue;
            }
            match serde_json::from_str::<Message>(&line) {
                Ok(m) => messages.push(m),
                Err(e) => crate::warn!("session {id} line {}: {e}", i + 1),
            }
        }
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            id: id.into(),
            name,
            path,
            file: Some(file),
            messages,
        })
    }

    /// Most recent session id, if any.
    pub fn latest() -> Option<String> {
        let mut ids: Vec<String> = list().into_iter().map(|(id, _)| id).collect();
        ids.sort();
        ids.pop()
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Give the session a name (empty clears it). The last marker wins on
    /// reload, so renaming is an append like everything else.
    pub fn rename(&mut self, name: &str) {
        let name = name.trim();
        if let Some(f) = self.file.as_mut() {
            let m = Marker {
                _name: Some(name.to_string()),
            };
            if let Ok(s) = serde_json::to_string(&m) {
                let _ = writeln!(f, "{s}");
            }
        }
        self.name = (!name.is_empty()).then(|| name.to_string());
    }

    pub fn push(&mut self, m: Message) {
        if let Some(f) = self.file.as_mut()
            && let Ok(s) = serde_json::to_string(&m)
        {
            let _ = writeln!(f, "{s}");
        }
        self.messages.push(m);
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(
                f,
                "{}",
                serde_json::json!({"role": "system", "content": "", "_clear": true})
            );
        }
    }
}

/// What `/resume` shows for one stored session.
#[derive(Debug, Clone)]
pub struct Summary {
    pub id: String,
    pub name: Option<String>,
    pub started_ms: u128,
    pub cwd: String,
    pub model: String,
    /// First user message, single line, trimmed.
    pub title: String,
    pub messages: usize,
}

/// Summaries of stored sessions, newest first. Empty sessions are skipped.
pub fn summaries() -> Vec<Summary> {
    let dir = crate::paths::sessions_dir();
    let mut out: Vec<Summary> = list()
        .into_iter()
        .filter_map(|(id, _)| {
            let f = File::open(dir.join(format!("{id}.jsonl"))).ok()?;
            let mut lines = BufReader::new(f).lines();
            let header: Header = serde_json::from_str(&lines.next()?.ok()?).ok()?;
            let mut title = String::new();
            let mut name = None;
            let mut messages = 0usize;
            for line in lines.map_while(|l| l.ok()) {
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(m) = marker(&line) {
                    name = m._name.filter(|n| !n.is_empty());
                    continue;
                }
                messages += 1;
                if title.is_empty()
                    && let Ok(m) = serde_json::from_str::<Message>(&line)
                    && m.role == ah_abi::Role::User
                {
                    title = m.content.lines().next().unwrap_or("").trim().to_string();
                }
            }
            (messages > 0 || name.is_some()).then_some(Summary {
                id: header.id,
                name,
                started_ms: header.started_ms,
                cwd: header.cwd,
                model: header.model,
                title,
                messages,
            })
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.started_ms));
    out
}

/// Resolve what the user typed to a session id: an exact id first, then the
/// newest session with that name (case-insensitive).
pub fn find(what: &str) -> Option<String> {
    let what = what.trim();
    if what.is_empty() {
        return None;
    }
    if list().iter().any(|(id, _)| id == what) {
        return Some(what.to_string());
    }
    summaries()
        .into_iter()
        .find(|s| {
            s.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(what))
        })
        .map(|s| s.id)
}

/// `(id, size_bytes)` for every stored session.
pub fn list() -> Vec<(String, u64)> {
    let Ok(rd) = std::fs::read_dir(crate::paths::sessions_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<(String, u64)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".jsonl")?.to_string();
            let size = e.metadata().ok()?.len();
            Some((id, size))
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_lines_are_recognised() {
        assert!(marker(r#"{"_name":"work"}"#).is_some());
        assert!(marker(r#"{"role":"user","content":"_name"}"#).is_none());
        assert!(marker(r#"{"role":"user","content":"\"_name\""}"#).is_none());
    }
}
