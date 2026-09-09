//! Shared runtime: settings, plugins, provider, and the engine owning the conversation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use ah_core::abi::*;
use ah_core::agent::{Agent, AgentEvent, AgentIo, Hooks, NoHooks, TurnSummary};
use ah_core::agents::{AgentSpawner, Spawner};
use ah_core::plugins::{LoadReport, PluginHost};
use ah_core::provider::Provider;
use ah_core::provider::openrouter::OpenRouter;
use ah_core::session::Session;
use ah_core::settings::{Origin, SettingsStack};
use ah_core::tools::Registry;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::Overrides;

pub type AnyError = Box<dyn std::error::Error>;

/// Turn `--set a.b=c` into a nested JSON patch. Values parse as JSON when they
/// can, otherwise as strings.
pub fn set_to_patch(spec: &str) -> Result<Value, AnyError> {
    let (key, raw) = spec
        .split_once('=')
        .ok_or_else(|| format!("--set expects KEY=VALUE, got `{spec}`"))?;
    let val = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
    let mut v = val;
    for part in key.split('.').rev() {
        v = serde_json::json!({ part: v });
    }
    Ok(v)
}

pub fn overrides_patch(o: &Overrides) -> Result<Value, AnyError> {
    let mut patch = serde_json::json!({});
    if let Some(m) = &o.model {
        merge_patch(&mut patch, &serde_json::json!({"model": {"id": m}}));
    }
    if o.yolo {
        merge_patch(
            &mut patch,
            &serde_json::json!({"permissions": {"mode": "auto"}}),
        );
    }
    if o.ask {
        merge_patch(
            &mut patch,
            &serde_json::json!({"permissions": {"mode": "ask"}}),
        );
    }
    if o.no_plugins {
        merge_patch(
            &mut patch,
            &serde_json::json!({"plugins": {"enabled": false}}),
        );
    }
    if !o.plugins.is_empty() {
        let paths: Vec<String> = o.plugins.iter().map(|p| p.display().to_string()).collect();
        merge_patch(
            &mut patch,
            &serde_json::json!({"plugins": {"paths": paths}}),
        );
    }
    if let Some(s) = &o.system {
        merge_patch(&mut patch, &serde_json::json!({"prompt": {"append": s}}));
    }
    if let Some(mt) = o.max_tokens {
        merge_patch(
            &mut patch,
            &serde_json::json!({"model": {"max_tokens": mt}}),
        );
    }
    for s in &o.sets {
        merge_patch(&mut patch, &set_to_patch(s)?);
    }
    Ok(patch)
}

/// Settings from files plus CLI overrides. Plugin layers are added by `Engine::load_plugins`.
pub fn load_settings(o: &Overrides) -> Result<SettingsStack, AnyError> {
    let mut stack = SettingsStack::from_files()?;
    stack.push(Origin::Cli, overrides_patch(o)?)?;
    Ok(stack)
}

pub fn resolve_cwd(o: &Overrides) -> Result<PathBuf, AnyError> {
    Ok(match &o.cwd {
        Some(d) => {
            std::env::set_current_dir(d)?;
            d.canonicalize()?
        }
        None => std::env::current_dir()?,
    })
}

/// Commands the UI thread sends to the engine thread.
pub enum EngineCmd {
    Submit {
        text: String,
        /// `data:` URLs attached to the message.
        images: Vec<String>,
    },
    Slash {
        name: String,
        args: String,
        stage: SlashStage,
    },
    Statusline(Box<StatusContext>),
    /// Fully merged settings after the UI applied new layers.
    Settings(Box<Settings>, Value),
    /// Drop and reload all plugins with the current settings.
    Reload,
    Clear,
    /// Switch to a stored session by id.
    Resume(String),
    Rename(String),
    /// Summarise the conversation now; the string is optional focus text.
    Compact(String),
    /// A background job ended; let the model see it and react.
    Wake,
    Quit,
}

