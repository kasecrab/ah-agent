//! Putting a machine's sessions on the wire.
//!
//! Every `UiEvent` an engine produces passes through [`Publisher::observe`],
//! which does no work beyond putting it in a list: it is called from the
//! thread something else is waiting on, and a socket has no business being
//! there. A thread of its own seals what has gathered and sends it.
//!
//! Text is gathered rather than sent as it arrives. A turn produces thousands
//! of small pieces and nobody reads them one at a time, so they travel in
//! batches; anything a person has to see on its own — a tool starting, an
//! error, a question waiting on an answer — goes immediately and is never
//! folded into anything else.
//!
//! What a session *is* lives behind [`Sessions`]. This file knows how to seal
//! something and put it on a socket, and nothing else.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ah_core::abi::RemoteSettings;
use ah_core::agent::{AgentEvent, event_json};
use ah_remote::crypto::{self, Keys, Opener, Sealer};
use ah_remote::link::{Config, Event, Link};
use ah_remote::proto::{Bye, Dir, Envelope, FromDesk, FromPhone, Hello, PROTO, Role};
use serde_json::Value;

use crate::app::UiEvent;
use crate::remote::lock::Lock;
use crate::remote::sessions::{self, Act, Sessions};

/// How often the thread looks at what has gathered.
const TICK: Duration = Duration::from_millis(20);

/// How often the holder looks to see whether a window is waiting for the
/// pairing. Five times a second is free; fifty would be rude.
const YIELD_CHECK: Duration = Duration::from_millis(200);

/// How often a desktop with nothing to say tells the relay it is still there.
/// Comfortably under the relay's patience, and rare enough that a day of
/// sitting idle costs a few hundred frames rather than a few hundred thousand.
const KEEPALIVE: Duration = Duration::from_secs(120);

/// How long a window waits for whoever is publishing to stand down before
/// giving up and publishing nothing.
const HANDOVER: Duration = Duration::from_secs(3);

/// Link ids this process will hold decryption state for at once. Twice the
/// number of phones the relay lets attach, so a reconnect never evicts a
/// phone that is still there.
const MAX_OPENERS: usize = 8;

/// Images one message from a phone may carry, and how much of the frame they
/// may be between them. Both well under what the relay will carry at all, so
/// the desktop refuses on its own terms rather than on the relay's.
const MAX_IMAGES: usize = 8;
const MAX_IMAGE_CHARS: usize = 3 * 1024 * 1024;

/// What the rest of the program is told about the link.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// A phone attached, by the name it gave for itself.
    Attached(String),
    /// And has stopped watching.
    Detached(String),
    /// Something worth a line in the transcript.
    Said(String),
    /// A window wants the pairing. Whoever holds it should stand down.
    Yield,
}

/// Where the link is, for the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Up,
    Dialling,
    Failed,
}

/// Publishes a machine's sessions for as long as it is held.
pub struct Publisher {
    shared: Arc<Shared>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Held, not used: while this exists nothing else publishes.
    _lock: Lock,
}

struct Shared {
    settings: RemoteSettings,
    batch: Mutex<Batch>,
    sessions: Arc<dyn Sessions>,
    up: AtomicBool,
    failed: AtomicBool,
    /// Set when the thread has stopped for good. A window taking the pairing
    /// ends it quietly, and whoever is holding this needs to notice: the
    /// batch has nothing draining it otherwise.
    done: AtomicBool,
    /// The question on screen, as the session it belongs to and its number.
    /// An answer for anything else arrived too late, and sending it on would
    /// apply it to whatever came next.
    pending: Mutex<HashMap<String, u64>>,
    /// Who answered that question, when it was a phone.
    answered_by: Mutex<HashMap<(String, u64), String>>,
    /// What was last said about a session, so saying it again is free to ask
    /// for and costs nothing to refuse.
    last_state: Mutex<Option<ah_remote::proto::SessionState>>,
    /// Which phones are watching. A phone attaches whenever it reconnects or
    /// its screen comes back, which is often, and a transcript that said so
    /// every time would be mostly that.
    watching: Mutex<std::collections::HashSet<String>>,
    /// How many more commands will be acted on before the next refill.
    allowance: Allowance,
}

