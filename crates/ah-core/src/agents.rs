//! Subagents: agent loops the model starts to do a piece of work on its own.
//!
//! A child is a whole loop of its own — its own conversation, model, tools and
//! working directory — running on one thread and handing back one report. The
//! parent shares its provider, so a child costs a thread and a socket from the
//! same pool, not a second process.
//!
//! Nothing here polls. A caller waiting for one child blocks on that child's
//! condition variable; a caller waiting for the first of several blocks on the
//! table's, which every finish bumps. An idle child does not exist: a child is
//! either running or done.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ah_abi::{AgentDef, AgentSettings, Favorite, Message, Role, Settings, Usage, merge_patch};
use serde_json::{Value, json};

use crate::agent::{Agent, AgentEvent, AgentIo, Mailbox, NoHooks};
use crate::provider::Provider;
use crate::tools::Registry;

/// Who has already been told that a child finished: the agent it belongs to,
/// or the screen showing that agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    Model(u32),
    /// The screen, for that agent's own view: the main conversation is 0.
    Ui(u32),
}

impl Audience {
    fn bit(self) -> u32 {
        match self {
            Audience::Model(_) => 1,
            Audience::Ui(_) => 2,
        }
    }

    fn covers(self, child: &Child) -> bool {
        match self {
            Audience::Model(parent) | Audience::Ui(parent) => child.parent == parent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Waiting for a slot; `agents.max_concurrent` are running already.
    Queued,
    Running,
    /// Said what it had to say.
    Done,
    /// Stopped early: the request limit, the context budget, or an error.
    Stopped,
    Cancelled,
}

impl State {
    pub fn over(self) -> bool {
        !matches!(self, State::Queued | State::Running)
    }

    fn word(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Done => "done",
            State::Stopped => "stopped",
            State::Cancelled => "cancelled",
        }
    }
}

/// Everything a child needs to run, kept so a follow-up can run it again.
struct Launch {
    provider: Arc<dyn Provider>,
    model: String,
    settings: Settings,
    /// The same settings as JSON, for a child of its own to patch in turn.
    settings_value: Value,
    cwd: PathBuf,
    session_id: String,
    max_requests: u32,
    max_context_bytes: u64,
    report_bytes: usize,
    log_lines: usize,
    stack_bytes: usize,
    window: u64,
}

pub struct Child {
    pub id: u32,
    /// The agent type it was started as.
    pub kind: String,
    pub task: String,
    pub model: String,
    /// Agent that started it: 0 is the one the user talks to.
    pub parent: u32,
    pub depth: u16,
    pub started: Instant,
    cancel: Arc<AtomicBool>,
    state: Mutex<State>,
    done: Condvar,
    /// Bumped on every change, so a view can tell without locking.
    version: AtomicU64,
    reported: AtomicU32,
    requests: AtomicU32,
    tool_calls: AtomicU32,
    duration_ms: AtomicU64,
    usage: Mutex<Usage>,
    /// What it is doing right now, one line.
    activity: Mutex<String>,
    /// The last lines of what it did, for the agent view.
    log: Mutex<VecDeque<String>>,
    /// What it said at the end, capped.
    report: Mutex<String>,
    /// Everything it did, as it happened, so a screen can show its work the
    /// way the main conversation is shown. Capped by bytes; the oldest go.
    events: Mutex<Events>,
    /// Said to it while it runs, read before its next request.
    inbox: Mutex<Vec<String>>,
    /// Kept so a follow-up continues instead of starting over.
    messages: Mutex<Vec<Message>>,
    launch: Arc<Launch>,
}

impl Child {
    pub fn state(&self) -> State {
        *self.state.lock().unwrap()
    }

    pub fn running(&self) -> bool {
        !self.state().over()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn requests(&self) -> u32 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn tool_calls(&self) -> u32 {
        self.tool_calls.load(Ordering::Relaxed)
    }

    pub fn usage(&self) -> Usage {
        *self.usage.lock().unwrap()
    }

    pub fn duration(&self) -> Duration {
        match self.state().over() {
            false => self.started.elapsed(),
            true => Duration::from_millis(self.duration_ms.load(Ordering::Relaxed)),
        }
    }

    pub fn activity(&self) -> String {
        self.activity.lock().unwrap().clone()
    }

    pub fn report(&self) -> String {
        self.report.lock().unwrap().clone()
    }

    /// What it has done since event `cursor`, and the number to ask from next
    /// time. `0` is everything still kept.
    pub fn events_since(&self, cursor: u64) -> (Vec<AgentEvent>, u64) {
        let events = self.events.lock().unwrap();
        let out: Vec<AgentEvent> = events
            .items
            .iter()
            .filter(|(n, _)| *n > cursor)
            .map(|(_, ev)| ev.clone())
            .collect();
        (out, events.next - 1)
    }

    fn record(&self, ev: AgentEvent) {
        self.events.lock().unwrap().push(ev);
    }

    /// The last `max` lines of what it has done.
    pub fn log(&self, max: usize) -> Vec<String> {
        let log = self.log.lock().unwrap();
        log.iter()
            .skip(log.len().saturating_sub(max))
            .cloned()
            .collect()
    }

    /// Ask it to stop. It notices between requests, before its next tool, or
    /// within a tick of the stream it is reading.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.note("stopping".to_string());
    }