/// Events the engine thread sends to the UI thread.
pub enum UiEvent {
    Agent(AgentEvent),
    PluginsLoaded {
        reports: Vec<LoadReport>,
        /// `(plugin name, patch)` in apply order: manifest patches then `on_load`.
        patches: Vec<(String, Value)>,
        commands: Vec<(String, SlashCommandSpec)>,
    },
    PluginLogs(Vec<(String, LogLevel, String)>),
    Statusline(Box<StatuslineOut>),
    /// Result of a plugin slash command: output, command name, stage.
    Slash(Box<SlashCommandOut>, String, SlashStage),
    AskPermission {
        call: ToolCall,
        reason: String,
    },
    /// The `ask_user` tool wants an answer. The turn is stopped until one is
    /// sent back down the answer channel.
    AskUser(Box<Ask>),
    Busy(bool),
    Resumed {
        id: String,
        name: Option<String>,
        messages: Vec<Message>,
    },
    Renamed(Option<String>),
}

/// `(reports, settings patches in apply order, slash commands)` from a plugin load.
pub type PluginLoad = (
    Vec<LoadReport>,
    Vec<(String, Value)>,
    Vec<(String, SlashCommandSpec)>,
);

/// Provider, tools, plugins and session. Runs one turn at a time.
pub struct Engine {
    pub provider: Option<Arc<dyn Provider>>,
    pub registry: Registry,
    pub host: Option<PluginHost>,
    pub settings: Settings,
    pub settings_value: Value,
    pub cwd: PathBuf,
    pub session: Session,
    pub cancel: Arc<AtomicBool>,
    pub total_usage: Usage,
    /// Conversation size as of the last response.
    pub context_tokens: u64,
    /// `(model id, window)` looked up in the catalogue. Only a real window is
    /// kept: a miss must be retried, since the catalogue may still be on its way.
    window: Option<(String, u64)>,
    /// Whether this session has already been told its window is unknown.
    warned_no_window: bool,
}

impl Engine {
    pub fn new(
        stack: &SettingsStack,
        cwd: PathBuf,
        resume: Option<&str>,
        persist: bool,
    ) -> Result<Self, AnyError> {
        let settings = stack.settings().clone();
        let session = match resume {
            Some("") => match Session::latest() {
                Some(id) => Session::open(&id)?,
                None => return Err("no sessions to resume".into()),
            },
            Some(what) => match ah_core::session::find(what) {
                Some(id) => Session::open(&id)?,
                None => return Err(format!("no session named or with id {what:?}").into()),
            },
            None if persist => Session::create(&cwd.display().to_string(), &settings.model.id)?,
            None => Session::ephemeral(),
        };
        let mut e = Self {
            provider: None,
            registry: Registry::new(),
            host: None,
            settings,
            settings_value: stack.value().clone(),
            cwd,
            session,
            cancel: Arc::new(AtomicBool::new(false)),
            total_usage: Usage::default(),
            context_tokens: 0,
            window: None,
            warned_no_window: false,
        };
        e.rebuild_provider();
        e.rebuild_registry();
        // A resumed conversation is already using the window, but no reply has
        // reported its size yet, so estimate it rather than start from zero.
        e.context_tokens = ah_core::agent::messages_tokens(&e.session.messages);
        // The plan belongs to the session, so resuming one resumes its tasks.
        ah_core::plan::store().load(e.session.plan.clone());
        Ok(e)
    }

    /// The built-in tools, plus the two that start and mind subagents. Those
    /// two name the configured agent types in their description, so they are
    /// added here rather than among the built-ins.
    pub fn rebuild_registry(&mut self) {
        self.registry = Registry::builtins(&self.settings.tools);
        let types = ah_core::agents::types_of(&self.settings);
        ah_core::agents::install_tools(&mut self.registry, &self.settings, types);
    }

    pub fn rebuild_provider(&mut self) {
        let key = ah_core::auth::api_key(self.settings.model.api_key.as_deref());
        self.provider = key.map(|k| {
            Arc::new(OpenRouter::new(self.settings.model.base_url.clone(), k)) as Arc<dyn Provider>
        });
    }

    pub fn has_key(&self) -> bool {
        self.provider.is_some()
    }

    /// Context window of the current model: `context.window` if set, else the
    /// cached catalogue, else 0.
    pub fn context_window(&mut self) -> u64 {
        if self.settings.context.window > 0 {
            return self.settings.context.window;
        }
        let id = &self.settings.model.id;
        if let Some((m, w)) = &self.window
            && m == id
        {
            return *w;
        }
        let w = ah_core::models::context_window(id).unwrap_or(0);
        // A miss is deliberately not remembered. The catalogue is fetched in
        // the background and lands in a file this reads, so remembering a zero
        // would leave auto compaction off for the rest of the process and let
        // the conversation grow until the provider refuses it.
        if w > 0 {
            self.window = Some((id.clone(), w));
        }
        w
    }