/// A bucket that fills back up over time, so a burst goes through and a loop
/// does not.
///
/// It is not a security boundary — whoever holds the code is allowed to be
/// here — it is what keeps a phone stuck in a retry loop, or a mistake in an
/// app, from starting turns faster than a person could read them and spending
/// a day's model budget doing it.
struct Allowance {
    left: Mutex<(f64, Instant)>,
}

impl Allowance {
    /// Commands allowed at once after a quiet spell, and how many a second are
    /// added back. Twenty is more than a person taps; two a second is more
    /// than a person sustains.
    const BURST: f64 = 20.0;
    const PER_SEC: f64 = 2.0;

    fn new() -> Self {
        Self {
            left: Mutex::new((Self::BURST, Instant::now())),
        }
    }

    fn take(&self) -> bool {
        let mut state = lock(&self.left);
        let (ref mut left, ref mut when) = *state;
        let elapsed = when.elapsed().as_secs_f64();
        *when = Instant::now();
        *left = (*left + elapsed * Self::PER_SEC).min(Self::BURST);
        if *left < 1.0 {
            return false;
        }
        *left -= 1.0;
        true
    }
}

/// What has gathered since the last frame went out.
#[derive(Default)]
struct Batch {
    /// Events by the session they belong to.
    events: HashMap<String, Vec<Value>>,
    /// Which session spoke first, so frames leave in that order.
    order: Vec<String>,
    bytes: usize,
    /// Set by an event nobody should have to wait for a batch to see.
    urgent: bool,
    /// Payloads that are not events, which never wait at all.
    ahead: Vec<FromDesk>,
}

/// Start publishing if everything it needs is in place: a pairing, a relay to
/// reach it through, the setting turned on, and nothing else already doing
/// it. Any of those missing is an ordinary `None`.
pub fn start(
    settings: &ah_core::abi::Settings,
    sessions: Arc<dyn Sessions>,
    notes: mpsc::Sender<Note>,
) -> Option<Publisher> {
    let raw = ah_remote::code::parse(&ah_core::auth::remote_code()?)?;
    let url = ah_core::auth::remote_url()?;
    Publisher::start(&settings.remote, url, Keys::derive(&raw), sessions, notes)
}

