//! Putting a session on the wire.
//!
//! Every `UiEvent` the engine produces passes through [`Publisher::observe`],
//! which does no work beyond putting it in a list: it is called from the
//! thread the UI is waiting on, and a socket has no business being there. A
//! thread of its own seals what has gathered and sends it.
//!
//! Text is gathered rather than sent as it arrives. A turn produces thousands
//! of small pieces and nobody reads them one at a time, so they travel in
//! batches; anything a person has to see on its own — a tool starting, an
//! error, the end of a turn — goes immediately and is never folded into
//! anything else.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ah_core::abi::{RemoteSettings, Usage};
use ah_core::agent::{AgentEvent, event_json};
use ah_remote::crypto::{self, Keys, Opener, Sealer};
use ah_remote::link::{Config, Event, Link};
use ah_remote::proto::{
    Bye, Dir, Envelope, FromDesk, FromPhone, Hello, PROTO, Role, SessionInfo, SessionState,
};
use serde_json::Value;

use crate::app::{EngineCmd, Inbox, UiEvent};
use crate::remote::lock::Lock;

/// How often the thread looks at what has gathered. Short enough that the
/// flush deadline is honoured to within a frame of itself.
const TICK: Duration = Duration::from_millis(20);

/// What the rest of the program is told about the link.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// A phone attached, by the name it gave for itself.
    Attached(String),
    /// Something about the link worth a line in the transcript.
    Said(String),
}

/// Where the link is, for the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Publishing.
    Up,
    /// Dialling, or waiting to dial again.
    Dialling,
    /// Not coming back without something changing.
    Failed,
}

/// The ways into the running session, cloned from the channels the window
/// itself uses. A phone reaches the engine by exactly the same means as the
/// keyboard does, which is why the two can answer the same question.
pub struct Reach {
    pub cmd: mpsc::Sender<EngineCmd>,
    pub perm: mpsc::Sender<bool>,
    pub ask: mpsc::Sender<ah_core::abi::Reply>,
    pub cancel: Arc<AtomicBool>,
    pub inbox: Arc<Inbox>,
}

/// What a phone is told about the session being watched.
#[derive(Debug, Clone, Default)]
pub struct Live {
    pub session: String,
    pub name: Option<String>,
    pub cwd: String,
    pub model: String,
    pub busy: bool,
    pub context_tokens: u64,
    pub context_window: u64,
    pub usage: Usage,
}

/// Publishes one session for as long as it is held.
pub struct Publisher {
    shared: Arc<Shared>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Held, not used: while this exists no other window publishes.
    _lock: Lock,
}

struct Shared {
    settings: RemoteSettings,
    batch: Mutex<Batch>,
    live: Mutex<Live>,
    up: AtomicBool,
    failed: AtomicBool,
    /// The question on screen, if there is one. An answer for anything else
    /// arrived too late and is dropped rather than applied to whatever came
    /// next.
    pending: AtomicU64,
    /// Who answered the question with this number, when it was a phone.
    answered_by: Mutex<Option<(u64, String)>>,
    reach: Reach,
}

/// What has gathered since the last frame went out.
#[derive(Default)]
struct Batch {
    events: Vec<Value>,
    bytes: usize,
    /// Set by an event nobody should have to wait 200 ms to see.
    urgent: bool,
    /// Payloads that are not events, which never wait at all.
    ahead: Vec<FromDesk>,
}

/// Start publishing if everything it needs is in place: a pairing, a relay to
/// reach it through, the setting turned on, and no other window already doing
/// it. Any of those missing is an ordinary `None`, not a failure worth saying
/// anything about.
pub fn start(
    settings: &ah_core::abi::Settings,
    live: Live,
    reach: Reach,
    notes: mpsc::Sender<Note>,
) -> Option<Publisher> {
    let raw = ah_remote::code::parse(&ah_core::auth::remote_code()?)?;
    let url = ah_core::auth::remote_url()?;
    Publisher::start(
        &settings.remote,
        url,
        Keys::derive(&raw),
        live,
        reach,
        notes,
    )
}