    /// Something to read before its next request. The user typing at a child
    /// and the model's `agents say` both land here.
    pub fn say(&self, text: impl Into<String>) {
        let text = text.into();
        self.note(format!("told: {}", text.lines().next().unwrap_or("")));
        self.inbox.lock().unwrap().push(text);
        self.bump();
    }

    /// Block until it finishes or `timeout` passes. True when it finished.
    pub fn wait(&self, timeout: Duration) -> bool {
        let state = self.state.lock().unwrap();
        let (_state, r) = self
            .done
            .wait_timeout_while(state, timeout, |s| !s.over())
            .unwrap();
        !r.timed_out()
    }

    /// Wait, looking at `cancel` every 40 ms.
    pub fn wait_while(&self, timeout: Duration, cancel: &AtomicBool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            if self.wait(left.min(Duration::from_millis(40))) {
                return true;
            }
        }
    }

    /// One line for a list: what it is, how it is doing, what it was asked.
    pub fn summary(&self) -> String {
        let u = self.usage();
        let secs = self.duration().as_secs_f32();
        format!(
            "agent {} ({}) {} {:.0}s · {} requests · {} tools · ${:.4}",
            self.id,
            self.kind,
            self.state().word(),
            secs,
            self.requests(),
            self.tool_calls(),
            u.cost
        )
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
        table().wake();
    }

    /// Add a line to what this agent has done. Its own steps land here, and so
    /// does anything the user or the agent above it says to it: an agent's view
    /// is the only place that news belongs.
    pub fn note(&self, line: String) {
        {
            let mut log = self.log.lock().unwrap();
            log.push_back(line.clone());
            let cap = self.launch.log_lines.max(1);
            while log.len() > cap {
                log.pop_front();
            }
        }
        *self.activity.lock().unwrap() = line;
        self.bump();
    }

    /// True the first time this audience is told it finished.
    fn claim(&self, who: Audience) -> bool {
        self.state().over() && self.reported.fetch_or(who.bit(), Ordering::SeqCst) & who.bit() == 0
    }

    fn finish(&self, state: State) {
        self.duration_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
        *self.state.lock().unwrap() = state;
        self.version.fetch_add(1, Ordering::Relaxed);
        self.done.notify_all();
    }
}

impl Mailbox for Child {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.inbox.lock().unwrap())
    }
}

/// A child's own transcript, in the order it happened. Each event is numbered,
/// so a screen that has seen the first `n` can ask for the rest.
struct Events {
    items: VecDeque<(u64, AgentEvent)>,
    next: u64,
    bytes: usize,
    cap: usize,
}

impl Events {
    fn new(cap: usize) -> Self {
        Self {
            items: VecDeque::new(),
            next: 1,
            bytes: 0,
            cap: cap.max(4096),
        }
    }

    fn push(&mut self, ev: AgentEvent) {
        let size = weight(&ev);
        self.items.push_back((self.next, ev));
        self.next += 1;
        self.bytes += size;
        while self.bytes > self.cap && self.items.len() > 1 {
            if let Some((_, old)) = self.items.pop_front() {
                self.bytes = self.bytes.saturating_sub(weight(&old));
            }
        }
    }
}

/// Roughly what an event costs to keep.
fn weight(ev: &AgentEvent) -> usize {
    match ev {
        AgentEvent::Text(t) | AgentEvent::Reasoning(t) | AgentEvent::Notice(t) => t.len() + 16,
        AgentEvent::AssistantMessage(m) => {
            m.content.len() + m.reasoning.as_deref().map_or(0, str::len) + 16
        }
        AgentEvent::ToolStart(c) => c.function.name.len() + c.function.arguments.len() + 16,
        AgentEvent::ToolEnd { call, result, .. } => {
            call.function.arguments.len() + result.output.len() + 32
        }
        _ => 32,
    }
}

type Waker = Box<dyn Fn() + Send + Sync>;

pub struct Agents {
    kids: Mutex<Vec<Arc<Child>>>,
    next_id: AtomicU32,
    /// Bumped by every finish; what a caller waiting for the first of several
    /// blocks on.
    finished: (Mutex<u64>, Condvar),
    waker: Mutex<Option<Waker>>,
    pending: AtomicBool,
    /// What children have spent since the caller last folded it in.
    spent: Mutex<Usage>,
    /// Children started in this process, ever. Caps are counted on this rather
    /// than on the table, which forgets the old ones.
    started: AtomicU32,
    /// Children an agent is blocked on right now. The screen says "waiting for
    /// agents" on the strength of this, so it counts what a caller is actually
    /// stuck behind, not what merely happens to be running.
    waiting: AtomicU32,
}

/// A caller waiting on children, counted while it waits. Dropping it stops
/// the count, whether the wait ended, timed out or was cancelled.
pub struct Waiting {
    table: &'static Agents,
    n: u32,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.table.waiting.fetch_sub(self.n, Ordering::Relaxed);
    }
}

