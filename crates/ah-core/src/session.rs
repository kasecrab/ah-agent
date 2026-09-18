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
/// metadata that changes after the header was written. `_clear` and
/// `_compact` drop every message logged before them.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Marker {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    _name: Option<String>,
    /// The model the conversation was switched to. The last one in the file
    /// wins, so resuming picks up where the conversation was left rather than
    /// wherever the config files stand today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    _model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    _clear: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    _compact: Option<bool>,
    /// Snapshot of the task list; the last one in the file wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    _plan: Option<crate::plan::Plan>,
}

impl Marker {
    fn resets(&self) -> bool {
        self._clear == Some(true) || self._compact == Some(true)
    }
}

fn marker(line: &str) -> Option<Marker> {
    if !(line.contains("\"_name\"")
        || line.contains("\"_model\"")
        || line.contains("\"_clear\"")
        || line.contains("\"_compact\"")
        || line.contains("\"_plan\""))
    {
        return None;
    }
    serde_json::from_str::<Marker>(line).ok().filter(|m| {
        m._name.is_some()
            || m._model.is_some()
            || m._clear.is_some()
            || m._compact.is_some()
            || m._plan.is_some()
    })
}

pub struct Session {
    pub id: String,
    pub name: Option<String>,
    /// The model this conversation is being held with. Empty only for an
    /// ephemeral session, which has no file to remember one in.
    pub model: String,
    path: PathBuf,
    file: Option<File>,
    pub messages: Vec<Message>,
    /// Task list as it stood when the session was last written.
    pub plan: crate::plan::Plan,
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
            model: String::new(),
            path: PathBuf::new(),
            file: None,
            messages: Vec::new(),
            plan: crate::plan::Plan::default(),
        }
    }

    pub fn create(cwd: &str, model: &str) -> Result<Self> {
        let dir = crate::paths::sessions_dir();
        std::fs::create_dir_all(&dir)?;
        let id = new_id();
        let path = dir.join(format!("{id}.jsonl"));
        // A transcript holds every tool result the model was shown, which is
        // as much of this machine as the session touched. The credentials file
        // beside it is 0600 and this is the same material by a longer route.
        let mut file =
            crate::paths::owner_only(OpenOptions::new().create(true).append(true)).open(&path)?;
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
            model: model.into(),
            path,
            file: Some(file),
            messages: Vec::new(),
            plan: crate::plan::Plan::default(),
        })
    }

    pub fn open(id: &str) -> Result<Self> {
        let path = crate::paths::sessions_dir().join(format!("{id}.jsonl"));
        let reader = BufReader::new(File::open(&path)?);
        let mut messages = Vec::new();
        let mut name = None;
        let mut model = String::new();
        let mut plan = crate::plan::Plan::default();
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            if i == 0 {
                // The model the conversation started on. A `_model` marker
                // further down replaces it; a header ah cannot read leaves it
                // empty, which the caller reads as "no model of its own".
                if let Ok(h) = serde_json::from_str::<Header>(&line) {
                    model = h.model;
                }
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            if let Some(m) = marker(&line) {
                if m.resets() {
                    messages.clear();
                }
                if m._clear == Some(true) {
                    plan = crate::plan::Plan::default();
                }
                if let Some(n) = m._name {
                    name = Some(n).filter(|n| !n.is_empty());
                }
                // Not reset by `_clear` or `_compact`: those throw away what
                // was said, not who it was being said to.
                if let Some(m) = m._model.filter(|m| !m.is_empty()) {
                    model = m;
                }
                if let Some(p) = m._plan {
                    plan = p;
                }
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
            model,
            path,
            file: Some(file),
            messages,
            plan,
        })
    }

    /// Most recent session id, if any.
    pub fn latest() -> Option<String> {
        let mut ids: Vec<String> = list().into_iter().map(|s| s.id).collect();
        ids.sort();
        ids.pop()
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Where this session's generated images go. An ephemeral session has one
    /// too: a one-shot run still writes the picture it was asked for, it is
    /// just not referenced by any session file afterwards.
    pub fn images_dir(&self) -> PathBuf {
        crate::paths::session_images_dir(&self.id)
    }

    /// Give the session a name (empty clears it). The last marker wins on
    /// reload, so renaming is an append like everything else.
    pub fn rename(&mut self, name: &str) {
        let name = name.trim();
        self.write_marker(&Marker {
            _name: Some(name.to_string()),
            ..Default::default()
        });
        self.name = (!name.is_empty()).then(|| name.to_string());
    }

    /// Record that the conversation is now being held with `model`.
    ///
    /// Appended like everything else, and the last one wins on reload, so a
    /// session resumes on the model it was last switched to rather than on
    /// whatever the config files happen to say by then.
    pub fn set_model(&mut self, model: &str) {
        if model.is_empty() || model == self.model {
            return;
        }
        self.model = model.to_string();
        self.write_marker(&Marker {
            _model: Some(model.to_string()),
            ..Default::default()
        });
    }

    pub fn push(&mut self, mut m: Message) {
        // Recorded speech is a side channel to the transcriber. It has no
        // business in the conversation, and none at all in a file on disk.
        m.audio.clear();
        if let Some(f) = self.file.as_mut()
            && let Ok(s) = serde_json::to_string(&m)
        {
            let _ = writeln!(f, "{s}");
        }
        self.messages.push(m);
    }

    fn write_marker(&mut self, m: &Marker) {
        if let Some(f) = self.file.as_mut()
            && let Ok(s) = serde_json::to_string(m)
        {
            let _ = writeln!(f, "{s}");
        }
    }

    /// Record the task list, so resuming the session resumes the plan.
    pub fn save_plan(&mut self, plan: &crate::plan::Plan) {
        if *plan == self.plan {
            return;
        }
        self.plan = plan.clone();
        self.write_marker(&Marker {
            _plan: Some(plan.clone()),
            ..Default::default()
        });
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.plan = crate::plan::Plan::default();
        self.write_marker(&Marker {
            _clear: Some(true),
            ..Default::default()
        });
    }

    /// Replace the conversation with `messages` (after compaction). Logged as
    /// a marker followed by the new messages, so the file stays append-only.
    pub fn reset(&mut self, messages: Vec<Message>) {
        self.messages.clear();
        self.write_marker(&Marker {
            _compact: Some(true),
            ..Default::default()
        });
        for m in messages {
            self.push(m);
        }
    }
}