    /// Say once that the window is unknown. Auto compaction is off until it is
    /// known, and nothing else on screen would show that.
    fn warn_if_no_window(&mut self, window: u64, io: &dyn AgentIo) {
        if window > 0 || self.warned_no_window || !self.settings.context.auto_compact {
            return;
        }
        self.warned_no_window = true;
        io.emit(AgentEvent::Notice(format!(
            "the model catalogue does not give a context window for {}, so the conversation \
             will not be compacted on its own. Set context.window to a size to turn it back on.",
            self.settings.model.id
        )));
    }

    /// Agent over this engine's provider, registry, hooks and settings.
    /// `None` without an API key.
    fn agent<'a>(&'a mut self, no_hooks: &'a mut NoHooks, window: u64) -> Option<Agent<'a>> {
        let Self {
            provider,
            registry,
            host,
            settings,
            cwd,
            cancel,
            context_tokens,
            session,
            settings_value,
            ..
        } = self;
        let shared = provider.clone()?;
        let borrowed = provider.as_deref()?;
        let hooks: &mut dyn Hooks = match host.as_mut() {
            Some(h) => h,
            None => no_hooks,
        };
        // Subagents share this provider, so they share its connections too.
        let spawner: Option<Arc<dyn Spawner + Sync>> = settings.agents.enabled.then(|| {
            Arc::new(AgentSpawner::new(
                shared.clone(),
                settings.clone(),
                settings_value.clone(),
                cwd.clone(),
                session.id.clone(),
                0,
                0,
            )) as Arc<dyn Spawner + Sync>
        });
        let mut a = Agent::new(borrowed, registry, hooks, settings, cwd.clone(), cancel);
        a.session_id = session.id.clone();
        a.context_window = window;
        a.context_tokens = *context_tokens;
        a.spawner = spawner;
        Some(a)
    }

    /// (Re)load plugins. Returns reports and the patches the UI must layer in.
    pub fn load_plugins(&mut self) -> PluginLoad {
        self.host = None;
        let cwd = self.cwd.display().to_string();
        let mut host = PluginHost::load(&self.settings, &self.settings_value, &cwd);
        let mut patches = host.manifest_patches();
        // `on_load` sees settings with the manifest patches already applied.
        let mut preview = self.settings_value.clone();
        for (_, p) in &patches {
            merge_patch(&mut preview, p);
        }
        let preview_settings: Settings =
            serde_json::from_value(preview.clone()).unwrap_or_else(|_| self.settings.clone());
        host.update_settings(&preview);
        patches.extend(host.on_load(&preview_settings, &cwd));
        let reports = host.reports.clone();
        let commands = host.commands();
        self.host = Some(host);
        (reports, patches, commands)
    }

    pub fn apply_settings(&mut self, settings: Settings, value: Value) {
        let model_changed = settings.model.id != self.settings.model.id
            || settings.model.base_url != self.settings.model.base_url
            || settings.model.api_key != self.settings.model.api_key;
        let tools_changed =
            settings.tools != self.settings.tools || settings.agents != self.settings.agents;
        self.settings = settings;
        self.settings_value = value;
        if let Some(h) = self.host.as_mut() {
            h.update_settings(&self.settings_value);
        }
        if model_changed {
            self.rebuild_provider();
            self.warned_no_window = false;
        }
        if tools_changed {
            self.rebuild_registry();
        }
    }

    pub fn take_plugin_logs(&mut self) -> Vec<(String, LogLevel, String)> {
        self.host
            .as_mut()
            .map(|h| h.take_logs())
            .unwrap_or_default()
    }

    /// Run a full user turn. Blocks until the model stops calling tools.
    pub fn run_turn(
        &mut self,
        text: String,
        images: Vec<String>,
        io: &dyn AgentIo,
    ) -> Result<TurnSummary, ah_core::Error> {
        self.turn(Some(Message::user_with_images(text, images)), io)
    }