/// The table for this process. Children belong to the run, so there is one.
pub fn table() -> &'static Agents {
    static AGENTS: OnceLock<Agents> = OnceLock::new();
    AGENTS.get_or_init(|| Agents {
        kids: Mutex::new(Vec::new()),
        next_id: AtomicU32::new(1),
        finished: (Mutex::new(0), Condvar::new()),
        waker: Mutex::new(None),
        pending: AtomicBool::new(false),
        spent: Mutex::new(Usage::default()),
        started: AtomicU32::new(0),
        waiting: AtomicU32::new(0),
    })
}

impl Agents {
    /// How many children somebody is blocked on. Zero while the agents run in
    /// the background and nobody is waiting.
    pub fn waiting(&self) -> u32 {
        self.waiting.load(Ordering::Relaxed)
    }

    /// Count `n` children as waited on until the guard is dropped.
    pub fn waiting_on(&'static self, n: u32) -> Waiting {
        self.waiting.fetch_add(n, Ordering::Relaxed);
        Waiting { table: self, n }
    }

    pub fn get(&self, id: u32) -> Option<Arc<Child>> {
        self.kids
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.id == id)
            .cloned()
    }

    /// The child as far as one agent is concerned: another agent's child is
    /// not there at all.
    pub fn get_owned(&self, id: u32, parent: u32) -> Option<Arc<Child>> {
        self.get(id).filter(|c| c.parent == parent)
    }

    pub fn all(&self) -> Vec<Arc<Child>> {
        self.kids.lock().unwrap().clone()
    }

    pub fn owned_by(&self, parent: u32) -> Vec<Arc<Child>> {
        self.kids
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.parent == parent)
            .cloned()
            .collect()
    }

    pub fn running(&self) -> usize {
        self.kids
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.running())
            .count()
    }

    fn running_now(&self) -> usize {
        self.kids
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.state() == State::Running)
            .count()
    }

    /// One line per child that finished since this audience last asked.
    pub fn notices(&self, who: Audience) -> Vec<String> {
        self.kids
            .lock()
            .unwrap()
            .iter()
            .filter(|c| who.covers(c) && c.claim(who))
            .map(|c| c.summary())
            .collect()
    }

    /// True when a child has finished that this audience has not been told
    /// about, without claiming the news.
    pub fn unheard(&self, who: Audience) -> bool {
        self.kids.lock().unwrap().iter().any(|c| {
            who.covers(c) && c.state().over() && c.reported.load(Ordering::Relaxed) & who.bit() == 0
        })
    }

    /// Usage the children have run up, taken away as it is read: the caller
    /// folds it into the session's total.
    pub fn take_spent(&self) -> Usage {
        std::mem::take(&mut *self.spent.lock().unwrap())
    }

    pub fn set_waker(&self, waker: Waker) {
        *self.waker.lock().unwrap() = Some(waker);
    }

    fn wake(&self) {
        if self.pending.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(w) = self.waker.lock().unwrap().as_ref() {
            w();
        }
    }

    /// The watcher is about to read; the next change wakes it again.
    pub fn caught_up(&self) {
        self.pending.store(false, Ordering::SeqCst);
    }

    /// Block until one of `ids` finishes — or, with `all`, until every one of
    /// them has. True when the wait was satisfied rather than timed out.
    pub fn wait_any(&self, ids: &[u32], all: bool, timeout: Duration, cancel: &AtomicBool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let over = ids
                .iter()
                .filter(|id| self.get(**id).is_none_or(|c| c.state().over()))
                .count();
            if (all && over == ids.len()) || (!all && over > 0) {
                return true;
            }
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let guard = self.finished.0.lock().unwrap();
            let seen = *guard;
            let _ = self.finished.1.wait_timeout_while(
                guard,
                left.min(Duration::from_millis(40)),
                |n| *n == seen,
            );
        }
    }

    /// Stop every child, wait for what stops in time, and let the rest go.
    pub fn shutdown(&self, grace: Duration) {
        let kids = self.all();
        for c in &kids {
            c.cancel();
        }
        let deadline = Instant::now() + grace;
        for c in &kids {
            let left = deadline.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                c.wait(left);
            }
        }
    }

    /// Drop the oldest finished children once there are too many. A child is
    /// only dropped once the model it belongs to has heard about it.
    fn evict(&self, kids: &mut Vec<Arc<Child>>, keep: usize) {
        let over_count = kids.iter().filter(|c| c.state().over()).count();
        if over_count <= keep {
            return;
        }
        let mut over = over_count - keep;
        let heard = Audience::Model(0).bit();
        kids.retain(|c| {
            if over > 0 && c.state().over() && c.reported.load(Ordering::Relaxed) & heard != 0 {
                over -= 1;
                return false;
            }
            true
        });
    }

    fn announce_finish(&self) {
        {
            let mut n = self.finished.0.lock().unwrap();
            *n += 1;
        }
        self.finished.1.notify_all();
        self.wake();
    }

    /// Start whatever is queued and has room to run.
    fn start_queued(&self, max_concurrent: u32) {
        loop {
            if self.running_now() as u32 >= max_concurrent.max(1) {
                return;
            }
            let next = self
                .kids
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.state() == State::Queued)
                .cloned();
            match next {
                Some(c) => run(&c, max_concurrent),
                None => return,
            }
        }
    }
}