impl Publisher {
    pub fn start(
        settings: &RemoteSettings,
        url: String,
        keys: Keys,
        sessions: Arc<dyn Sessions>,
        notes: mpsc::Sender<Note>,
    ) -> Option<Self> {
        if !settings.enabled {
            return None;
        }
        // Asking rather than simply taking: a daemon may be holding this,
        // and a window opening is the one thing it stands down for.
        let lock = ask_for_it(HANDOVER)?;

        let shared = Arc::new(Shared {
            settings: settings.clone(),
            batch: Mutex::new(Batch::default()),
            sessions,
            up: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            done: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            answered_by: Mutex::new(HashMap::new()),
            last_state: Mutex::new(None),
            watching: Mutex::new(std::collections::HashSet::new()),
            allowance: Allowance::new(),
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

    /// One event from one session. Called on a thread something else is
    /// waiting on, so it only ever adds to a list.
    pub fn observe(&self, session: &str, ev: &UiEvent) {
        let ev = match ev {
            UiEvent::Agent(ev) => ev,
            // A question is waiting on somebody, so it does not wait for a
            // batch.
            UiEvent::AskPermission { id, call, reason } => {
                lock(&self.shared.pending).insert(session.to_string(), *id);
                return self.ahead(FromDesk::AskPermission {
                    session: session.to_string(),
                    id: *id,
                    call: call.clone(),
                    reason: reason.clone(),
                });
            }
            UiEvent::AskUser { id, ask } => {
                lock(&self.shared.pending).insert(session.to_string(), *id);
                return self.ahead(FromDesk::AskUser {
                    session: session.to_string(),
                    id: *id,
                    ask: (**ask).clone(),
                });
            }
            UiEvent::Answered { id } => {
                lock(&self.shared.pending).remove(session);
                let by = lock(&self.shared.answered_by)
                    .remove(&(session.to_string(), *id))
                    .unwrap_or_else(|| "the desk".to_string());
                return self.ahead(FromDesk::Answered {
                    session: session.to_string(),
                    id: *id,
                    by,
                });
            }
            // Everything else is either already in what a phone is told, or
            // is none of its business.
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
        if !batch.events.contains_key(session) {
            batch.order.push(session.to_string());
        }
        let run = batch.events.entry(session.to_string()).or_default();
        fold(run, json);
        batch.urgent |= urgent;
    }

    /// Something that should not wait for the next batch.
    fn ahead(&self, payload: FromDesk) {
        lock(&self.shared.batch).ahead.push(payload);
    }

    /// Tell every phone that a session has moved on, if it has.
    ///
    /// Called after every event the engine produces, which is thousands a
    /// turn, so the first thing it does is notice that almost none of them
    /// change anything a phone would be shown.
    pub fn state_changed(&self, session: &str) {
        let Some(state) = self.shared.sessions.state(session) else {
            return;
        };
        let mut last = lock(&self.shared.last_state);
        if last.as_ref() == Some(&state) {
            return;
        }
        // A turn ending is when a new session first has anything in it, and
        // so the moment the list of them is worth sending again.
        let ended = last.as_ref().is_some_and(|was| was.busy) && !state.busy;
        *last = Some(state.clone());
        drop(last);
        self.ahead(FromDesk::State(state));
        if ended {
            self.ahead(FromDesk::Sessions {
                list: self.shared.sessions.list(),
            });
        }
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

impl Publisher {
    /// Whether the thread has stopped, so whoever is holding this knows to let
    /// it go rather than go on handing it events nothing will send.
    pub fn finished(&self) -> bool {
        self.shared.done.load(Ordering::Acquire)
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

/// Where a window says it wants the pairing.
pub fn yield_path() -> std::path::PathBuf {
    ah_core::paths::data_dir().join("remote.yield")
}

/// Whether somebody is actually asking for the pairing.
///
/// The file lives in this user's data directory, so on an ordinary machine
/// nobody else can write it; the ownership check is for the machine that is
/// not ordinary, where a world-writable directory would otherwise let anything
/// on the host take a session off the air by touching a file.
#[cfg(unix)]
pub fn yield_asked() -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: no arguments, no allocation, cannot fail.
    let me = unsafe { geteuid() };
    std::fs::metadata(yield_path()).is_ok_and(|m| m.uid() == me)
}

#[cfg(not(unix))]
pub fn yield_asked() -> bool {
    yield_path().exists()
}

#[cfg(unix)]
unsafe extern "C" {
    fn geteuid() -> u32;
}

/// Ask whoever is publishing to stand down, and wait a little for them to.
///
/// The file is removed either way: left behind, it would keep the next holder
/// standing down forever over a window that has long since gone.
pub fn ask_for_it(patience: Duration) -> Option<Lock> {
    if let Some(lock) = Lock::take() {
        return Some(lock);
    }
    let path = yield_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, b"");
    let deadline = Instant::now() + patience;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(lock) = Lock::take() {
            let _ = std::fs::remove_file(&path);
            return Some(lock);
        }
    }
    let _ = std::fs::remove_file(&path);
    None
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
    if kind == "compact_progress"
        && let Some(last) = events.last_mut()
        && last.get("type").and_then(|v| v.as_str()) == Some("compact_progress")
    {
        // Only the latest count means anything; the ones before it were only
        // ever going to be replaced.
        *last = next;
        return;
    }
    events.push(next);
}

/// Cut a single event down to something worth sending over a phone link.
fn trim(event: &mut Value, max: usize) {
    fn cut(s: &mut String, max: usize) {
        if s.len() > max {
            let left = s.len() - max;
            s.truncate(max);
            s.push_str(&format!("\n… [{left} bytes not sent to the phone]"));
        }
    }
    for field in ["text", "error", "summary"] {
        if let Some(Value::String(s)) = event.get_mut(field) {
            cut(s, max);
        }
    }
    // A tool's output is the one that actually gets long.
    if let Some(Value::String(s)) = event.pointer_mut("/result/output") {
        cut(s, max);
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
    let mut last_yield_check = Instant::now();
    let mut last_heard = Instant::now();
    let flush_after = Duration::from_millis(shared.settings.flush_ms);

    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(TICK) {
        // Somebody wanting the pairing is the one thing that ends this early.
        if last_yield_check.elapsed() >= YIELD_CHECK {
            last_yield_check = Instant::now();
            if yield_asked() {
                let _ = notes.send(Note::Yield);
                break;
            }
        }

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
                    // A new link is a new conversation with every phone, so
                    // none of them counts as watching until it says so.
                    lock(&shared.watching).clear();
                    shared.up.store(true, Ordering::Release);
                    outbox.insert(0, hello(shared.sessions.as_ref()));
                    outbox.push(FromDesk::Sessions {
                        list: shared.sessions.list(),
                    });
                }
                Event::Frame(raw) => {
                    outbox.extend(answer(&raw, &keys, &mut openers, &shared, &notes));
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
                for session in std::mem::take(&mut batch.order) {
                    if let Some(evs) = batch.events.remove(&session)
                        && !evs.is_empty()
                    {
                        outbox.push(FromDesk::Events { session, evs });
                    }
                }
                batch.bytes = 0;
                batch.urgent = false;
                last_flush = Instant::now();
            }
        }

        // A desktop with nothing to say says so anyway, now and then. The
        // relay judges whether one is still there by when it last heard from
        // it, and protocol pings are answered by the runtime without the hub
        // ever waking — so a quiet desktop that never did this could be pushed
        // off its own pairing by anybody holding the code.
        if shared.up.load(Ordering::Acquire) && last_heard.elapsed() >= KEEPALIVE {
            last_heard = Instant::now();
            link.send(serde_json::to_string(&Envelope::keepalive()).unwrap_or_default());
        }

        if outbox.is_empty() {
            continue;
        }
        if !shared.up.load(Ordering::Acquire) {
            // Nowhere to put them yet. Keep what fits and drop the oldest: a
            // phone that missed the middle of a turn is told there is a gap,
            // which is better than a window that grows without end.
            trim_outbox(&mut outbox, shared.settings.outbox_bytes);
            continue;
        }
        for payload in outbox.drain(..) {
            let plain = serde_json::to_vec(&payload).unwrap_or_default();
            let (seq, ct) = seal.seal(&plain);
            link.send(
                serde_json::to_string(&Envelope::publish(&hex(&id), seq, ct)).unwrap_or_default(),
            );
            // A frame is as good as a keepalive, and better: it is the thing
            // the keepalive stands in for.
            last_heard = Instant::now();
        }
    }

    // Whatever brought the loop to an end — the stop channel, or a window
    // asking for the pairing — nothing is draining the batch after this.
    shared.done.store(true, Ordering::Release);

    // On the way out, say so: a phone that knows the desktop left stops
    // waiting for it.
    if shared.up.load(Ordering::Acquire) {
        let plain = serde_json::to_vec(&FromDesk::Bye { reason: Bye::Quit }).unwrap_or_default();
        let (seq, ct) = seal.seal(&plain);
        link.send(
            serde_json::to_string(&Envelope::publish(&hex(&id), seq, ct)).unwrap_or_default(),
        );
        // Long enough for the socket thread to take it off the queue.
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

/// What a phone asked for, and what it gets back.
fn answer(
    raw: &str,
    keys: &Keys,
    openers: &mut HashMap<String, Opener>,
    shared: &Shared,
    notes: &mpsc::Sender<Note>,
) -> Vec<FromDesk> {
    let Ok(Envelope::Cmd {
        link,
        plink,
        seq,
        ct,
        ..
    }) = serde_json::from_str::<Envelope>(raw)
    else {
        return Vec::new();
    };
    let (Some(id), Some(phone)) = (unhex(&link), unhex(&plink)) else {
        return Vec::new();
    };
    // Kept only once a frame has actually opened. An entry made on the way in
    // would mean anything able to reach this socket could leave one behind per
    // made-up link id, which is a list this process holds and nothing trims.
    let plain = match openers.get_mut(&plink) {
        Some(opener) => match opener.open(seq, &ct) {
            Ok(plain) => plain.to_vec(),
            Err(_) => return Vec::new(),
        },
        None => {
            if openers.len() >= MAX_OPENERS {
                // Only real phones ever get in here, and there are never many
                // of them; the oldest key makes room for a phone that
                // reconnected with a new link.
                if let Some(oldest) = openers.keys().next().cloned() {
                    openers.remove(&oldest);
                }
            }
            let mut opener = Opener::new(keys.link_key(Dir::P2d, &id, &phone), Dir::P2d, id, phone);
            let Ok(plain) = opener.open(seq, &ct) else {
                return Vec::new();
            };
            let plain = plain.to_vec();
            openers.insert(plink.clone(), opener);
            plain
        }
    };
    let Ok(asked) = serde_json::from_slice::<FromPhone>(&plain) else {
        return Vec::new();
    };

    // A code holder is allowed to drive this machine; a code holder in a loop
    // is not allowed to drive it thousands of times a second. This does not
    // make the code safer to lose — nothing here does — it keeps a mistake or
    // a runaway from costing a day's model spend before anybody notices.
    if !shared.allowance.take() {
        return Vec::new();
    }

    let device = device_name(&plink);
    let ack = |error: Option<String>| {
        vec![FromDesk::Ack {
            cmd_seq: seq,
            ok: error.is_none(),
            session: None,
            error,
        }]
    };

    // Reading is answered here: it is the same answer whoever owns the
    // sessions, and none of it changes anything.
    let (session, act) = match asked {
        FromPhone::List => {
            return vec![FromDesk::Sessions {
                list: shared.sessions.list(),
            }];
        }
        FromPhone::Attach { session, .. } => {
            if lock(&shared.watching).insert(device.clone()) && shared.settings.notice {
                let _ = notes.send(Note::Attached(device));
            }
            let Some(state) = shared.sessions.state(&session) else {
                return ack(Some("no such session".into()));
            };
            let (messages, truncated) = snapshot(&session, shared.settings.snapshot_messages);
            return vec![
                FromDesk::State(state),
                FromDesk::Snapshot {
                    session,
                    messages,
                    truncated,
                },
            ];
        }
        FromPhone::Detach { .. } => {
            if lock(&shared.watching).remove(&device) && shared.settings.notice {
                let _ = notes.send(Note::Detached(device));
            }
            return Vec::new();
        }
        FromPhone::GetBlob { session, path } => {
            return blob(&session, &path, shared.settings.max_frame_bytes);
        }

        FromPhone::Submit {
            session,
            text,
            images,
        } => {
            // An image entry is normally a path, and a path is read off this
            // disk and sent to the model. From a phone that would be an
            // arbitrary file read with nobody asked about it — `file:///etc/…`
            // is a path like any other — so over the wire only the bytes
            // themselves are taken.
            if let Some(why) = unreadable_image(&images) {
                return ack(Some(why));
            }
            (session, Act::Submit { text, images })
        }
        FromPhone::Interrupt { session } => (session, Act::Interrupt),
        FromPhone::Compact { session, focus } => (session, Act::Compact(focus)),
        FromPhone::Clear { session } => (session, Act::Clear),
        // A name is drawn in the status line and in the session list, both of
        // which are a terminal.
        FromPhone::Rename { session, name } => (session, Act::Rename(sessions::printable(&name))),
        FromPhone::Resume { session } => (session, Act::Resume),
        FromPhone::NewSession { cwd, model, prompt } => {
            (String::new(), Act::Start { cwd, model, prompt })
        }

        // An answer is only an answer to the question that is on screen.
        // Anything else arrived too late, and sending it on would apply it to
        // whatever came next.
        FromPhone::AnswerPermission { session, id, allow } => {
            let Some(asked) = asking(shared, &session, id) else {
                return ack(Some("that question has been answered".into()));
            };
            lock(&shared.answered_by).insert((asked.clone(), id), device);
            (asked, Act::AllowTool(allow))
        }
        FromPhone::AnswerAsk { session, id, reply } => {
            let Some(asked) = asking(shared, &session, id) else {
                return ack(Some("that question has been answered".into()));
            };
            lock(&shared.answered_by).insert((asked.clone(), id), device);
            (asked, Act::Answer(reply))
        }
    };

    let said = act.said();
    match shared.sessions.act(&session, act) {
        Ok(started) => {
            if let Some(text) = said
                && shared.settings.notice
            {
                let _ = notes.send(Note::Said(text));
            }
            // A session that did not exist a moment ago is named on the ack
            // itself. Working it out from the list that follows means
            // comparing against whatever the phone last saw, which is one
            // stale list away from opening the wrong session.
            let mut out = vec![FromDesk::Ack {
                cmd_seq: seq,
                ok: true,
                session: started.clone(),
                error: None,
            }];
            if started.is_some() {
                out.push(FromDesk::Sessions {
                    list: shared.sessions.list(),
                });
            }
            out
        }
        Err(why) => ack(Some(why)),
    }
}

/// Why a phone's images cannot be taken, or `None` when they can.
///
/// A phone has the picture; it does not have this filesystem. So it sends the
/// bytes and nothing else is accepted — not a path, not a `file://` URL, not
/// the `ah-image:` form the session file uses. Anything else would be this
/// machine reading a file of a remote peer's choosing and posting it to the
/// model, which is not something a tool prompt would even be shown for.
fn unreadable_image(images: &[String]) -> Option<String> {
    if images.len() > MAX_IMAGES {
        return Some(format!("more than {MAX_IMAGES} images in one message"));
    }
    let total: usize = images.iter().map(String::len).sum();
    if total > MAX_IMAGE_CHARS {
        return Some("those images are too big to send in one message".into());
    }
    images
        .iter()
        .any(|i| !ah_core::image::is_data_url(i))
        .then(|| "an image sent from a phone has to be the picture itself, not a path to one on this machine".to_string())
}

/// Whether this is an answer to a question that is actually on screen, and if
/// so which session asked it.
///
/// One outstanding question per session, not one per machine. The daemon runs
/// several at once and their prompt numbers start again from one in each, so a
/// single slot meant two sessions both asking "id 1": one answer was refused
/// and the other cleared the slot, leaving a session waiting on an answer that
/// could no longer be given.
///
/// A phone that has not attached to anything names no session. That is only
/// unambiguous when exactly one session is asking; when two are, it is
/// refused, because guessing would answer the wrong one.
fn asking(shared: &Shared, session: &str, id: u64) -> Option<String> {
    let pending = lock(&shared.pending);
    if !session.is_empty() {
        return (pending.get(session) == Some(&id)).then(|| session.to_string());
    }
    let mut asking = pending.iter().filter(|(_, pending)| **pending == id);
    let only = asking.next()?;
    asking.next().is_none().then(|| only.0.clone())
}

/// Which phone this was, as far as anything here knows: the first characters
/// of the link it seals under. Nothing a person named, and nothing that
/// follows it between connections.
fn device_name(plink: &str) -> String {
    format!("phone {}", &plink[..plink.len().min(8)])
}

/// A file a session produced, in pieces small enough to travel.
///
/// Only what that session drew, and only by a path that is still inside the
/// directory it draws into once every `..` in it has been resolved: the path
/// came from a phone, and a phone is not this machine.
fn blob(session: &str, path: &str, chunk: usize) -> Vec<FromDesk> {
    let deny = |why: &str| {
        vec![FromDesk::Ack {
            cmd_seq: 0,
            ok: false,
            session: None,
            error: Some(why.to_string()),
        }]
    };
    let Ok(dir) = ah_core::paths::session_images_dir(session).canonicalize() else {
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

/// The tail of a conversation, read from the session file rather than kept in
/// memory: it is the same text, and it survives this process restarting.
fn snapshot(session: &str, want: usize) -> (Vec<ah_core::abi::Message>, bool) {
    let Ok(session) = ah_core::session::Session::open(session) else {
        return (Vec::new(), false);
    };
    let total = session.messages.len();
    let from = total.saturating_sub(want);
    (session.messages[from..].to_vec(), from > 0)
}

fn hello(sessions: &dyn Sessions) -> FromDesk {
    FromDesk::Hello(Hello {
        host: hostname(),
        os: std::env::consts::OS.to_string(),
        ah_version: env!("CARGO_PKG_VERSION").to_string(),
        proto: PROTO,
        holder: holder().into(),
        roots: sessions.roots(),
    })
}

/// Whether a window or the daemon is publishing, which is the only thing a
/// phone can use to tell one from the other.
fn holder() -> &'static str {
    if std::env::var_os("AH_REMOTE_DAEMON").is_some() {
        "daemon"
    } else {
        "tui"
    }
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
mod questions {
    use super::*;

    fn shared() -> Shared {
        Shared {
            settings: Default::default(),
            batch: Mutex::new(Batch::default()),
            sessions: Arc::new(crate::remote::window::Window::new(
                crate::remote::window::Reach {
                    cmd: mpsc::channel().0,
                    perm: mpsc::channel().0,
                    ask: mpsc::channel().0,
                    cancel: Arc::new(AtomicBool::new(false)),
                    inbox: Arc::new(crate::app::Inbox::default()),
                },
                Default::default(),
            )),
            up: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            done: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            answered_by: Mutex::new(HashMap::new()),
            last_state: Mutex::new(None),
            watching: Mutex::new(std::collections::HashSet::new()),
            allowance: Allowance::new(),
        }
    }

    #[test]
    fn two_sessions_asking_at_once_do_not_answer_each_other() {
        // Prompt numbers start again in every engine, so both of these are
        // question 1 and neither is the other.
        let s = shared();
        lock(&s.pending).insert("alpha".into(), 1);
        lock(&s.pending).insert("beta".into(), 1);

        assert_eq!(asking(&s, "alpha", 1).as_deref(), Some("alpha"));
        assert_eq!(asking(&s, "beta", 1).as_deref(), Some("beta"));
        // Answering alpha leaves beta still waiting, rather than clearing it.
        lock(&s.pending).remove("alpha");
        assert!(asking(&s, "alpha", 1).is_none());
        assert_eq!(asking(&s, "beta", 1).as_deref(), Some("beta"));
    }

    #[test]
    fn a_phone_that_named_no_session_is_only_obeyed_when_there_is_no_doubt() {
        let s = shared();
        lock(&s.pending).insert("alpha".into(), 1);
        assert_eq!(asking(&s, "", 1).as_deref(), Some("alpha"));
        // A second session asking the same number makes the empty name
        // ambiguous, and a guess would answer the wrong one.
        lock(&s.pending).insert("beta".into(), 1);
        assert!(asking(&s, "", 1).is_none());
    }

    #[test]
    fn an_answer_to_a_question_that_is_gone_is_refused() {
        let s = shared();
        lock(&s.pending).insert("alpha".into(), 7);
        assert!(asking(&s, "alpha", 6).is_none());
        assert!(asking(&s, "somebody-else", 7).is_none());
    }
}

#[cfg(test)]
mod allowance {
    use super::*;

    #[test]
    fn a_burst_goes_through_and_a_loop_does_not() {
        let a = Allowance::new();
        for i in 0..Allowance::BURST as usize {
            assert!(a.take(), "refused command {i} of an ordinary burst");
        }
        assert!(!a.take(), "a loop kept going past the burst");
    }

    #[test]
    fn it_fills_back_up() {
        let a = Allowance::new();
        while a.take() {}
        // Pretend a second went by, which is what the clock would do here.
        {
            let mut state = lock(&a.left);
            state.1 = Instant::now() - Duration::from_secs(2);
        }
        assert!(a.take(), "it never came back");
    }
}

#[cfg(test)]
mod images {
    use super::*;

    #[test]
    fn a_phone_may_send_a_picture_but_not_a_path_to_one() {
        let png = "data:image/png;base64,iVBORw0KGgo=".to_string();
        assert!(unreadable_image(std::slice::from_ref(&png)).is_none());

        // Every one of these would be this machine reading its own disk on a
        // remote peer\'s say-so.
        for path in [
            "/etc/shadow",
            "file:///etc/shadow",
            "file:///dev/zero",
            "ah-image:whatever.png",
            "~/.ssh/id_ed25519",
        ] {
            assert!(
                unreadable_image(&[path.to_string()]).is_some(),
                "{path} was accepted"
            );
        }
        // And one bad entry spoils the message rather than being dropped
        // quietly, which would change what was sent without saying so.
        assert!(unreadable_image(&[png, "/etc/shadow".into()]).is_some());
    }

    #[test]
    fn there_is_a_limit_to_how_much_one_message_may_carry() {
        let big = format!("data:image/png;base64,{}", "A".repeat(MAX_IMAGE_CHARS));
        assert!(unreadable_image(&[big]).is_some());
        let many: Vec<String> = (0..MAX_IMAGES + 1)
            .map(|_| "data:image/png;base64,AA==".to_string())
            .collect();
        assert!(unreadable_image(&many).is_some());
    }
}

#[cfg(test)]
mod files {
    use super::*;

    /// A session with one drawing in it, and a data directory of its own.
    fn drawn(dir: &std::path::Path) -> String {
        // SAFETY: every test that moves this variable holds the same guard.
        unsafe { std::env::set_var("AH_DATA_DIR", dir) };
        let session = String::from("abc123");
        let images = ah_core::paths::session_images_dir(&session);
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join("drawing.png"), vec![7u8; 3000]).unwrap();
        session
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
        let session = drawn(&dir);
        let path = ah_core::paths::session_images_dir(&session).join("drawing.png");

        let out = blob(&session, path.to_str().unwrap(), 1024);
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
        let session = drawn(&dir);
        let images = ah_core::paths::session_images_dir(&session);

        // The path comes from a phone, so it is not taken at its word. Every
        // one of these resolves outside the session's own directory.
        for attempt in [
            "/etc/passwd".to_string(),
            images.join("../../../../etc/passwd").display().to_string(),
            images.join("..").display().to_string(),
        ] {
            assert!(
                refused(&blob(&session, &attempt, 1024)).is_some(),
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
        let session = drawn(&dir);
        let path = ah_core::paths::session_images_dir(&session).join("nothing.png");
        assert_eq!(
            refused(&blob(&session, path.to_str().unwrap(), 1024)).as_deref(),
            Some("no such file")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