    /// Let the model act on something that happened while it was idle — a
    /// background job that ended. The loop picks the news up on its own; no
    /// message of ours goes into the conversation.
    pub fn wake(&mut self, io: &dyn AgentIo) -> Result<TurnSummary, ah_core::Error> {
        self.turn(None, io)
    }

    fn turn(
        &mut self,
        message: Option<Message>,
        io: &dyn AgentIo,
    ) -> Result<TurnSummary, ah_core::Error> {
        let window = self.context_window();
        self.warn_if_no_window(window, io);
        self.cancel.store(false, Ordering::Relaxed);
        let mut messages = std::mem::take(&mut self.session.messages);
        let mut no_hooks = NoHooks;
        let Some(mut agent) = self.agent(&mut no_hooks, window) else {
            self.session.messages = messages;
            return Err(no_key(io));
        };
        // Compact before the new message so it survives verbatim.
        if agent.over_threshold()
            && let Err(e) = agent.auto_compact(&mut messages, io)
        {
            io.emit(AgentEvent::Notice(format!("compaction failed: {e}")));
        }
        let before = messages.len();
        messages.extend(message);
        let res = agent.run_turn(&mut messages, io);
        let compacted = agent.compactions > 0;
        let context_tokens = agent.context_tokens;
        self.context_tokens = context_tokens;
        if compacted {
            self.session.reset(messages);
        } else {
            let appended: Vec<Message> = messages.drain(before..).collect();
            self.session.messages = messages;
            for m in appended {
                self.session.push(m);
            }
        }
        if let Ok(s) = &res {
            self.total_usage.add(&s.usage);
        }
        // Subagents spend on their own account; fold it in so the status line
        // and /usage count the whole turn, not just this loop.
        self.total_usage.add(&ah_core::agents::table().take_spent());
        self.session.save_plan(&ah_core::plan::store().snapshot());
        res
    }

    /// `/compact`: summarise now, regardless of size.
    pub fn compact(&mut self, focus: &str, io: &dyn AgentIo) -> Result<(), ah_core::Error> {
        let window = self.context_window();
        if self.session.messages.is_empty() {
            io.emit(AgentEvent::Notice("nothing to compact".into()));
            return Ok(());
        }
        self.cancel.store(false, Ordering::Relaxed);
        let mut messages = std::mem::take(&mut self.session.messages);
        let mut no_hooks = NoHooks;
        let Some(mut agent) = self.agent(&mut no_hooks, window) else {
            self.session.messages = messages;
            return Err(no_key(io));
        };
        let res = agent.compact(&mut messages, focus, io);
        let context_tokens = agent.context_tokens;
        self.context_tokens = context_tokens;
        if res.is_ok() {
            self.session.reset(messages);
        } else {
            self.session.messages = messages;
        }
        match &res {
            // Esc during a compaction leaves the conversation alone; say so
            // quietly instead of as a failure.
            Err(ah_core::Error::Cancelled) => {
                io.emit(AgentEvent::Notice("compaction cancelled".into()))
            }
            Err(e) => io.emit(AgentEvent::Error(format!("compaction failed: {e}"))),
            Ok(()) => {}
        }
        res
    }

    pub fn statusline(&mut self, ctx: &StatusContext) -> Option<StatuslineOut> {
        self.host.as_mut().and_then(|h| h.statusline(ctx))
    }

    pub fn slash(&mut self, name: &str, args: &str, stage: SlashStage) -> Option<SlashCommandOut> {
        let cwd = self.cwd.display().to_string();
        self.host
            .as_mut()
            .and_then(|h| h.slash_command(name, args, &cwd, stage))
    }