/// What `/resume` shows for one stored session.
#[derive(Debug, Clone)]
pub struct Summary {
    pub id: String,
    pub name: Option<String>,
    pub started_ms: u128,
    /// When the session file was last written: the age worth showing, since a
    /// two-day-old session spoken to a minute ago is a minute old to anybody
    /// looking for it. Never zero and never before `started_ms`, so a caller
    /// can use it without a fallback of its own.
    pub touched_ms: u128,
    pub cwd: String,
    /// The model the session will resume on: the last one it was switched to,
    /// or the one it started with.
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
        .filter_map(|stored| {
            let id = &stored.id;
            let f = File::open(dir.join(format!("{id}.jsonl"))).ok()?;
            let mut lines = BufReader::new(f).lines();
            let header: Header = serde_json::from_str(&lines.next()?.ok()?).ok()?;
            let mut title = String::new();
            let mut name = None;
            let mut model = header.model;
            let mut messages = 0usize;
            for line in lines.map_while(|l| l.ok()) {
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(m) = marker(&line) {
                    if m.resets() {
                        messages = 0;
                    }
                    if let Some(n) = m._name {
                        name = Some(n).filter(|n| !n.is_empty());
                    }
                    if let Some(m) = m._model.filter(|m| !m.is_empty()) {
                        model = m;
                    }
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
                // A clock that went backwards, or a file whose mtime the
                // filesystem would not give up, would otherwise read as older
                // than the session it belongs to.
                touched_ms: stored.touched_ms.max(header.started_ms),
                cwd: header.cwd,
                model,
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
    if list().iter().any(|s| s.id == what) {
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

/// One stored session as the directory describes it, without opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub id: String,
    /// What the file weighs.
    pub bytes: u64,
    /// When it was last written, in milliseconds since the epoch. A session
    /// file is appended to as the conversation happens, so this is the last
    /// thing anybody said in it. Zero where the filesystem will not say, which
    /// is a thing to fall back from rather than a date.
    pub touched_ms: u128,
}

/// Every stored session, by id. One `stat` apiece and nothing opened: what is
/// inside a session file is read by whoever actually wants it.
pub fn list() -> Vec<Stored> {
    let Ok(rd) = std::fs::read_dir(crate::paths::sessions_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<Stored> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(".jsonl")?.to_string();
            let meta = e.metadata().ok()?;
            Some(Stored {
                id,
                bytes: meta.len(),
                touched_ms: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis()),
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
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
        assert!(
            marker(r#"{"role":"system","content":"","_clear":true}"#).is_some_and(|m| m.resets())
        );
        assert!(marker(r#"{"_compact":true}"#).is_some_and(|m| m.resets()));
    }

    /// Writes a session file by hand so its header can say a time the file
    /// itself does not have. Returns the id.
    fn stored(dir: &std::path::Path, id: &str, started_ms: u128) -> String {
        let header =
            format!(r#"{{"id":"{id}","started_ms":{started_ms},"cwd":"/tmp","model":"m"}}"#);
        std::fs::write(
            dir.join("sessions").join(format!("{id}.jsonl")),
            format!("{header}\n{{\"role\":\"user\",\"content\":\"hi\"}}\n"),
        )
        .unwrap();
        id.to_string()
    }

    /// The age worth showing comes from the file, not from the header: a
    /// session started days ago and spoken to a minute ago is a minute old.
    #[test]
    fn a_summary_is_as_recent_as_the_last_thing_written_to_it() {
        let _env = crate::test_env::guard();
        let dir = std::env::temp_dir().join(format!("ah-touched-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        // SAFETY: every test that moves this variable holds the one guard.
        let old = std::env::var_os("AH_DATA_DIR");
        unsafe { std::env::set_var("AH_DATA_DIR", &dir) };

        let ancient = stored(&dir, "aaa-old", 1_000);
        let ahead = stored(&dir, "bbb-ahead", now_ms() + 600_000);
        let found = summaries();

        let old_one = found.iter().find(|s| s.id == ancient).expect("listed");
        assert!(
            old_one.touched_ms > old_one.started_ms,
            "a file written now is newer than a session started in 1970"
        );
        assert!(
            now_ms().saturating_sub(old_one.touched_ms) < 60_000,
            "the file was written moments ago, so that is what it reports"
        );

        // A clock that ran backwards, or a header from a machine whose clock
        // is ahead, must not read as older than the session it belongs to.
        let ahead_one = found.iter().find(|s| s.id == ahead).expect("listed");
        assert_eq!(
            ahead_one.touched_ms, ahead_one.started_ms,
            "never older than the start it came with"
        );

        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: as above, under the same guard.
        unsafe {
            match old {
                Some(v) => std::env::set_var("AH_DATA_DIR", v),
                None => std::env::remove_var("AH_DATA_DIR"),
            }
        }
    }

    /// Run `f` with a data directory of its own, so the session files a test
    /// writes are the only ones it can see.
    fn in_data_dir(tag: &str, f: impl FnOnce(&std::path::Path)) {
        let _env = crate::test_env::guard();
        let dir = std::env::temp_dir().join(format!("ah-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        // SAFETY: every test that moves this variable holds the one guard.
        let old = std::env::var_os("AH_DATA_DIR");
        unsafe { std::env::set_var("AH_DATA_DIR", &dir) };
        f(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: as above, under the same guard.
        unsafe {
            match old {
                Some(v) => std::env::set_var("AH_DATA_DIR", v),
                None => std::env::remove_var("AH_DATA_DIR"),
            }
        }
    }

    /// The whole point: a conversation switched to another model is still on
    /// that model tomorrow, whatever the config files have come to say. The
    /// header is only where it started, so the last switch is what counts.
    #[test]
    fn a_session_opens_on_the_model_it_was_last_switched_to() {
        in_data_dir("model-marker", |_| {
            let mut s = Session::create("/tmp", "vendor/flash").unwrap();
            let id = s.id.clone();
            s.push(Message::user("hello"));
            s.set_model("vendor/mid");
            s.push(Message::user("something harder"));
            s.set_model("vendor/pro");
            drop(s);

            let back = Session::open(&id).unwrap();
            assert_eq!(back.model, "vendor/pro", "the last switch, not the first");
            assert_eq!(back.messages.len(), 2, "the messages are still there");
            assert_eq!(
                summaries().iter().find(|s| s.id == id).unwrap().model,
                "vendor/pro",
                "and the listing says the same as the resume will do"
            );
        });
    }

    /// A session nobody ever switched resumes on the model in its header, and
    /// one written by an older ah — no marker anywhere — does too.
    #[test]
    fn a_session_never_switched_opens_on_the_model_it_started_with() {
        in_data_dir("model-header", |dir| {
            let s = Session::create("/tmp", "vendor/flash").unwrap();
            let id = s.id.clone();
            drop(s);
            let old = stored(dir, "older-ah", 1_000);

            assert_eq!(Session::open(&id).unwrap().model, "vendor/flash");
            assert_eq!(Session::open(&old).unwrap().model, "m");
        });
    }

    /// `/clear` and compaction throw away what was said, not who it was being
    /// said to: the conversation is still being held with the same model.
    #[test]
    fn clearing_and_compacting_leave_the_model_alone() {
        in_data_dir("model-clear", |_| {
            let mut s = Session::create("/tmp", "vendor/flash").unwrap();
            let id = s.id.clone();
            s.set_model("vendor/pro");
            s.push(Message::user("one"));
            s.clear();
            s.push(Message::user("two"));
            s.reset(vec![Message::user("a summary")]);
            drop(s);

            let back = Session::open(&id).unwrap();
            assert_eq!(back.model, "vendor/pro");
            assert_eq!(back.messages.len(), 1);
        });
    }

    /// Switching to the model already in use writes nothing: a session whose
    /// model never moves must not grow a marker per turn.
    #[test]
    fn switching_to_the_same_model_writes_nothing() {
        in_data_dir("model-same", |_| {
            let mut s = Session::create("/tmp", "vendor/flash").unwrap();
            let path = s.path().clone();
            let before = std::fs::metadata(&path).unwrap().len();
            s.set_model("vendor/flash");
            s.set_model("");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
        });
    }

    /// `list` is a directory read and nothing more, so it has to carry what
    /// the directory knows rather than making a caller open the file again.
    #[test]
    fn a_stored_session_carries_its_size_and_its_last_write() {
        let _env = crate::test_env::guard();
        let dir = std::env::temp_dir().join(format!("ah-stored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        // SAFETY: every test that moves this variable holds the one guard.
        let old = std::env::var_os("AH_DATA_DIR");
        unsafe { std::env::set_var("AH_DATA_DIR", &dir) };

        stored(&dir, "ccc-one", 1_000);
        let rows = list();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].bytes > 0, "the file has something in it");
        assert!(
            now_ms().saturating_sub(rows[0].touched_ms) < 60_000,
            "written moments ago"
        );

        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: as above, under the same guard.
        unsafe {
            match old {
                Some(v) => std::env::set_var("AH_DATA_DIR", v),
                None => std::env::remove_var("AH_DATA_DIR"),
            }
        }
    }
}