/// Put a child on a thread of its own. Queued children go through here too,
/// once one ahead of them has finished.
fn run(child: &Arc<Child>, max_concurrent: u32) {
    *child.state.lock().unwrap() = State::Running;
    child.bump();
    let c = child.clone();
    let mut builder = std::thread::Builder::new().name(format!("ah-agent-{}", child.id));
    if child.launch.stack_bytes > 0 {
        builder = builder.stack_size(child.launch.stack_bytes);
    }
    let spawned = builder.spawn(move || {
        let state = turn(&c);
        // A command of its own that is still running is handed back, so its
        // news reaches somebody once the child is gone.
        crate::jobs::table().reparent_all(c.id, c.parent);
        c.finish(state);
        table().announce_finish();
        table().start_queued(max_concurrent);
    });
    if spawned.is_err() {
        child.finish(State::Stopped);
        *child.report.lock().unwrap() = "could not start a thread for this agent".into();
        table().announce_finish();
    }
}

/// One run of the loop for a child: its whole life, or one follow-up.
fn turn(child: &Arc<Child>) -> State {
    let l = child.launch.clone();
    let mut registry = Registry::builtins(&l.settings.tools);
    // An agent may start agents of its own when the depth allows it, with a
    // spawner that counts from where it stands.
    let spawner: Option<Arc<dyn Spawner + Sync>> = l.settings.agents.enabled.then(|| {
        Arc::new(AgentSpawner::new(
            l.provider.clone(),
            l.settings.clone(),
            l.settings_value.clone(),
            l.cwd.clone(),
            l.session_id.clone(),
            child.id,
            child.depth,
        )) as Arc<dyn Spawner + Sync>
    });
    if let Some(s) = spawner.as_ref() {
        install_tools(&mut registry, &l.settings, s.types());
    }
    let mut hooks = NoHooks;
    let io = ChildIo(child.clone());
    let mut agent = Agent::new(
        &*l.provider,
        &registry,
        &mut hooks,
        &l.settings,
        l.cwd.clone(),
        &child.cancel,
    );
    agent.agent_id = child.id;
    agent.session_id = l.session_id.clone();
    agent.max_requests = l.max_requests;
    agent.context_window = l.window;
    agent.mailbox = Some(child.clone());
    agent.spawner = spawner;

    let mut messages = std::mem::take(&mut *child.messages.lock().unwrap());
    let result = agent.run_turn(&mut messages, &io);

    let report = last_word(&messages).unwrap_or_else(|| child.log(20).join("\n"));
    *child.report.lock().unwrap() = crate::tools::truncate(&report, l.report_bytes);
    let too_big = crate::agent::messages_tokens(&messages) * 4 > l.max_context_bytes;
    *child.messages.lock().unwrap() = messages;

    match result {
        _ if child.cancel.load(Ordering::Relaxed) => State::Cancelled,
        Ok(s) if s.cancelled => State::Cancelled,
        Ok(_) if too_big => State::Stopped,
        Ok(_) => State::Done,
        Err(e) => {
            child.note(format!("error: {e}"));
            State::Stopped
        }
    }
}

/// The last thing the child said in its own words.
fn last_word(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !m.content.trim().is_empty())
        .map(|m| m.content.trim().to_string())
}

/// What a child reports to. Nothing streams out of a child: the parent gets
/// the report, the screen gets a line per step, and neither is asked anything.
struct ChildIo(Arc<Child>);

impl AgentIo for ChildIo {
    fn emit(&self, ev: AgentEvent) {
        let c = &self.0;
        match &ev {
            AgentEvent::RequestStart { turn } => {
                c.requests.store(*turn, Ordering::Relaxed);
                c.bump();
            }
            AgentEvent::ToolStart(call) => {
                let line = crate::tools::describe::describe(&call.function.name, &{
                    serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null)
                })
                .unwrap_or_else(|| call.function.name.clone());
                c.tool_calls.fetch_add(1, Ordering::Relaxed);
                c.note(line);
            }
            AgentEvent::Usage(u) => {
                c.usage.lock().unwrap().add(u);
                table().spent.lock().unwrap().add(u);
                c.bump();
            }
            AgentEvent::Error(e) => c.note(format!("error: {e}")),
            AgentEvent::Notice(n) => c.note(n.clone()),
            _ => {}
        }
        // Kept whole, so a screen can show the agent's work the way it shows
        // the conversation: the same blocks, from the same events.
        match ev {
            AgentEvent::Usage(_) | AgentEvent::ToolMessage(_) | AgentEvent::SettingsPatch(_) => {}
            ev => {
                c.record(ev);
                c.bump();
            }
        }
    }

    /// A child never interrupts the user: what it may do was settled when its
    /// type was written.
    fn ask_permission(&self, _call: &ah_abi::ToolCall, _reason: &str) -> bool {
        false
    }
}

/// What the `agent` tool asks for.
pub struct SpawnRequest {
    pub kind: String,
    pub task: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
}

