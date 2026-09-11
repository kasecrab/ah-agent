//! `ah remote serve`: the machine, with nobody at the keyboard.
//!
//! A window publishes the session it already has. This publishes whatever a
//! phone asks for — a session on disk brought back, or a new one started
//! somewhere the settings allow — and keeps running when no window is open.
//!
//! Each session gets an engine of its own, and each engine gets a settings
//! stack built from an explicit path rather than from wherever this process
//! happens to be standing. Nothing here changes the process's own directory:
//! two sessions in two repositories would otherwise be one session's tools
//! running in the other's tree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ah_core::abi::{Reply, Settings};
use ah_core::settings::{Origin, SettingsStack};
use ah_remote::proto::{SessionInfo, SessionState};

use crate::Overrides;
use crate::app::AnyError;
use crate::app::{Engine, EngineCmd, Inbox, UiEvent};
use crate::remote::publisher::{self, Note, Publisher};
use crate::remote::sessions::{Act, Sessions};

/// How long the daemon waits before trying for the pairing again after a
/// window took it.
const RETRY: Duration = Duration::from_secs(2);

/// One session, and the ways into it.
struct Running {
    id: String,
    cwd: PathBuf,
    model: String,
    name: Option<String>,
    busy: bool,
    cmd: Sender<EngineCmd>,
    perm: Sender<bool>,
    ask: Sender<Reply>,
    cancel: Arc<AtomicBool>,
    inbox: Arc<Inbox>,
}

/// Everything this machine is offering.
pub struct Machine {
    running: Mutex<HashMap<String, Running>>,
    /// Where a phone may start a session. Read once, from somewhere a
    /// repository cannot reach.
    roots: Vec<PathBuf>,
    trusted: bool,
    max_sessions: usize,
    /// Events from every engine, tagged with the session they came from.
    events: Sender<(String, UiEvent)>,
}

impl Machine {
    /// Start an engine for a session, or bring one back that is on disk.
    ///
    /// The settings stack is built here rather than taken from the process:
    /// `.ah/config.toml` belongs to the directory the session runs in, and
    /// this process may be standing somewhere else entirely.
    fn open(
        &self,
        cwd: &Path,
        resume: Option<&str>,
        model: Option<String>,
    ) -> Result<String, String> {
        if self.running.lock().map(|r| r.len()).unwrap_or(0) >= self.max_sessions {
            return Err("this machine is already running as many sessions as it will".into());
        }
        let mut stack = SettingsStack::from_files().map_err(|e| e.to_string())?;
        let project = cwd.join(".ah").join("config.toml");
        if project.exists() {
            stack.push_file(&project).map_err(|e| e.to_string())?;
        }
        if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
            stack
                .push(
                    Origin::Runtime("remote".into()),
                    serde_json::json!({"model": {"id": model}}),
                )
                .map_err(|e| e.to_string())?;
        }
        // Nobody is at this keyboard. A session nobody is watching runs tools
        // without being asked unless it is told otherwise, and "unless it is
        // told otherwise" is the whole of the protection a phone has.
        if !self.trusted {
            stack
                .push(
                    Origin::Runtime("remote".into()),
                    serde_json::json!({"permissions": {"mode": "ask"}}),
                )
                .map_err(|e| e.to_string())?;
        }

        let mut engine =
            Engine::new(&stack, cwd.to_path_buf(), resume, true).map_err(|e| e.to_string())?;
        let id = engine.session.id.clone();
        let name = engine.session.name.clone();
        let cancel = engine.cancel.clone();
        let inbox = engine.inbox.clone();
        let model = stack.settings().model.id.clone();

        let (cmd, cmd_rx) = mpsc::channel();
        let (perm, perm_rx) = mpsc::channel();
        let (ask, ask_rx) = mpsc::channel();
        let (ui, ui_rx) = mpsc::channel();

        let (_reports, patches, _) = engine.load_plugins();
        for (plugin, patch) in patches {
            let _ = stack.push(Origin::Plugin(plugin), patch);
        }
        engine.apply_settings(stack.settings().clone(), stack.value().clone());

        std::thread::Builder::new()
            .name(format!("ah-session-{}", &id[..8.min(id.len())]))
            .spawn(move || engine.serve(cmd_rx, ui, perm_rx, ask_rx))
            .map_err(|e| e.to_string())?;

