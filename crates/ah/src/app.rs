//! Shared runtime: settings, plugins, provider, and the engine owning the conversation.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};

use ah_core::abi::*;
use ah_core::agent::{Agent, AgentEvent, AgentIo, Hooks, NoHooks, TurnSummary};
use ah_core::plugins::{LoadReport, PluginHost};
use ah_core::provider::Provider;
use ah_core::provider::openrouter::OpenRouter;
use ah_core::session::Session;
use ah_core::settings::{Origin, SettingsStack};
use ah_core::tools::Registry;
use serde_json::Value;

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
    Submit(String),
    Slash {
        name: String,
        args: String,
    },
    Statusline(StatusContext),
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
    Statusline(String),
    Slash(Box<SlashCommandOut>),
    AskPermission {
        call: ToolCall,
        reason: String,
    },
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
    pub provider: Option<Box<dyn Provider>>,
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
    /// `(model id, window)` looked up in the catalogue.
    window: Option<(String, u64)>,
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
            registry: Registry::builtins(&settings.tools),
            host: None,
            settings,
            settings_value: stack.value().clone(),
            cwd,
            session,
            cancel: Arc::new(AtomicBool::new(false)),
            total_usage: Usage::default(),
            context_tokens: 0,
            window: None,
        };
        e.rebuild_provider();
        Ok(e)
    }

    pub fn rebuild_provider(&mut self) {
        let key = ah_core::auth::api_key(self.settings.model.api_key.as_deref());
        self.provider = key.map(|k| {
            Box::new(OpenRouter::new(self.settings.model.base_url.clone(), k)) as Box<dyn Provider>
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
        self.window = Some((id.clone(), w));
        w
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
            ..
        } = self;
        let provider = provider.as_deref()?;
        let hooks: &mut dyn Hooks = match host.as_mut() {
            Some(h) => h,
            None => no_hooks,
        };
        let mut a = Agent::new(provider, registry, hooks, settings, cwd.clone(), cancel);
        a.context_window = window;
        a.context_tokens = *context_tokens;
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
        let tools_changed = settings.tools != self.settings.tools;
        self.settings = settings;
        self.settings_value = value;
        if let Some(h) = self.host.as_mut() {
            h.update_settings(&self.settings_value);
        }
        if model_changed {
            self.rebuild_provider();
        }
        if tools_changed {
            self.registry = Registry::builtins(&self.settings.tools);
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
        io: &dyn AgentIo,
    ) -> Result<TurnSummary, ah_core::Error> {
        let window = self.context_window();
        self.cancel.store(false, Ordering::Relaxed);
        let mut messages = std::mem::take(&mut self.session.messages);
        let mut no_hooks = NoHooks;
        let Some(mut agent) = self.agent(&mut no_hooks, window) else {
            self.session.messages = messages;
            return Err(no_key(io));
        };
        // Compact before the new message so it survives verbatim.
        if agent.over_threshold() {
            io.emit(AgentEvent::Notice("compacting context…".into()));
            if let Err(e) = agent.compact(&mut messages, "", io) {
                io.emit(AgentEvent::Notice(format!("compaction failed: {e}")));
            }
        }
        let before = messages.len();
        messages.push(Message::user(text));
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
        if let Err(e) = &res {
            io.emit(AgentEvent::Error(format!("compaction failed: {e}")));
        }
        res
    }

    pub fn statusline(&mut self, ctx: &StatusContext) -> Option<String> {
        self.host.as_mut().and_then(|h| h.statusline(ctx))
    }

    pub fn slash(&mut self, name: &str, args: &str) -> Option<SlashCommandOut> {
        let cwd = self.cwd.display().to_string();
        self.host
            .as_mut()
            .and_then(|h| h.slash_command(name, args, &cwd))
    }

    /// Engine thread main loop for the TUI.
    pub fn serve(mut self, rx: Receiver<EngineCmd>, tx: Sender<UiEvent>, perm_rx: Receiver<bool>) {
        let io = ChannelIo {
            tx: tx.clone(),
            perm_rx,
            cancel: self.cancel.clone(),
        };
        while let Ok(cmd) = rx.recv() {
            match cmd {
                EngineCmd::Submit(text) => {
                    let _ = tx.send(UiEvent::Busy(true));
                    let _ = self.run_turn(text, &io);
                    let logs = self.take_plugin_logs();
                    if !logs.is_empty() {
                        let _ = tx.send(UiEvent::PluginLogs(logs));
                    }
                    let _ = tx.send(UiEvent::Busy(false));
                }
                EngineCmd::Slash { name, args } => {
                    if let Some(out) = self.slash(&name, &args) {
                        let _ = tx.send(UiEvent::Slash(Box::new(out)));
                    } else {
                        let _ = tx.send(UiEvent::Slash(Box::new(SlashCommandOut {
                            message: Some(format!("unknown command /{name} (try /help)")),
                            ..Default::default()
                        })));
                    }
                }
                EngineCmd::Statusline(ctx) => {
                    if let Some(s) = self.statusline(&ctx) {
                        let _ = tx.send(UiEvent::Statusline(s));
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
                    self.context_tokens = 0;
                }
                EngineCmd::Resume(id) => match Session::open(&id) {
                    Ok(s) => {
                        self.session = s;
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
pub struct ChannelIo {
    pub tx: Sender<UiEvent>,
    pub perm_rx: Receiver<bool>,
    pub cancel: Arc<AtomicBool>,
}

impl AgentIo for ChannelIo {
    fn emit(&self, ev: AgentEvent) {
        let _ = self.tx.send(UiEvent::Agent(ev));
    }
    fn ask_permission(&self, call: &ToolCall, reason: &str) -> bool {
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
        // drop stale answers first
        while self.perm_rx.try_recv().is_ok() {}
        match self.perm_rx.recv() {
            Ok(v) => v && !self.cancel.load(Ordering::Relaxed),
            Err(_) => false,
        }
    }
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

/// Built-in statusline template.
pub fn render_status_template(fmt: &str, ctx: &StatusContext) -> String {
    let short_cwd = {
        let home = dirs::home_dir()
            .map(|h| h.display().to_string())
            .unwrap_or_default();
        if !home.is_empty() && ctx.cwd.starts_with(&home) {
            format!("~{}", &ctx.cwd[home.len()..])
        } else {
            ctx.cwd.clone()
        }
    };
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
        .replace("{cwd}", &short_cwd)
        .replace(
            "{git}",
            &if ctx.git_branch.is_empty() {
                String::new()
            } else {
                format!("({})", ctx.git_branch)
            },
        )
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

    #[test]
    fn context_labels() {
        assert_eq!(context_label(0, 8000), "0%");
        assert_eq!(context_label(49, 8000), "<1%");
        assert_eq!(context_label(4000, 8000), "50%");
        assert_eq!(context_label(49, 0), "49 ctx");
        assert_eq!(context_label(12_345, 0), "12k ctx");
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