/// How a tool reaches the machinery. The loop hands one of these to its tools
/// the way it hands them the way to ask the user.
pub trait Spawner: Sync {
    fn spawn(&self, req: SpawnRequest) -> Result<Arc<Child>, String>;
    /// Agent types and what they are for, for the tool description.
    fn types(&self) -> Vec<(String, String)>;
    fn limits(&self) -> &AgentSettings;
    /// Which agent is doing the spawning.
    fn parent(&self) -> u32;
}

/// The real one, built once per turn by whoever owns the provider.
pub struct AgentSpawner {
    provider: Arc<dyn Provider>,
    settings: Settings,
    settings_value: Value,
    cwd: PathBuf,
    session_id: String,
    parent: u32,
    depth: u16,
    /// Context window per model, looked up once.
    windows: Mutex<Vec<(String, u64)>>,
}

impl AgentSpawner {
    pub fn new(
        provider: Arc<dyn Provider>,
        settings: Settings,
        settings_value: Value,
        cwd: PathBuf,
        session_id: String,
        parent: u32,
        depth: u16,
    ) -> Self {
        Self {
            provider,
            settings,
            settings_value,
            cwd,
            session_id,
            parent,
            depth,
            windows: Mutex::new(Vec::new()),
        }
    }

    fn window(&self, model: &str) -> u64 {
        if self.settings.context.window > 0 {
            return self.settings.context.window;
        }
        let mut cache = self.windows.lock().unwrap();
        if let Some((_, w)) = cache.iter().find(|(m, _)| m == model) {
            return *w;
        }
        let w = crate::models::context_window(model).unwrap_or(0);
        cache.push((model.to_string(), w));
        w
    }

    /// The working directory for a child: its own, but never outside the
    /// parent's.
    fn child_cwd(&self, asked: Option<&str>) -> Result<PathBuf, String> {
        let Some(rel) = asked.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(self.cwd.clone());
        };
        let path = crate::tools::resolve_path(&self.cwd, rel);
        let full = path.canonicalize().map_err(|e| format!("{rel}: {e}"))?;
        let root = self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone());
        if !full.starts_with(&root) {
            return Err(format!("{rel} is outside the working directory"));
        }
        Ok(full)
    }
}

impl Spawner for AgentSpawner {
    fn parent(&self) -> u32 {
        self.parent
    }

    fn limits(&self) -> &AgentSettings {
        &self.settings.agents
    }

    fn types(&self) -> Vec<(String, String)> {
        types_of(&self.settings)
    }

    fn spawn(&self, req: SpawnRequest) -> Result<Arc<Child>, String> {
        let cfg = &self.settings.agents;
        if !cfg.enabled {
            return Err("subagents are turned off".into());
        }
        if self.depth >= cfg.max_depth {
            return Err("an agent this deep cannot start agents of its own".into());
        }
        let t = table();
        if t.started.load(Ordering::Relaxed) >= cfg.max_total {
            return Err(format!(
                "this session has already started {} agents; do the rest yourself",
                cfg.max_total
            ));
        }
        let kind = if req.kind.trim().is_empty() {
            "default".to_string()
        } else {
            req.kind.trim().to_string()
        };
        let fallback = AgentDef::default();
        let def = match cfg.defs.get(&kind) {
            Some(d) => d,
            None if kind == "default" => &fallback,
            None => {
                let known: Vec<String> = self.types().into_iter().map(|(n, _)| n).collect();
                return Err(format!(
                    "no agent type {kind:?}; there is {}",
                    known.join(", ")
                ));
            }
        };
        let model = req
            .model
            .filter(|_| cfg.allow_model_arg)
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        let settings = child_settings(
            &self.settings_value,
            def,
            cfg,
            &self.settings,
            self.depth + 1,
            model.as_deref(),
        )?;
        let cwd = self.child_cwd(req.cwd.as_deref())?;
        let window = self.window(&settings.model.id);

        let id = t.next_id.fetch_add(1, Ordering::Relaxed);
        let settings_value = serde_json::to_value(&settings).unwrap_or(Value::Null);
        let launch = Launch {
            provider: self.provider.clone(),
            model: settings.model.id.clone(),
            settings_value,
            session_id: format!("{}/a{id}", self.session_id),
            max_requests: pick(def.max_requests, cfg.max_requests),
            max_context_bytes: cfg.max_context_bytes,
            report_bytes: cfg.report_bytes,
            log_lines: cfg.log_lines,
            stack_bytes: cfg.stack_bytes,
            window,
            settings,
            cwd,
        };
        let child = Arc::new(Child {
            id,
            kind,
            task: req.task.clone(),
            model: launch.model.clone(),
            parent: self.parent,
            depth: self.depth + 1,
            started: Instant::now(),
            cancel: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(State::Queued),
            done: Condvar::new(),
            version: AtomicU64::new(0),
            reported: AtomicU32::new(0),
            requests: AtomicU32::new(0),
            tool_calls: AtomicU32::new(0),
            duration_ms: AtomicU64::new(0),
            usage: Mutex::new(Usage::default()),
            activity: Mutex::new(String::new()),
            log: Mutex::new(VecDeque::new()),
            report: Mutex::new(String::new()),
            events: Mutex::new(Events::new(cfg.view_bytes)),
            inbox: Mutex::new(Vec::new()),
            messages: Mutex::new(vec![Message::user(req.task)]),
            launch: Arc::new(launch),
        });
        t.started.fetch_add(1, Ordering::Relaxed);
        {
            let mut kids = t.kids.lock().unwrap();
            kids.push(child.clone());
            t.evict(&mut kids, cfg.keep);
        }
        t.start_queued(cfg.max_concurrent);
        Ok(child)
    }
}

