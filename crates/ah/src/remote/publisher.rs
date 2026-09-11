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
use std::sync::atomic::{AtomicBool, Ordering};
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

use crate::app::UiEvent;
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
    notes: mpsc::Sender<Note>,
) -> Option<Publisher> {
    let raw = ah_remote::code::parse(&ah_core::auth::remote_code()?)?;
    let url = ah_core::auth::remote_url()?;
    Publisher::start(&settings.remote, url, Keys::derive(&raw), live, notes)
}

impl Publisher {
    /// Start publishing, if this machine is paired, configured to, and not
    /// already publishing from another window.
    pub fn start(
        settings: &RemoteSettings,
        url: String,
        keys: Keys,
        live: Live,
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
        let UiEvent::Agent(ev) = ev else {
            // Everything else the UI hears about is either already reflected
            // in what a phone is told, or is none of its business.
            return;
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
        // Everything else is a phone asking this session to do something,
        // which it cannot yet. Saying so beats silence.
        _ => vec![FromDesk::Ack {
            cmd_seq: seq,
            ok: false,
            error: Some("this build can be watched but not driven".into()),
        }],
    })
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
