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

pub struct Session {
    pub id: String,
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
            path,
            file: Some(file),
            messages: Vec::new(),
        })
    }

    pub fn open(id: &str) -> Result<Self> {
        let path = crate::paths::sessions_dir().join(format!("{id}.jsonl"));
        let reader = BufReader::new(File::open(&path)?);
        let mut messages = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if i == 0 || line.trim().is_empty() {
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