/// The agent types on offer, with what each is for. There is always a
/// `default`, whether or not the settings describe one.
pub fn types_of(settings: &Settings) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = settings
        .agents
        .defs
        .iter()
        .map(|(name, def)| (name.clone(), def.description.clone()))
        .collect();
    if !out.iter().any(|(n, _)| n == "default") {
        out.insert(
            0,
            (
                "default".to_string(),
                "Does the task with the tools every agent gets.".to_string(),
            ),
        );
    }
    out
}

/// Put the two agent tools in a registry, if the settings offer them. The
/// descriptions carry the agent types, so they are built here rather than
/// among the built-ins.
pub fn install_tools(registry: &mut Registry, settings: &Settings, types: Vec<(String, String)>) {
    let cfg = &settings.agents;
    if !cfg.enabled {
        return;
    }
    let offered = |name: &str| {
        settings.tools.enabled.iter().any(|t| t == name)
            && !settings.tools.disabled.iter().any(|t| t == name)
    };
    if offered("agent") {
        registry.register(Box::new(crate::tools::agent::AgentTool {
            types: crate::tools::agent::Types(types),
            allow_model_arg: cfg.allow_model_arg,
            max_spawn: cfg.max_spawn.max(1),
            timeout_ms: cfg.timeout_ms,
        }));
    }
    if offered("agents") {
        registry.register(Box::new(crate::tools::agent::AgentsTool {
            kill_grace_ms: cfg.kill_grace_ms,
            timeout_ms: cfg.timeout_ms,
            max_concurrent: cfg.max_concurrent,
        }));
    }
}

/// Ask a finished child to carry on, with something new to go on. Its
/// conversation is still there, so it keeps what it learned.
pub fn follow_up(child: &Arc<Child>, text: &str, max_concurrent: u32) -> Result<(), String> {
    if child.running() {
        child.say(text);
        return Ok(());
    }
    child.cancel.store(false, Ordering::Relaxed);
    child.messages.lock().unwrap().push(Message::user(text));
    *child.state.lock().unwrap() = State::Queued;
    child.reported.store(0, Ordering::SeqCst);
    table().start_queued(max_concurrent);
    Ok(())
}

fn pick(chosen: u32, fallback: u32) -> u32 {
    if chosen > 0 { chosen } else { fallback }
}