        // One thread per session carrying its events out, tagged so the
        // publisher can tell them apart.
        let events = self.events.clone();
        let tagged = id.clone();
        std::thread::Builder::new()
            .name("ah-session-fwd".into())
            .spawn(move || {
                while let Ok(ev) = ui_rx.recv() {
                    if events.send((tagged.clone(), ev)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        lock(&self.running).insert(
            id.clone(),
            Running {
                id: id.clone(),
                cwd: cwd.to_path_buf(),
                model,
                name,
                busy: false,
                cmd,
                perm,
                ask,
                cancel,
                inbox,
            },
        );
        Ok(id)
    }

    /// Which session a phone meant. Naming one is the ordinary case; naming
    /// none means the only one there is, and means nothing at all when there
    /// is more than one to choose between.
    fn which(&self, session: &str) -> Option<String> {
        let running = lock(&self.running);
        if !session.is_empty() {
            return running.contains_key(session).then(|| session.to_string());
        }
        match running.len() {
            1 => running.keys().next().cloned(),
            _ => None,
        }
    }

    /// Note what an engine's event means for what a phone would be told.
    fn note(&self, session: &str, ev: &UiEvent) {
        let mut running = lock(&self.running);
        let Some(r) = running.get_mut(session) else {
            return;
        };
        match ev {
            UiEvent::Busy(busy) => r.busy = *busy,
            UiEvent::Renamed(name) => r.name = name.clone(),
            UiEvent::Resumed { id, name, .. } => {
                r.id = id.clone();
                r.name = name.clone();
            }
            _ => {}
        }
    }

    /// Whether a phone may start a session here. The list is read from the
    /// environment or the user's own config and nowhere else, so a repository
    /// cannot widen it by being cloned.
    fn allowed(&self, cwd: &Path) -> Result<PathBuf, String> {
        let full = cwd
            .canonicalize()
            .map_err(|_| format!("no such directory: {}", cwd.display()))?;
        if !full.is_dir() {
            return Err(format!("not a directory: {}", full.display()));
        }
        if self.roots.iter().any(|root| full.starts_with(root)) {
            Ok(full)
        } else {
            Err(format!(
                "{} is not inside anywhere a phone may start a session",
                full.display()
            ))
        }
    }
}

impl Sessions for Machine {
    fn list(&self) -> Vec<SessionInfo> {
        let running = lock(&self.running);
        ah_core::session::summaries()
            .into_iter()
            .map(|s| SessionInfo {
                live: running.contains_key(&s.id),
                id: s.id,
                name: s.name,
                title: s.title,
                cwd: s.cwd,
                model: s.model,
                started_ms: s.started_ms as u64,
                messages: s.messages as u32,
            })
            .collect()
    }

    fn state(&self, session: &str) -> Option<SessionState> {
        let session = self.which(session)?;
        let running = lock(&self.running);
        let r = running.get(&session)?;
        Some(SessionState {
            session: r.id.clone(),
            busy: r.busy,
            model: r.model.clone(),
            cwd: r.cwd.display().to_string(),
            name: r.name.clone(),
            context_tokens: 0,
            context_window: 0,
            usage: Default::default(),
        })
    }

    fn act(&self, session: &str, act: Act) -> Result<Option<String>, String> {
        // The two that make a session rather than needing one.
        match act {
            Act::Start { cwd, model, prompt } => {
                let cwd = self.allowed(Path::new(&cwd))?;
                let id = self.open(&cwd, None, model)?;
                if let Some(prompt) = prompt.filter(|p| !p.trim().is_empty())
                    && let Some(r) = lock(&self.running).get(&id)
                {
                    let _ = r.cmd.send(EngineCmd::Submit {
                        text: prompt,
                        images: Vec::new(),
                    });
                }
                return Ok(Some(id));
            }
            Act::Resume => {
                if lock(&self.running).contains_key(session) {
                    return Ok(None);
                }
                let summary = ah_core::session::summaries()
                    .into_iter()
                    .find(|s| s.id == session)
                    .ok_or_else(|| "no such session".to_string())?;
                // Where it ran is where it goes on running, and that is not
                // negotiable from a phone: the directory came from the
                // session's own file.
                let cwd = PathBuf::from(&summary.cwd);
                let id = self.open(&cwd, Some(session), None)?;
                return Ok(Some(id));
            }
            _ => {}
        }

        let session = self
            .which(session)
            .ok_or_else(|| "that session is not running; resume it first".to_string())?;
        let running = lock(&self.running);
        let r = running
            .get(&session)
            .ok_or_else(|| "that session is not running; resume it first".to_string())?;
        fn gone<T>(_: mpsc::SendError<T>) -> String {
            "the session stopped listening".to_string()
        }
        match act {
            Act::Submit { text, images } => {
                if r.busy {
                    r.inbox.push(text);
                } else {
                    r.cmd
                        .send(EngineCmd::Submit { text, images })
                        .map_err(gone)?;
                }
            }
            Act::Interrupt => r.cancel.store(true, Ordering::Relaxed),
            Act::AllowTool(allow) => r.perm.send(allow).map_err(gone)?,
            Act::Answer(reply) => r.ask.send(reply).map_err(gone)?,
            Act::Compact(focus) => r.cmd.send(EngineCmd::Compact(focus)).map_err(gone)?,
            Act::Clear => r.cmd.send(EngineCmd::Clear).map_err(gone)?,
            Act::Rename(name) => r.cmd.send(EngineCmd::Rename(name)).map_err(gone)?,
            Act::Start { .. } | Act::Resume => unreachable!("handled above"),
        }
        Ok(None)
    }
}

/// Where a phone may start a session: `remote.roots`, or the home directory.
///
/// Read the way the relay URL is read — from the environment, or from the
/// user's own config, and from nowhere a repository can reach. A cloned
/// project naming directories a phone may run an agent in would be somebody
/// else choosing what this machine is willing to do.
fn roots(stack: &SettingsStack) -> Vec<PathBuf> {
    if let Ok(from_env) = std::env::var("AH_REMOTE_ROOTS") {
        let listed: Vec<PathBuf> = from_env
            .split(':')
            .filter(|p| !p.trim().is_empty())
            .filter_map(|p| PathBuf::from(p.trim()).canonicalize().ok())
            .collect();
        if !listed.is_empty() {
            return listed;
        }
    }
    let user = ah_core::paths::user_config_file();
    let listed: Vec<String> = stack
        .layers()
        .iter()
        .filter(|l| {
            matches!(l.origin, Origin::Defaults | Origin::Cli)
                || matches!(&l.origin, Origin::File(p) if *p == user)
        })
        .filter_map(|l| l.patch.pointer("/remote/roots").cloned())
        .filter_map(|v| serde_json::from_value::<Vec<String>>(v).ok())
        .next_back()
        .unwrap_or_default();
    let listed: Vec<PathBuf> = listed
        .iter()
        .filter_map(|p| PathBuf::from(p).canonicalize().ok())
        .collect();
    if !listed.is_empty() {
        return listed;
    }
    dirs::home_dir().into_iter().collect()
}

/// Whether a phone's sessions are allowed to run tools without asking. Read
/// from the same trusted places, for the same reason.
fn trusted(stack: &SettingsStack) -> bool {
    let user = ah_core::paths::user_config_file();
    stack
        .layers()
        .iter()
        .filter(|l| {
            matches!(l.origin, Origin::Defaults | Origin::Cli)
                || matches!(&l.origin, Origin::File(p) if *p == user)
        })
        .filter_map(|l| l.patch.pointer("/remote/trust_paired_device").cloned())
        .filter_map(|v| v.as_bool())
        .next_back()
        .unwrap_or(false)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run until told to stop.
pub fn serve(o: &Overrides, detach: bool) -> Result<(), AnyError> {
    if detach {
        return relaunch(o);
    }
    // So the machine can say which of the two is publishing.
    // SAFETY: set before any thread that reads it exists.
    unsafe { std::env::set_var("AH_REMOTE_DAEMON", "1") };

    let stack = crate::app::load_settings(o)?;
    let settings: Settings = stack.settings().clone();
    if ah_core::auth::remote_code().is_none() {
        return Err("not paired. `ah remote pair --url <relay>` sets one up.".into());
    }

    let (events_tx, events_rx) = mpsc::channel();
    let machine = Arc::new(Machine {
        running: Mutex::new(HashMap::new()),
        roots: roots(&stack),
        trusted: trusted(&stack),
        max_sessions: settings.remote.max_sessions.max(1),
        events: events_tx,
    });

    println!("ah remote serve");
    for root in &machine.roots {
        println!("  sessions may start under {}", root.display());
    }
    println!(
        "  tools {}",
        if machine.trusted {
            "run without asking (remote.trust_paired_device)"
        } else {
            "are asked about"
        }
    );

    let cancel = Arc::new(AtomicBool::new(false));
    crate::cli::install_ctrlc(cancel.clone());

    let (notes_tx, notes_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("ah-remote-notes".into())
        .spawn(move || {
            while let Ok(note) = notes_rx.recv() {
                match note {
                    Note::Attached(device) => println!("{device} attached"),
                    Note::Detached(device) => println!("{device} left"),
                    Note::Said(text) => println!("{text}"),
                    // Nothing to hand over to: a window takes the lock by
                    // asking, and the publisher stands down on its own.
                    Note::Yield => println!("a window asked for the pairing"),
                }
            }
        })
        .expect("spawn notes");

    // Publishing comes and goes: a window takes the pairing while it is open,
    // and this picks it up again when the window closes.
    let mut publisher: Option<Publisher> = None;
    let mut settings_for_publisher = crate::app::load_settings(o)?.settings().clone();
    // Publishing is what this command is for, whatever the config says about
    // windows doing it.
    settings_for_publisher.remote.enabled = true;

    while !cancel.load(Ordering::Relaxed) {
        if publisher.is_none() {
            publisher = publisher::start(
                &settings_for_publisher,
                machine.clone() as Arc<dyn Sessions>,
                notes_tx.clone(),
            );
            if publisher.is_some() {
                println!("publishing");
            }
        }
        match events_rx.recv_timeout(RETRY) {
            Ok((session, ev)) => {
                machine.note(&session, &ev);
                if let Some(p) = publisher.as_ref() {
                    p.observe(&session, &ev);
                    if matches!(ev, UiEvent::Busy(_) | UiEvent::Renamed(_)) {
                        p.state_changed(&session);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // A publisher that stood down for a window leaves its thread;
                // noticing costs one check every couple of seconds.
                if publisher.as_ref().is_some_and(|p| {
                    matches!(p.state(), publisher::State::Failed)
                        || publisher::yield_path().exists()
                }) {
                    publisher = None;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    println!("stopping");
    drop(publisher);
    ah_core::agents::table().shutdown(Duration::from_secs(2));
    ah_core::jobs::table().shutdown(Duration::from_millis(500));
    Ok(())
}

/// Start again, detached, and let this one go.
///
/// Re-exec rather than fork: the child is a fresh process with a group of its
/// own, which is what makes it survive the terminal that started it, and it
/// needs none of the FFI a fork would.
#[cfg(unix)]
fn relaunch(o: &Overrides) -> Result<(), AnyError> {
    use std::os::unix::process::CommandExt;
    let log = ah_core::paths::data_dir().join("remote.log");
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.args(["remote", "serve"]);
    if let Some(cwd) = &o.cwd {
        command.arg("--cwd").arg(cwd);
    }
    let child = command
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .spawn()?;
    println!("serving in the background as {}", child.id());
    println!("what it says goes to {}", log.display());
    Ok(())
}

#[cfg(not(unix))]
fn relaunch(_o: &Overrides) -> Result<(), AnyError> {
    Err("running in the background needs a unix host; `ah remote serve` works everywhere".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(roots: Vec<PathBuf>) -> Machine {
        let (events, _rx) = mpsc::channel();
        Machine {
            running: Mutex::new(HashMap::new()),
            roots,
            trusted: false,
            max_sessions: 4,
            events,
        }
    }

    #[test]
    fn a_directory_outside_the_roots_is_refused() {
        let home = std::env::temp_dir().join(format!("ah-roots-{}", std::process::id()));
        let inside = home.join("project");
        std::fs::create_dir_all(&inside).unwrap();
        let m = machine(vec![home.canonicalize().unwrap()]);

        assert!(m.allowed(&inside).is_ok(), "inside is allowed");
        // Every one of these resolves somewhere the list does not cover.
        assert!(m.allowed(Path::new("/etc")).is_err());
        assert!(m.allowed(&inside.join("../../..")).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_directory_that_is_not_there_is_not_invented() {
        let m = machine(vec![std::env::temp_dir()]);
        assert!(m.allowed(Path::new("/no/such/place/at/all")).is_err());
    }

    #[test]
    fn roots_are_not_taken_from_a_project_file() {
        let _env = crate::remote::env_guard();
        let mut stack = SettingsStack::new();
        // A cloned repository says a phone may start sessions anywhere.
        stack
            .push(
                Origin::File("/somebody/elses/repo/.ah/config.toml".into()),
                serde_json::json!({"remote": {"roots": ["/"]}}),
            )
            .unwrap();
        assert_ne!(
            roots(&stack),
            vec![PathBuf::from("/")],
            "a repository chose where a phone may run an agent"
        );
    }

    #[test]
    fn trust_is_not_taken_from_a_project_file_either() {
        let _env = crate::remote::env_guard();
        let mut stack = SettingsStack::new();
        stack
            .push(
                Origin::File("/somebody/elses/repo/.ah/config.toml".into()),
                serde_json::json!({"remote": {"trust_paired_device": true}}),
            )
            .unwrap();
        assert!(!trusted(&stack), "a repository turned the asking off");
        // The user's own config is a different matter.
        let mut mine = SettingsStack::new();
        mine.push(
            Origin::Cli,
            serde_json::json!({"remote": {"trust_paired_device": true}}),
        )
        .unwrap();
        assert!(trusted(&mine));
    }
}