impl Publisher {
    /// Start publishing, if this machine is paired, configured to, and not
    /// already publishing from another window.
    pub fn start(
        settings: &RemoteSettings,
        url: String,
        keys: Keys,
        live: Live,
        reach: Reach,
        notes: mpsc::Sender<Note>,
    ) -> Option<Self> {
        if !settings.enabled {
            return None;
        }
        let lock = Lock::take()?;
        let shared = Arc::new(Shared {
            settings: settings.clone(),
            batch: Mutex::new(Batch::default()),
            live: Mutex::new(live),
            up: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            pending: AtomicU64::new(0),
            answered_by: Mutex::new(None),
            reach,
        });

        let (socket_tx, socket_rx) = mpsc::channel();
        let link = Link::open(
            Config {
                url,
                role: Role::Desk,
                keys: keys.clone(),
            },
            move |ev| {
                let _ = socket_tx.send(ev);
            },
        )
        .ok()?;

        let (stop_tx, stop_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("ah-remote-pub".into())
            .spawn({
                let shared = shared.clone();
                move || run(shared, keys, link, socket_rx, stop_rx, notes)
            })
            .ok()?;

        Some(Self {
            shared,
            stop: Some(stop_tx),
            thread: Some(thread),
            _lock: lock,
        })
    }

    /// One event from the engine. Called on the thread the UI is waiting on,
    /// so it only ever adds to a list.
    pub fn observe(&self, ev: &UiEvent) {
        let session = || lock(&self.shared.live).session.clone();
        let ev = match ev {
            UiEvent::Agent(ev) => ev,
            // A question waits for somebody, so it does not wait for a batch.
            UiEvent::AskPermission { id, call, reason } => {
                self.shared.pending.store(*id, Ordering::Release);
                return self.ahead(FromDesk::AskPermission {
                    session: session(),
                    id: *id,
                    call: call.clone(),
                    reason: reason.clone(),
                });
            }
            UiEvent::AskUser { id, ask } => {
                self.shared.pending.store(*id, Ordering::Release);
                return self.ahead(FromDesk::AskUser {
                    session: session(),
                    id: *id,
                    ask: (**ask).clone(),
                });
            }
            UiEvent::Answered { id } => {
                self.shared.pending.store(0, Ordering::Release);
                let by = match lock(&self.shared.answered_by).take() {
                    Some((answered, who)) if answered == *id => who,
                    _ => "the desk".to_string(),
                };
                return self.ahead(FromDesk::Answered {
                    session: session(),
                    id: *id,
                    by,
                });
            }
            // Everything else the window hears about is either already in
            // what a phone is told, or is none of its business.
            _ => return,
        };
        let urgent = matches!(
            ev,
            AgentEvent::ToolStart(_)
                | AgentEvent::ToolEnd { .. }
                | AgentEvent::ToolDenied { .. }
                | AgentEvent::Error(_)
                | AgentEvent::AssistantMessage(_)
                | AgentEvent::Image { .. }
                | AgentEvent::Compacted { .. }
                | AgentEvent::TurnEnd(_)
        );
        let mut json = event_json(ev);
        trim(&mut json, self.shared.settings.max_event_bytes);

        let mut batch = lock(&self.shared.batch);
        batch.bytes += json.to_string().len();
        fold(&mut batch.events, json);
        batch.urgent |= urgent;
    }

    /// Something that should not wait for the next batch.
    fn ahead(&self, payload: FromDesk) {
        lock(&self.shared.batch).ahead.push(payload);
    }

    /// Tell the link what the session is now, so a phone that asks is not
    /// told what it was.
    pub fn live(&self, live: Live) {
        *lock(&self.shared.live) = live;
    }

    pub fn state(&self) -> State {
        if self.shared.up.load(Ordering::Acquire) {
            State::Up
        } else if self.shared.failed.load(Ordering::Acquire) {
            State::Failed
        } else {
            State::Dialling
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if let Some(s) = &self.stop {
            let _ = s.send(());
        }
        self.stop = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Fold an event into what is already waiting, where folding loses nothing.
fn fold(events: &mut Vec<Value>, next: Value) {
    let kind = next.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if matches!(kind, "text" | "reasoning")
        && let Some(last) = events.last_mut()
        && last.get("type").and_then(|v| v.as_str()) == Some(kind)
    {
        // Two runs of the same kind read as one run. Anything else would be
        // two of whatever it is, which is not the same thing at all.
        let head = last.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let tail = next.get("text").and_then(|v| v.as_str()).unwrap_or("");
        last["text"] = Value::String(format!("{head}{tail}"));
        return;
    }
    if kind == "compact_progress" {
        // Only the latest count means anything; the ones before it were only
        // ever going to be replaced.
        if let Some(last) = events.last_mut()
            && last.get("type").and_then(|v| v.as_str()) == Some("compact_progress")
        {
            *last = next;
            return;
        }
    }
    events.push(next);
}

/// Cut a single event down to something worth sending over a phone link.
fn trim(event: &mut Value, max: usize) {
    for field in ["text", "error", "summary"] {
        let Some(Value::String(s)) = event.get_mut(field) else {
            continue;
        };
        if s.len() > max {
            let left = s.len() - max;
            s.truncate(max);
            s.push_str(&format!("\n… [{left} bytes not sent to the phone]"));
        }
    }
    // A tool's output is the one that actually gets long.
    if let Some(Value::String(s)) = event.pointer_mut("/result/output")
        && s.len() > max
    {
        let left = s.len() - max;
        s.truncate(max);
        s.push_str(&format!("\n… [{left} bytes not sent to the phone]"));
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn run(
    shared: Arc<Shared>,
    keys: Keys,
    link: Link,
    socket: mpsc::Receiver<Event>,
    stop: mpsc::Receiver<()>,
    notes: mpsc::Sender<Note>,
) {
    let mut id = crypto::new_link();
    let mut seal = sealer(&keys, &id);
    // One per phone, because each seals under a key of its own.
    let mut openers: HashMap<String, Opener> = HashMap::new();
    let mut outbox: Vec<FromDesk> = Vec::new();
    let mut last_flush = Instant::now();
    let flush_after = Duration::from_millis(shared.settings.flush_ms);

    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(TICK) {
        // What the socket has to say first: a fresh link changes the key
        // everything after it is sealed under.
        while let Ok(ev) = socket.try_recv() {
            match ev {
                Event::Open => {
                    // A new connection is a new link, so numbering starts
                    // again under a key nothing has used.
                    id = crypto::new_link();
                    seal = sealer(&keys, &id);
                    openers.clear();
                    shared.up.store(true, Ordering::Release);
                    outbox.insert(0, hello());
                    outbox.push(FromDesk::State(state_of(&lock(&shared.live))));
                }
                Event::Frame(raw) => {
                    if let Some(reply) = answer(&raw, &keys, &mut openers, &shared, &notes) {
                        outbox.extend(reply);
                    }
                }
                Event::Lost(_) => shared.up.store(false, Ordering::Release),
                Event::Fatal(why) => {
                    shared.up.store(false, Ordering::Release);
                    shared.failed.store(true, Ordering::Release);
                    let _ = notes.send(Note::Said(format!("remote: {why}")));
                }
            }
        }

        // Then whatever has gathered, if it is time or there is a reason not
        // to wait.
        {
            let mut batch = lock(&shared.batch);
            let due = last_flush.elapsed() >= flush_after
                || batch.bytes >= shared.settings.max_frame_bytes
                || batch.urgent;
            outbox.append(&mut batch.ahead);
            if due && !batch.events.is_empty() {
                let session = lock(&shared.live).session.clone();
                outbox.push(FromDesk::Events {
                    session,
                    evs: std::mem::take(&mut batch.events),
                });
                batch.bytes = 0;
                batch.urgent = false;
                last_flush = Instant::now();
            }
        }

        if outbox.is_empty() {
            continue;
        }
        if !shared.up.load(Ordering::Acquire) {
            // Nowhere to put them yet. Keep what fits and drop the oldest:
            // a phone that missed the middle of a turn is told there is a
            // gap, which is better than a window that grows without end.
            trim_outbox(&mut outbox, shared.settings.outbox_bytes);
            continue;
        }
        for payload in outbox.drain(..) {
            let plain = serde_json::to_vec(&payload).unwrap_or_default();
            let (seq, ct) = seal.seal(&plain);
            link.send(
                serde_json::to_string(&Envelope::publish(&hex(&id), seq, ct)).unwrap_or_default(),
            );
        }
    }

    // On the way out, say so: a phone that knows the desktop left stops
    // waiting for it.
    if shared.up.load(Ordering::Acquire) {
        let plain = serde_json::to_vec(&FromDesk::Bye { reason: Bye::Quit }).unwrap_or_default();
        let (seq, ct) = seal.seal(&plain);
        link.send(
            serde_json::to_string(&Envelope::publish(&hex(&id), seq, ct)).unwrap_or_default(),
        );
        // Long enough for the socket thread to pick it up off the queue.
        std::thread::sleep(Duration::from_millis(60));
    }
}

/// Keep the newest of what could not be sent. What falls off the front is
/// what a phone will be told it missed.
fn trim_outbox(outbox: &mut Vec<FromDesk>, max: usize) {
    let mut bytes: usize = outbox
        .iter()
        .map(|p| serde_json::to_vec(p).map(|v| v.len()).unwrap_or(0))
        .sum();
    while bytes > max && outbox.len() > 1 {
        let first = serde_json::to_vec(&outbox[0]).map(|v| v.len()).unwrap_or(0);
        outbox.remove(0);
        bytes -= first;
    }
}

fn sealer(keys: &Keys, id: &[u8; crypto::LINK_BYTES]) -> Sealer {
    let none = [0u8; crypto::LINK_BYTES];
    Sealer::new(keys.link_key(Dir::D2p, id, &none), Dir::D2p, *id, none)
}

/// What a phone asked for, and what it gets back. Read-only: what a phone can
/// say that changes anything is not wired up yet.
fn answer(
    raw: &str,
    keys: &Keys,
    openers: &mut HashMap<String, Opener>,
    shared: &Shared,
    notes: &mpsc::Sender<Note>,
) -> Option<Vec<FromDesk>> {
    let Ok(Envelope::Cmd {
        link,
        plink,
        seq,
        ct,
        ..
    }) = serde_json::from_str::<Envelope>(raw)
    else {
        return None;
    };
    let id = unhex(&link)?;
    let phone = unhex(&plink)?;
    let opener = openers
        .entry(plink.clone())
        .or_insert_with(|| Opener::new(keys.link_key(Dir::P2d, &id, &phone), Dir::P2d, id, phone));
    let plain = opener.open(seq, &ct).ok()?;
    let asked: FromPhone = serde_json::from_slice(plain).ok()?;

    let live = lock(&shared.live).clone();
    let reach = &shared.reach;
    let say = |what: String| {
        if shared.settings.notice {
            let _ = notes.send(Note::Said(what));
        }
    };
    let ok = |error: Option<String>| {
        vec![FromDesk::Ack {
            cmd_seq: seq,
            ok: error.is_none(),
            error,
        }]
    };

    Some(match asked {
        FromPhone::List => vec![FromDesk::Sessions {
            list: sessions(&live),
        }],
        FromPhone::Attach { device, .. } => {
            if shared.settings.notice {
                let _ = notes.send(Note::Attached(device));
            }
            let (messages, truncated) = snapshot(&live, shared.settings.snapshot_messages);
            vec![
                FromDesk::State(state_of(&live)),
                FromDesk::Snapshot {
                    session: live.session.clone(),
                    messages,
                    truncated,
                },
            ]
        }
        FromPhone::Detach { .. } => Vec::new(),

        // Said while the turn is running, it goes to the mailbox the loop
        // reads between requests; said while nothing is running, it starts a
        // turn. The phone does not have to know which, and cannot know it
        // without being wrong about it sometimes.
        FromPhone::Submit { text, images, .. } => {
            say(format!("remote: \u{201c}{}\u{201d}", first_line(&text)));
            if live.busy {
                reach.inbox.push(text);
            } else {
                let _ = reach.cmd.send(EngineCmd::Submit { text, images });
            }
            ok(None)
        }
        FromPhone::Interrupt { .. } => {
            // The same flag Esc sets, so a turn stopped from a phone stops
            // the way a turn stopped at the keyboard does.
            reach.cancel.store(true, Ordering::Relaxed);
            say("remote: stopped".into());
            ok(None)
        }
        FromPhone::AnswerPermission { id, allow, .. } => {
            if shared.pending.load(Ordering::Acquire) != id {
                // Answered already, or answered by somebody else. Sending it
                // on would apply it to whatever question came next.
                ok(Some("that question has been answered".into()))
            } else {
                *lock(&shared.answered_by) = Some((id, device_name(openers, &plink)));
                let _ = reach.perm.send(allow);
                say(format!(
                    "remote: {}",
                    if allow { "allowed" } else { "denied" }
                ));
                ok(None)
            }
        }
        FromPhone::AnswerAsk { id, reply, .. } => {
            if shared.pending.load(Ordering::Acquire) != id {
                ok(Some("that question has been answered".into()))
            } else {
                *lock(&shared.answered_by) = Some((id, device_name(openers, &plink)));
                let _ = reach.ask.send(reply);
                say("remote: answered".into());
                ok(None)
            }
        }
        FromPhone::Compact { focus, .. } => {
            let _ = reach.cmd.send(EngineCmd::Compact(focus));
            ok(None)
        }
        FromPhone::Clear { .. } => {
            let _ = reach.cmd.send(EngineCmd::Clear);
            say("remote: cleared".into());
            ok(None)
        }
        FromPhone::Rename { name, .. } => {
            let _ = reach.cmd.send(EngineCmd::Rename(name));
            ok(None)
        }
        FromPhone::GetBlob { path, .. } => blob(&live, &path, shared.settings.max_frame_bytes),

        // Starting a session somewhere else is not this window's to give.
        FromPhone::Resume { .. } | FromPhone::NewSession { .. } => ok(Some(
            "this window publishes one session; starting another needs the daemon".into(),
        )),
    })
}

/// The first line of what was said, for a transcript that should not be
/// swamped by a phone pasting an essay into it.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 60 {
        format!("{}\u{2026}", line.chars().take(60).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Which phone this was, as far as anything here knows: the first characters
/// of the link it seals under. Nothing a person named, and nothing that
/// follows it between connections.
fn device_name(openers: &HashMap<String, Opener>, plink: &str) -> String {
    let _ = openers;
    format!("phone {}", &plink[..plink.len().min(8)])
}

/// A file the session produced, in pieces small enough to travel.
///
/// Only what this session drew, and only by a path that is still inside the
/// directory it draws into once every `..` in it has been resolved: the path
/// came from a phone, and a phone is not this machine.
fn blob(live: &Live, path: &str, chunk: usize) -> Vec<FromDesk> {
    let deny = |why: &str| {
        vec![FromDesk::Ack {
            cmd_seq: 0,
            ok: false,
            error: Some(why.to_string()),
        }]
    };
    let dir = ah_core::paths::session_images_dir(&live.session);
    let Ok(dir) = dir.canonicalize() else {
        return deny("this session has drawn nothing");
    };
    let Ok(full) = std::path::Path::new(path).canonicalize() else {
        return deny("no such file");
    };
    if !full.starts_with(&dir) {
        return deny("not this session's to give");
    }
    let Ok(bytes) = std::fs::read(&full) else {
        return deny("no such file");
    };
    let mime = match full.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    };
    let id = full
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string();
    // Base64 grows by a third, so the pieces are cut from the smaller number.
    let per = (chunk * 3 / 4).max(1024);
    let chunks: Vec<&[u8]> = bytes.chunks(per).collect();
    let last = chunks.len().saturating_sub(1);
    chunks
        .iter()
        .enumerate()
        .map(|(i, piece)| FromDesk::Blob {
            id: id.clone(),
            mime: mime.to_string(),
            seq: i as u32,
            last: i == last,
            b64: ah_remote::base64(piece),
        })
        .collect()
}

/// Every session on this machine, with the live one marked.
fn sessions(live: &Live) -> Vec<SessionInfo> {
    ah_core::session::summaries()
        .into_iter()
        .map(|s| SessionInfo {
            live: s.id == live.session,
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

/// The tail of the conversation, read from the session file rather than kept
/// in memory: it is the same text, and it survives this window restarting.
fn snapshot(live: &Live, want: usize) -> (Vec<ah_core::abi::Message>, bool) {
    let Ok(session) = ah_core::session::Session::open(&live.session) else {
        return (Vec::new(), false);
    };
    let total = session.messages.len();
    let from = total.saturating_sub(want);
    (session.messages[from..].to_vec(), from > 0)
}

fn state_of(live: &Live) -> SessionState {
    SessionState {
        session: live.session.clone(),
        busy: live.busy,
        model: live.model.clone(),
        cwd: live.cwd.clone(),
        name: live.name.clone(),
        context_tokens: live.context_tokens,
        context_window: live.context_window,
        usage: live.usage,
    }
}

fn hello() -> FromDesk {
    FromDesk::Hello(Hello {
        host: hostname(),
        os: std::env::consts::OS.to_string(),
        ah_version: env!("CARGO_PKG_VERSION").to_string(),
        proto: PROTO,
        holder: "tui".into(),
    })
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this machine".into())
}

fn hex(raw: &[u8]) -> String {
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; crypto::LINK_BYTES]> {
    if s.len() != crypto::LINK_BYTES * 2 {
        return None;
    }
    let mut out = [0u8; crypto::LINK_BYTES];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(t: &str) -> Value {
        json!({"type": "text", "text": t})
    }

    #[test]
    fn a_run_of_text_becomes_one_piece() {
        let mut events = Vec::new();
        for part in ["Let ", "me ", "check."] {
            fold(&mut events, text(part));
        }
        assert_eq!(events.len(), 1, "three deltas, one event");
        assert_eq!(events[0]["text"], "Let me check.");
    }

    #[test]
    fn a_tool_call_is_not_folded_into_the_text_around_it() {
        let mut events = Vec::new();
        fold(&mut events, text("before "));
        fold(&mut events, json!({"type": "tool_start", "call": {}}));
        fold(&mut events, text("after"));
        assert_eq!(
            events.len(),
            3,
            "the tool call keeps them apart: {events:?}"
        );
        assert_eq!(events[0]["text"], "before ");
        assert_eq!(events[2]["text"], "after");
    }

    #[test]
    fn thinking_and_speaking_are_not_the_same_run() {
        let mut events = Vec::new();
        fold(&mut events, json!({"type": "reasoning", "text": "hmm"}));
        fold(&mut events, text("hello"));
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn only_the_last_count_of_a_compaction_survives() {
        let mut events = Vec::new();
        for done in [10, 20, 30] {
            fold(
                &mut events,
                json!({"type": "compact_progress", "done": done, "budget": 100}),
            );
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["done"], 30);
    }

    #[test]
    fn an_event_too_big_for_a_phone_is_cut_with_a_note() {
        let mut ev = text(&"x".repeat(500));
        trim(&mut ev, 100);
        let out = ev["text"].as_str().unwrap();
        assert!(out.starts_with(&"x".repeat(100)));
        assert!(out.contains("400 bytes not sent"), "{out}");
    }

    #[test]
    fn what_a_tool_printed_is_cut_the_same_way() {
        let mut ev = json!({"type": "tool_end", "result": {"output": "y".repeat(500)}});
        trim(&mut ev, 100);
        let out = ev.pointer("/result/output").unwrap().as_str().unwrap();
        assert!(out.contains("400 bytes not sent"), "{out}");
    }

    #[test]
    fn an_event_that_fits_is_left_exactly_as_it_was() {
        let mut ev = text("short");
        trim(&mut ev, 100);
        assert_eq!(ev["text"], "short");
    }

    #[test]
    fn what_could_not_be_sent_is_kept_newest_first() {
        let mut outbox: Vec<FromDesk> = (0..50)
            .map(|i| FromDesk::Notice {
                text: format!("notice number {i} with enough text to weigh something"),
            })
            .collect();
        trim_outbox(&mut outbox, 500);
        assert!(outbox.len() < 50, "some were dropped");
        assert!(!outbox.is_empty(), "not all of them");
        // The newest is the one worth keeping.
        let FromDesk::Notice { text } = outbox.last().unwrap() else {
            panic!("wrong kind");
        };
        assert!(text.contains("number 49"), "kept {text}");
    }

    #[test]
    fn one_frame_is_never_dropped_however_big_it_is() {
        let mut outbox = vec![FromDesk::Notice {
            text: "x".repeat(10_000),
        }];
        trim_outbox(&mut outbox, 10);
        assert_eq!(
            outbox.len(),
            1,
            "dropping the only frame says nothing at all"
        );
    }
}

#[cfg(test)]
mod files {
    use super::*;

    /// A session with one drawing in it, and a data directory of its own.
    fn drawn(dir: &std::path::Path) -> Live {
        // SAFETY: every test that moves this variable holds the same guard.
        unsafe { std::env::set_var("AH_DATA_DIR", dir) };
        let live = Live {
            session: "abc123".into(),
            ..Default::default()
        };
        let images = ah_core::paths::session_images_dir(&live.session);
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join("drawing.png"), vec![7u8; 3000]).unwrap();
        live
    }

    fn refused(out: &[FromDesk]) -> Option<String> {
        match out.first() {
            Some(FromDesk::Ack {
                ok: false, error, ..
            }) => error.clone(),
            _ => None,
        }
    }

    #[test]
    fn a_drawing_comes_back_in_pieces_that_fit() {
        let _env = crate::remote::env_guard();
        let dir = std::env::temp_dir().join(format!("ah-blob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let live = drawn(&dir);
        let path = ah_core::paths::session_images_dir(&live.session).join("drawing.png");

        let out = blob(&live, path.to_str().unwrap(), 1024);
        assert!(
            out.len() > 1,
            "3000 bytes does not fit in one frame: {}",
            out.len()
        );
        let mut seen = Vec::new();
        for (i, piece) in out.iter().enumerate() {
            let FromDesk::Blob {
                seq,
                last,
                b64,
                mime,
                ..
            } = piece
            else {
                panic!("not a file: {piece:?}");
            };
            assert_eq!(*seq as usize, i, "the pieces are numbered in order");
            assert_eq!(*last, i == out.len() - 1, "only the last says it is last");
            assert_eq!(mime, "image/png");
            seen.push(b64.clone());
        }
        assert!(!seen.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_out_of_the_session_is_refused() {
        let _env = crate::remote::env_guard();
        let dir = std::env::temp_dir().join(format!("ah-blob-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let live = drawn(&dir);
        let images = ah_core::paths::session_images_dir(&live.session);

        // The path comes from a phone, so it is not taken at its word. Every
        // one of these resolves outside the session's own directory.
        for attempt in [
            "/etc/passwd".to_string(),
            images.join("../../../../etc/passwd").display().to_string(),
            images.join("..").display().to_string(),
        ] {
            assert!(
                refused(&blob(&live, &attempt, 1024)).is_some(),
                "it handed over {attempt}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_there_is_not_pretended_about() {
        let _env = crate::remote::env_guard();
        let dir = std::env::temp_dir().join(format!("ah-blob-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let live = drawn(&dir);
        let path = ah_core::paths::session_images_dir(&live.session).join("nothing.png");
        assert_eq!(
            refused(&blob(&live, path.to_str().unwrap(), 1024)).as_deref(),
            Some("no such file")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_line_is_shortened_for_the_transcript() {
        assert_eq!(first_line("hello\nthere"), "hello");
        assert_eq!(first_line("   spaced   "), "spaced");
        let long = "w".repeat(200);
        let cut = first_line(&long);
        assert!(cut.chars().count() <= 61, "{} chars", cut.chars().count());
        assert!(cut.ends_with('\u{2026}'));
    }
}