    /// Engine thread main loop for the TUI.
    pub fn serve(
        mut self,
        rx: Receiver<EngineCmd>,
        tx: Sender<UiEvent>,
        perm_rx: Receiver<bool>,
        ask_rx: Receiver<Reply>,
    ) {
        let cancel = self.cancel.clone();
        let io = ChannelIo::new(tx.clone(), perm_rx, ask_rx, cancel);
        while let Ok(cmd) = rx.recv() {
            match cmd {
                EngineCmd::Submit { text, images } => {
                    let _ = tx.send(UiEvent::Busy(true));
                    let _ = self.run_turn(text, images, &io);
                    let logs = self.take_plugin_logs();
                    if !logs.is_empty() {
                        let _ = tx.send(UiEvent::PluginLogs(logs));
                    }
                    let _ = tx.send(UiEvent::Busy(false));
                }
                EngineCmd::Wake => {
                    if ah_core::jobs::table().unheard(ah_core::jobs::Audience::Model(0))
                        || ah_core::agents::table().unheard(ah_core::agents::Audience::Model(0))
                    {
                        let _ = tx.send(UiEvent::Busy(true));
                        let _ = self.wake(&io);
                        let _ = tx.send(UiEvent::Busy(false));
                    }
                }
                EngineCmd::Slash { name, args, stage } => {
                    let out = match self.slash(&name, &args, stage) {
                        Some(out) => out,
                        None if stage == SlashStage::Run => SlashCommandOut {
                            message: Some(format!("unknown command /{name} (try /help)")),
                            ..Default::default()
                        },
                        // No answer to a preview or pick still ends the picker cleanly.
                        None => SlashCommandOut::default(),
                    };
                    let _ = tx.send(UiEvent::Slash(Box::new(out), name, stage));
                }
                EngineCmd::Statusline(ctx) => {
                    if let Some(s) = self.statusline(&ctx) {
                        let _ = tx.send(UiEvent::Statusline(Box::new(s)));
                    }
                }
                EngineCmd::Settings(s, v) => self.apply_settings(*s, v),
                EngineCmd::Reload => {
                    self.rebuild_provider();
                    let (reports, patches, commands) = self.load_plugins();
                    let _ = tx.send(UiEvent::PluginsLoaded {
                        reports,
                        patches,
                        commands,
                    });
                    let logs = self.take_plugin_logs();
                    if !logs.is_empty() {
                        let _ = tx.send(UiEvent::PluginLogs(logs));
                    }
                }
                EngineCmd::Compact(focus) => {
                    let _ = tx.send(UiEvent::Busy(true));
                    let _ = self.compact(&focus, &io);
                    let _ = tx.send(UiEvent::Busy(false));
                }
                EngineCmd::Clear => {
                    self.session.clear();
                    ah_core::plan::store().clear();
                    self.context_tokens = 0;
                }
                EngineCmd::Resume(id) => match Session::open(&id) {
                    Ok(s) => {
                        self.session = s;
                        ah_core::plan::store().load(self.session.plan.clone());
                        self.total_usage = Usage::default();
                        self.context_tokens = 0;
                        let _ = tx.send(UiEvent::Resumed {
                            id: self.session.id.clone(),
                            name: self.session.name.clone(),
                            messages: self.session.messages.clone(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiEvent::Agent(AgentEvent::Error(format!(
                            "resume {id}: {e}"
                        ))));
                    }
                },
                EngineCmd::Rename(name) => {
                    self.session.rename(&name);
                    let _ = tx.send(UiEvent::Renamed(self.session.name.clone()));
                }
                EngineCmd::Quit => break,
            }
        }
    }
}

/// `AgentIo` over channels, used when the engine runs on its own thread.
///
/// The answer channels sit behind locks because the loop shares the driver
/// with tool calls that may run side by side; only one of them ever waits
/// here, and only while a prompt is on screen.
pub struct ChannelIo {
    pub tx: Sender<UiEvent>,
    perm_rx: Mutex<Receiver<bool>>,
    ask_rx: Mutex<Receiver<Reply>>,
    pub cancel: Arc<AtomicBool>,
}

impl ChannelIo {
    pub fn new(
        tx: Sender<UiEvent>,
        perm_rx: Receiver<bool>,
        ask_rx: Receiver<Reply>,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tx,
            perm_rx: Mutex::new(perm_rx),
            ask_rx: Mutex::new(ask_rx),
            cancel,
        }
    }
}

impl AgentIo for ChannelIo {
    fn emit(&self, ev: AgentEvent) {
        let _ = self.tx.send(UiEvent::Agent(ev));
    }
    fn ask_permission(&self, call: &ToolCall, reason: &str) -> bool {
        let rx = lock(&self.perm_rx);
        // Anything left from an earlier prompt goes before this one is asked;
        // draining afterwards would race the answer to this one and eat it.
        while rx.try_recv().is_ok() {}
        if self
            .tx
            .send(UiEvent::AskPermission {
                call: call.clone(),
                reason: reason.into(),
            })
            .is_err()
        {
            return false;
        }
        match rx.recv() {
            Ok(v) => v && !self.cancel.load(Ordering::Relaxed),
            Err(_) => false,
        }
    }
    fn ask_user(&self, ask: &Ask) -> Reply {
        let rx = lock(&self.ask_rx);
        while rx.try_recv().is_ok() {}
        if self
            .tx
            .send(UiEvent::AskUser(Box::new(ask.clone())))
            .is_err()
        {
            return Reply::Unavailable;
        }
        match rx.recv() {
            Ok(_) if self.cancel.load(Ordering::Relaxed) => Reply::Dismissed,
            Ok(r) => r,
            Err(_) => Reply::Unavailable,
        }
    }
}

/// A poisoned answer channel still holds a working receiver: the panic that
/// poisoned it was not in the queue.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn no_key(io: &dyn AgentIo) -> ah_core::Error {
    let msg = "no API key: run `ah login`, or set OPENROUTER_API_KEY".to_string();
    io.emit(AgentEvent::Error(msg.clone()));
    ah_core::Error::Auth(msg)
}

