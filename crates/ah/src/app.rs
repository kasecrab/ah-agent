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
            Some(id) => Session::open(id)?,
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
        let Some(provider) = self.provider.as_deref() else {
            let msg = "no API key: run `ah login`, or set OPENROUTER_API_KEY".to_string();
            io.emit(AgentEvent::Error(msg.clone()));
            return Err(ah_core::Error::Auth(msg));
        };
        self.cancel.store(false, Ordering::Relaxed);
        self.session.push(Message::user(text));
        let mut no_hooks = NoHooks;
        let hooks: &mut dyn Hooks = match self.host.as_mut() {
            Some(h) => h,
            None => &mut no_hooks,
        };
        let mut agent = Agent::new(
            provider,
            &self.registry,
            hooks,
            &self.settings,
            self.cwd.clone(),
            &self.cancel,
        );
        let before = self.session.messages.len();
        let mut messages = std::mem::take(&mut self.session.messages);
        let res = agent.run_turn(&mut messages, io);
        let appended: Vec<Message> = messages.drain(before..).collect();
        self.session.messages = messages;
        for m in appended {
            self.session.push(m);
        }
        if let Ok(s) = &res {
            self.total_usage.add(&s.usage);
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
                EngineCmd::Clear => self.session.clear(),
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
    fmt.replace("{model}", &ctx.model)
        .replace("{tokens_in}", &ctx.usage.prompt_tokens.to_string())
        .replace("{tokens_out}", &ctx.usage.completion_tokens.to_string())
        .replace("{cost}", &format!("{:.4}", ctx.usage.cost))
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
        .replace("{session}", &ctx.session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

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