/// The settings a child runs under: the session's, with the type's choices
/// over the top. What it may do is decided here, not while it runs.
pub fn child_settings(
    parent_value: &Value,
    def: &AgentDef,
    cfg: &AgentSettings,
    parent: &Settings,
    depth: u16,
    model_override: Option<&str>,
) -> Result<Settings, String> {
    let mut v = parent_value.clone();
    let mut patch = json!({
        "prompt": {"docs_hint": false},
        "context": {"auto_compact": false, "plan_reminder": false},
        "agents": {"enabled": depth < cfg.max_depth},
    });

    let wanted = model_override
        .map(str::to_string)
        .or_else(|| non_empty(&def.model))
        .or_else(|| non_empty(&cfg.model));
    let effort = non_empty(&def.effort).or_else(|| non_empty(&cfg.effort));
    if let Some(name) = wanted {
        let (id, fav_effort) = resolve_model(&name, &parent.model.favorites);
        patch["model"] = json!({"id": id});
        if let Some(e) = effort.clone().or(fav_effort) {
            patch["model"]["reasoning"] = effort_patch(&e);
        }
    } else if let Some(e) = effort {
        patch["model"] = json!({"reasoning": effort_patch(&e)});
    }

    if let Some(prompt) = non_empty(&def.prompt) {
        patch["prompt"]["system"] = json!(prompt);
    }

    let mut tools = if def.tools.is_empty() {
        cfg.tools.clone()
    } else {
        def.tools.clone()
    };
    // The plan belongs to the session and a child has nobody to ask, so those
    // two are never a child's to call. Nor is starting agents past the limit.
    let mut barred = vec!["plan".to_string(), "ask_user".to_string()];
    if depth >= cfg.max_depth {
        barred.push("agent".into());
        barred.push("agents".into());
    }
    if parent.permissions.mode == ah_abi::PermissionMode::Ask {
        // A tool the user wanted to approve is not one a child can use: it
        // would only ever come back refused.
        barred.extend(parent.permissions.ask_for.iter().cloned());
    }
    tools.retain(|t| !barred.contains(t));
    patch["tools"] = json!({
        "enabled": tools,
        "max_output_bytes": parent.tools.max_output_bytes.min(16 * 1024),
        "job_buffer_bytes": parent.tools.job_buffer_bytes.min(64 * 1024),
    });

    merge_patch(&mut v, &patch);
    if !def.settings.is_null() {
        // An agent type is a settings patch from wherever the settings came
        // from, which includes a repository. It may say which model to use and
        // what to put in the prompt; it may not hand its own children a shell,
        // turn the asking off, or reach root — all of which the bars above and
        // the parent's own permissions have just decided.
        let mut theirs = def.settings.clone();
        for key in crate::settings::GUARDED {
            crate::settings::take(&mut theirs, key);
        }
        merge_patch(&mut v, &theirs);
        // And whatever it said about tools, the bars still hold: a child
        // cannot be given back a tool the parent would have had to approve.
        merge_patch(&mut v, &patch);
    }
    serde_json::from_value(v).map_err(|e| format!("agent type settings: {e}"))
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn effort_patch(level: &str) -> Value {
    if level == "off" {
        Value::Null
    } else {
        json!({"effort": level})
    }
}

/// A favorite name, or a model id as given.
fn resolve_model(
    name: &str,
    favorites: &std::collections::BTreeMap<String, Favorite>,
) -> (String, Option<String>) {
    match favorites.get(name) {
        Some(f) => (f.id().to_string(), f.effort().map(str::to_string)),
        None => (name.to_string(), None),
    }
}

/// The table is process-wide and its slots are shared, so only one test at a
/// time may start children and count them.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::MockProvider;
    use crate::provider::{OnEvent, StreamEvent};
    use ah_abi::{ChatRequest, Usage};

    fn settings() -> Settings {
        let mut s = Settings::default();
        s.agents.max_total = 1000;
        s.model.id = "test/model".into();
        s
    }

    fn spawner_with(provider: Arc<dyn Provider>, settings: Settings) -> AgentSpawner {
        let value = serde_json::to_value(&settings).unwrap();
        AgentSpawner::new(
            provider,
            settings,
            value,
            std::env::current_dir().unwrap(),
            "sess".into(),
            0,
            0,
        )
    }

    fn says(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::Text(text.to_string()),
            StreamEvent::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                cost: 0.5,
                ..Default::default()
            }),
        ]
    }

    /// Answers everything the same way, and remembers how many answers were in
    /// flight at once.
    struct Together {
        now: Mutex<u32>,
        most: Mutex<u32>,
        hold: Duration,
    }

    impl Provider for Together {
        fn name(&self) -> &str {
            "together"
        }
        fn stream(
            &self,
            _req: &ChatRequest,
            _cancel: &AtomicBool,
            on_event: OnEvent<'_>,
        ) -> crate::Result<()> {
            {
                let mut now = self.now.lock().unwrap();
                *now += 1;
                let mut most = self.most.lock().unwrap();
                *most = (*most).max(*now);
            }
            std::thread::sleep(self.hold);
            *self.now.lock().unwrap() -= 1;
            on_event(StreamEvent::Text("done".into()));
            Ok(())
        }
    }

    /// Never answers, so the child is still going when it is stopped.
    struct Silent;

    impl Provider for Silent {
        fn name(&self) -> &str {
            "silent"
        }
        fn stream(
            &self,
            _req: &ChatRequest,
            cancel: &AtomicBool,
            _on: OnEvent<'_>,
        ) -> crate::Result<()> {
            for _ in 0..200 {
                if cancel.load(Ordering::Relaxed) {
                    return Err(crate::Error::Cancelled);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        }
    }

    #[test]
    fn a_child_does_the_work_and_says_what_it_found() {
        let _guard = test_lock();
        table().take_spent();
        let provider = Arc::new(MockProvider::new(vec![says("the parser is in sse.rs")]));
        let s = spawner_with(provider, settings());
        let child = s
            .spawn(SpawnRequest {
                kind: String::new(),
                task: "find the parser".into(),
                cwd: None,
                model: None,
            })
            .unwrap();
        assert!(
            child.wait(Duration::from_secs(5)),
            "the child never finished"
        );
        assert_eq!(child.state(), State::Done);
        assert_eq!(child.report(), "the parser is in sse.rs");
        assert_eq!(child.requests(), 1);
        assert_eq!(table().take_spent().cost, 0.5);
    }

    #[test]
    fn several_children_run_at_the_same_time() {
        let _guard = test_lock();
        let provider = Arc::new(Together {
            now: Mutex::new(0),
            most: Mutex::new(0),
            hold: Duration::from_millis(120),
        });
        let s = spawner_with(provider.clone(), settings());
        let ids: Vec<u32> = (0..3)
            .map(|n| {
                s.spawn(SpawnRequest {
                    kind: String::new(),
                    task: format!("task {n}"),
                    cwd: None,
                    model: None,
                })
                .unwrap()
                .id
            })
            .collect();
        let started = Instant::now();
        assert!(table().wait_any(&ids, true, Duration::from_secs(5), crate::tools::never()));
        assert!(
            started.elapsed() < Duration::from_millis(360),
            "they ran one after another"
        );
        assert_eq!(*provider.most.lock().unwrap(), 3);
    }

    #[test]
    fn only_so_many_run_at_once() {
        let _guard = test_lock();
        let provider = Arc::new(Together {
            now: Mutex::new(0),
            most: Mutex::new(0),
            hold: Duration::from_millis(60),
        });
        let mut set = settings();
        set.agents.max_concurrent = 1;
        let s = spawner_with(provider.clone(), set);
        let ids: Vec<u32> = (0..3)
            .map(|n| {
                s.spawn(SpawnRequest {
                    kind: String::new(),
                    task: format!("task {n}"),
                    cwd: None,
                    model: None,
                })
                .unwrap()
                .id
            })
            .collect();
        assert!(table().wait_any(&ids, true, Duration::from_secs(10), crate::tools::never()));
        assert_eq!(*provider.most.lock().unwrap(), 1);
    }

    #[test]
    fn a_child_that_is_stopped_says_so() {
        let _guard = test_lock();
        let s = spawner_with(Arc::new(Silent), settings());
        let child = s
            .spawn(SpawnRequest {
                kind: String::new(),
                task: "wait forever".into(),
                cwd: None,
                model: None,
            })
            .unwrap();
        // Let it get as far as its first request before stopping it.
        assert!(!child.wait(Duration::from_millis(100)));
        child.cancel();
        assert!(child.wait(Duration::from_secs(5)), "cancel did not stop it");
        assert_eq!(child.state(), State::Cancelled);
    }

    #[test]
    fn waiting_counts_only_while_somebody_waits() {
        let _guard = test_lock();
        assert_eq!(table().waiting(), 0);
        {
            let _held = table().waiting_on(2);
            assert_eq!(table().waiting(), 2);
        }
        assert_eq!(table().waiting(), 0);
    }

    #[test]
    fn what_a_child_is_told_shows_in_its_own_log() {
        let _guard = test_lock();
        let provider = Arc::new(MockProvider::new(vec![says("ok")]));
        let s = spawner_with(provider, settings());
        let child = s
            .spawn(SpawnRequest {
                kind: String::new(),
                task: "wait".into(),
                cwd: None,
                model: None,
            })
            .unwrap();
        child.say("look at sse.rs too");
        assert!(child.wait(Duration::from_secs(5)));
        assert!(
            child
                .log(50)
                .iter()
                .any(|l| l.contains("look at sse.rs too")),
            "{:?}",
            child.log(50)
        );
    }

    #[test]
    fn a_long_report_is_cut_down() {
        let _guard = test_lock();
        let long = "x".repeat(5000);
        let provider = Arc::new(MockProvider::new(vec![says(&long)]));
        let mut set = settings();
        set.agents.report_bytes = 500;
        let s = spawner_with(provider, set);
        let child = s
            .spawn(SpawnRequest {
                kind: String::new(),
                task: "say a lot".into(),
                cwd: None,
                model: None,
            })
            .unwrap();
        assert!(child.wait(Duration::from_secs(5)));
        let report = child.report();
        assert!(report.len() < 700, "{} bytes", report.len());
        assert!(report.contains("truncated"), "{report}");
    }

    #[test]
    fn a_child_has_no_plan_no_questions_and_no_children_of_its_own() {
        let parent = settings();
        let value = serde_json::to_value(&parent).unwrap();
        let child = child_settings(
            &value,
            &AgentDef::default(),
            &parent.agents,
            &parent,
            1,
            None,
        )
        .unwrap();
        assert!(!child.tools.enabled.contains(&"plan".to_string()));
        assert!(!child.tools.enabled.contains(&"ask_user".to_string()));
        assert!(!child.tools.enabled.contains(&"agent".to_string()));
        assert!(child.tools.enabled.contains(&"read_file".to_string()));
        assert!(!child.context.auto_compact);
        assert!(!child.prompt.docs_hint);
        // What the parent may never run, a child may never run either.
        assert_eq!(child.permissions.deny, parent.permissions.deny);
    }

    #[test]
    fn a_type_takes_its_own_model_and_the_rest_inherit_the_session() {
        let mut parent = settings();
        parent.model.favorites.insert(
            "fast".into(),
            Favorite::Full {
                id: "cheap/model".into(),
                effort: Some("low".into()),
            },
        );
        let value = serde_json::to_value(&parent).unwrap();

        let mut def = AgentDef {
            model: "fast".into(),
            ..Default::default()
        };
        let child = child_settings(&value, &def, &parent.agents, &parent, 1, None).unwrap();
        assert_eq!(child.model.id, "cheap/model");
        assert_eq!(child.model.effort(), Some("low"));

        def.model = String::new();
        let child = child_settings(&value, &def, &parent.agents, &parent, 1, None).unwrap();
        assert_eq!(child.model.id, parent.model.id);
    }

    #[test]
    fn a_type_that_asks_for_tools_the_user_gates_loses_them() {
        let mut parent = settings();
        parent.permissions.mode = ah_abi::PermissionMode::Ask;
        let value = serde_json::to_value(&parent).unwrap();
        let def = AgentDef {
            tools: vec!["read_file".into(), "bash".into(), "write_file".into()],
            ..Default::default()
        };
        let child = child_settings(&value, &def, &parent.agents, &parent, 1, None).unwrap();
        assert_eq!(child.tools.enabled, vec!["read_file".to_string()]);
    }
}