/// `42%` of the window when it is known, else the token count.
pub fn context_label(tokens: u64, window: u64) -> String {
    match tokens.saturating_mul(100).checked_div(window) {
        Some(0) if tokens > 0 => "<1%".to_string(),
        Some(p) => format!("{p}%"),
        None if tokens >= 1000 => format!("{}k ctx", tokens / 1000),
        None => format!("{tokens} ctx"),
    }
}

/// `2/7` while a plan is unfinished, empty otherwise.
fn plan_label() -> String {
    let (done, total) = ah_core::plan::store().snapshot().counts();
    if total == 0 || done == total {
        return String::new();
    }
    format!("{done}/{total}")
}

/// Everything the status line can show: the name `/statusline` lists, and what
/// it means. The order is the order `/statusline` offers them in.
pub const STATUS_ITEMS: &[(&str, &str)] = &[
    ("favorite", "the favorite this model came from"),
    ("model", "model id"),
    ("effort", "reasoning effort"),
    ("modalities", "what the model reads and writes"),
    ("context", "how full the context window is"),
    ("tokens", "tokens sent and received"),
    ("cost", "what the session has cost"),
    ("plan", "tasks finished out of the total"),
    ("cwd", "working directory"),
    ("git", "git branch"),
    ("plugins", "how many plugins are loaded"),
    ("state", "what the turn is doing"),
    ("session", "session id"),
];

/// What stands between two items.
pub const STATUS_SEP: &str = " · ";

/// A piece of the status line: the item that produced it, or `""` for the dots
/// between items and for the text a custom `format` produced.
pub struct Segment {
    pub item: &'static str,
    pub text: String,
}

/// `~/src/ah` for a path under the home directory.
fn short_cwd(cwd: &str) -> String {
    let home = dirs::home_dir()
        .map(|h| h.display().to_string())
        .unwrap_or_default();
    if !home.is_empty() && cwd.starts_with(&home) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd.to_string()
    }
}

/// What one item reads, empty when it has nothing to say.
fn item_text(item: &str, ctx: &StatusContext) -> String {
    let usage = &ctx.usage;
    match item {
        "favorite" => ctx.favorite.clone(),
        "model" => ctx.model.clone(),
        "effort" => ctx.effort.clone(),
        "modalities" => ctx.modalities.clone(),
        "context" => context_label(ctx.context_tokens, ctx.context_window),
        "tokens" if usage.prompt_tokens + usage.completion_tokens == 0 => String::new(),
        "tokens" => format!("↑{} ↓{}", usage.prompt_tokens, usage.completion_tokens),
        "cost" if usage.cost <= 0.0 => String::new(),
        "cost" => format!("${:.4}", usage.cost),
        "plan" => plan_label(),
        "cwd" => short_cwd(&ctx.cwd),
        "git" => ctx.git_branch.clone(),
        "plugins" if ctx.plugins == 0 => String::new(),
        "plugins" => format!("{} plugins", ctx.plugins),
        "state" => ctx.state.clone(),
        "session" => ctx.session_id.clone(),
        _ => String::new(),
    }
}

/// The status line in pieces, so each item can be painted on its own. A custom
/// `format` skips the items and comes back as a single piece.
pub fn status_segments(cfg: &StatusLine, ctx: &StatusContext) -> Vec<Segment> {
    if !cfg.format.is_empty() {
        return vec![Segment {
            item: "",
            text: render_status_template(&cfg.format, ctx),
        }];
    }
    let mut out: Vec<Segment> = Vec::new();
    for name in &cfg.items {
        let Some((item, _)) = STATUS_ITEMS.iter().find(|(n, _)| n == name) else {
            continue;
        };
        let text = item_text(item, ctx);
        if text.is_empty() {
            continue;
        }
        out.push(Segment {
            item: "",
            text: if out.is_empty() { " " } else { STATUS_SEP }.to_string(),
        });
        out.push(Segment { item, text });
    }
    fit(out, ctx.width as usize)
}

/// The width of the whole row.
fn row_width(segs: &[Segment]) -> usize {
    segs.iter().map(|s| s.text.width()).sum()
}

/// A deep path as its last parts: `…/crates/ah/src`.
fn tail_path(path: &str, room: usize) -> String {
    let mut out = String::new();
    for part in path.rsplit('/').filter(|p| !p.is_empty()) {
        if out.width() + part.width() + 2 > room {
            break;
        }
        out = format!("/{part}{out}");
    }
    format!("…{out}")
}

/// Keep the row inside the terminal: the path gives up its head first, then
/// the items at the right end drop off, since the left of the row matters most.
fn fit(mut segs: Vec<Segment>, width: usize) -> Vec<Segment> {
    if width == 0 || row_width(&segs) <= width {
        return segs;
    }
    if let Some(at) = segs.iter().position(|s| s.item == "cwd") {
        let over = row_width(&segs) - width;
        let room = segs[at].text.width().saturating_sub(over);
        segs[at].text = tail_path(&segs[at].text, room);
    }
    while row_width(&segs) > width && segs.len() > 2 {
        segs.truncate(segs.len() - 2);
    }
    segs
}

/// The same pieces, joined, for plugins and for anything that wants the text.
pub fn status_text(cfg: &StatusLine, ctx: &StatusContext) -> String {
    status_segments(cfg, ctx)
        .into_iter()
        .map(|s| s.text)
        .collect()
}

/// A custom `statusline.format`, with its placeholders filled in.
pub fn render_status_template(fmt: &str, ctx: &StatusContext) -> String {
    let short_cwd = short_cwd(&ctx.cwd);
    let out = fmt
        .replace("{model}", &ctx.model)
        .replace("{favorite}", &ctx.favorite)
        .replace("{effort}", &ctx.effort)
        .replace("{tokens_in}", &ctx.usage.prompt_tokens.to_string())
        .replace("{tokens_out}", &ctx.usage.completion_tokens.to_string())
        .replace("{cost}", &format!("{:.4}", ctx.usage.cost))
        .replace(
            "{context}",
            &context_label(ctx.context_tokens, ctx.context_window),
        )
        .replace("{modalities}", &ctx.modalities)
        .replace("{cwd}", &short_cwd)
        .replace(
            "{git}",
            &if ctx.git_branch.is_empty() {
                String::new()
            } else {
                format!("({})", ctx.git_branch)
            },
        )
        .replace("{plan}", &plan_label())
        .replace("{plugins}", &ctx.plugins.to_string())
        .replace("{state}", &ctx.state)
        .replace("{session}", &ctx.session_id);
    // empty placeholders leave double spaces behind
    let mut collapsed = String::with_capacity(out.len());
    for (i, part) in out.split(' ').enumerate() {
        if part.is_empty() && i > 0 {
            continue;
        }
        if i > 0 {
            collapsed.push(' ');
        }
        collapsed.push_str(part);
    }
    collapsed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects the notices an engine emits.
    struct Heard(Mutex<Vec<String>>);

    impl AgentIo for Heard {
        fn emit(&self, ev: AgentEvent) {
            if let AgentEvent::Notice(n) = ev {
                self.0.lock().unwrap().push(n);
            }
        }
        fn ask_permission(&self, _call: &ToolCall, _reason: &str) -> bool {
            false
        }
    }

    #[test]
    fn an_unknown_window_is_looked_up_again_and_said_once() {
        let mut stack = SettingsStack::new();
        stack
            .push(
                Origin::Cli,
                serde_json::json!({"model": {"id": "nobody/not-a-real-model"}}),
            )
            .unwrap();
        let mut e = Engine::new(&stack, ".".into(), None, false).unwrap();
        assert_eq!(e.context_window(), 0);
        // Remembering the miss would leave auto compaction off for the whole
        // process, and the conversation would grow until the provider refused it.
        assert!(e.window.is_none(), "a miss must be looked up again");
        let io = Heard(Mutex::new(Vec::new()));
        e.warn_if_no_window(0, &io);
        e.warn_if_no_window(0, &io);
        let said = io.0.lock().unwrap();
        assert_eq!(said.len(), 1, "said more than once: {said:?}");
        assert!(said[0].contains("nobody/not-a-real-model"), "{}", said[0]);
    }

    #[test]
    fn context_labels() {
        assert_eq!(context_label(0, 8000), "0%");
        assert_eq!(context_label(49, 8000), "<1%");
        assert_eq!(context_label(4000, 8000), "50%");
        assert_eq!(context_label(49, 0), "49 ctx");
        assert_eq!(context_label(12_345, 0), "12k ctx");
    }

    fn ctx() -> StatusContext {
        StatusContext {
            model: "mock/model".into(),
            usage: Usage {
                prompt_tokens: 42,
                completion_tokens: 7,
                cost: 0.0042,
                ..Default::default()
            },
            cwd: "/tmp/work".into(),
            git_branch: "main".into(),
            plugins: 0,
            state: "idle".into(),
            session_id: "s1".into(),
            width: 80,
            favorite: "★fast".into(),
            effort: "high".into(),
            context_tokens: 4000,
            context_window: 8000,
            modalities: "T→T".into(),
            rendered: String::new(),
        }
    }

    #[test]
    fn the_items_are_joined_with_dots_and_keep_their_names() {
        let cfg = StatusLine::default();
        let segs = status_segments(&cfg, &ctx());
        let named: Vec<&str> = segs
            .iter()
            .filter(|s| !s.item.is_empty())
            .map(|s| s.item)
            .collect();
        assert_eq!(
            named,
            [
                "favorite",
                "model",
                "effort",
                "modalities",
                "context",
                "tokens",
                "cost",
                "cwd",
                "git"
            ]
        );
        assert_eq!(
            status_text(&cfg, &ctx()),
            " ★fast · mock/model · high · T→T · 50% · ↑42 ↓7 · $0.0042 · /tmp/work · main"
        );
    }

    #[test]
    fn an_item_with_nothing_to_say_takes_its_dot_with_it() {
        let cfg = StatusLine {
            items: ["model", "cost", "git"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ..Default::default()
        };
        let mut c = ctx();
        c.usage.cost = 0.0;
        assert_eq!(status_text(&cfg, &c), " mock/model · main");
    }

    #[test]
    fn a_narrow_terminal_shortens_the_path_then_drops_the_tail() {
        let mut c = ctx();
        c.cwd = "/home/me/src/agent_harness/crates/ah".into();
        c.width = 80;
        let text = status_text(&StatusLine::default(), &c);
        assert!(text.width() <= 80, "{text:?}");
        assert!(text.ends_with("…/crates/ah · main"), "{text:?}");
        // Narrower still: what is left of the path goes, and the items with it.
        c.width = 20;
        let text = status_text(&StatusLine::default(), &c);
        assert_eq!(text, " ★fast · mock/model");
    }

    #[test]
    fn a_custom_format_is_used_whole() {
        let cfg = StatusLine {
            format: "{model} @ {cwd}".into(),
            ..Default::default()
        };
        let segs = status_segments(&cfg, &ctx());
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "mock/model @ /tmp/work");
        assert!(segs[0].item.is_empty(), "a template has no items to colour");
    }

    #[test]
    fn set_patch_nesting_and_json_values() {
        assert_eq!(
            set_to_patch("theme.accent=magenta").unwrap(),
            serde_json::json!({"theme": {"accent": "magenta"}})
        );
        assert_eq!(
            set_to_patch("layout.input_height=5").unwrap(),
            serde_json::json!({"layout": {"input_height": 5}})
        );
        assert_eq!(
            set_to_patch("tools.enabled=[\"bash\"]").unwrap(),
            serde_json::json!({"tools": {"enabled": ["bash"]}})
        );
        assert!(set_to_patch("nope").is_err());
    }
}
