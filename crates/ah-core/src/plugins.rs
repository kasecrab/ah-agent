//! wasm plugin host on `wasmi`.
//!
//! Each plugin is a core wasm module exporting `ah_alloc`, `ah_free`,
//! `ah_manifest` and `ah_call`, and importing from module `"ah"`:
//!
//! ```text
//! log(level: i32, ptr: i32, len: i32)
//! host_call(name_ptr, name_len, in_ptr, in_len) -> i32   // result length, <0 = error
//! host_read(dst_ptr, cap) -> i32                          // copies pending result
//! ```
//!
//! Host calls are two-step (call, then read) to avoid re-entrancy.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ah_abi::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use wasmi::{
    Caller, Engine, Instance, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
    TypedFunc,
};

use crate::agent::Hooks;
use crate::{Error, Result};

struct State {
    name: String,
    limits: StoreLimits,
    pending: Vec<u8>,
    kv: HashMap<String, String>,
    kv_path: PathBuf,
    settings: Value,
    cwd: String,
    log: Vec<(LogLevel, String)>,
}

pub struct Plugin {
    pub manifest: Manifest,
    pub path: PathBuf,
    store: Store<State>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    free: TypedFunc<(i32, i32), ()>,
    call: TypedFunc<(i32, i32, i32, i32), i64>,
    hooks: HashSet<Hook>,
    tool_names: HashSet<String>,
    disabled_hooks: HashSet<Hook>,
    fuel: u64,
}

#[derive(Debug, Clone)]
pub struct LoadReport {
    pub path: PathBuf,
    pub name: String,
    pub ok: bool,
    pub message: String,
}

pub struct PluginHost {
    engine: Engine,
    plugins: Vec<Plugin>,
    pub reports: Vec<LoadReport>,
    /// Collected plugin log lines, drained by the UI.
    pub log: Vec<(String, LogLevel, String)>,
}

fn pack(ptr: i32, len: i32) -> (usize, usize) {
    (ptr as u32 as usize, len as u32 as usize)
}

impl PluginHost {
    pub fn empty() -> Self {
        let mut cfg = wasmi::Config::default();
        cfg.consume_fuel(true);
        // validate at load, translate on first call
        cfg.compilation_mode(wasmi::CompilationMode::Lazy);
        Self {
            engine: Engine::new(&cfg),
            plugins: Vec::new(),
            reports: Vec::new(),
            log: Vec::new(),
        }
    }

    /// Discover and load every plugin the settings allow.
    pub fn load(settings: &Settings, settings_value: &Value, cwd: &str) -> Self {
        let mut host = Self::empty();
        if !settings.plugins.enabled {
            return host;
        }
        let mut files: Vec<PathBuf> = Vec::new();
        let mut dirs = crate::paths::plugin_dirs();
        for p in &settings.plugins.paths {
            let pb = crate::tools::resolve_path(Path::new(cwd), p);
            if pb.is_dir() {
                dirs.push(pb);
            } else {
                files.push(pb);
            }
        }
        for d in dirs {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            let mut found: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "wasm"))
                .collect();
            found.sort();
            files.extend(found);
        }
        for f in files {
            host.load_file(&f, settings, settings_value, cwd);
        }
        host
    }

    pub fn load_file(
        &mut self,
        path: &Path,
        settings: &Settings,
        settings_value: &Value,
        cwd: &str,
    ) {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();
        if settings.plugins.disabled.contains(&stem) {
            self.reports.push(LoadReport {
                path: path.into(),
                name: stem,
                ok: false,
                message: "disabled".into(),
            });
            return;
        }
        match self.instantiate(path, settings, settings_value, cwd) {
            Ok(p) => {
                if settings.plugins.disabled.contains(&p.manifest.name) {
                    self.reports.push(LoadReport {
                        path: path.into(),
                        name: p.manifest.name,
                        ok: false,
                        message: "disabled".into(),
                    });
                    return;
                }
                if self
                    .plugins
                    .iter()
                    .any(|x| x.manifest.name == p.manifest.name)
                {
                    self.reports.push(LoadReport {
                        path: path.into(),
                        name: p.manifest.name.clone(),
                        ok: false,
                        message: "duplicate name".into(),
                    });
                    return;
                }
                self.reports.push(LoadReport {
                    path: path.into(),
                    name: p.manifest.name.clone(),
                    ok: true,
                    message: format!(
                        "v{} hooks={} tools={}",
                        p.manifest.version,
                        p.hooks.len(),
                        p.tool_names.len()
                    ),
                });
                self.plugins.push(p);
            }
            Err(e) => {
                crate::warn!("plugin {}: {e}", path.display());
                self.reports.push(LoadReport {
                    path: path.into(),
                    name: stem,
                    ok: false,
                    message: e.to_string(),
                });
            }
        }
    }

    fn instantiate(
        &self,
        path: &Path,
        settings: &Settings,
        settings_value: &Value,
        cwd: &str,
    ) -> Result<Plugin> {
        let bytes = std::fs::read(path)?;
        let perr = |m: String| Error::Plugin {
            plugin: path.display().to_string(),
            message: m,
        };
        let module = Module::new(&self.engine, &bytes).map_err(|e| perr(e.to_string()))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();
        let kv_path = crate::paths::plugin_state_dir().join(format!("{stem}.json"));
        let kv = std::fs::read_to_string(&kv_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let state = State {
            name: stem.clone(),
            limits: StoreLimitsBuilder::new()
                .memory_size(settings.plugins.max_memory_bytes as usize)
                .build(),
            pending: Vec::new(),
            kv,
            kv_path,
            settings: settings_value.clone(),
            cwd: cwd.into(),
            log: Vec::new(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);

        let mut linker: Linker<State> = Linker::new(&self.engine);
        linker
            .func_wrap(
                "ah",
                "log",
                |mut caller: Caller<'_, State>, level: i32, ptr: i32, len: i32| {
                    let msg = read_guest(&mut caller, ptr, len)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    caller
                        .data_mut()
                        .log
                        .push((LogLevel::from_u32(level as u32), msg));
                },
            )
            .map_err(|e| perr(e.to_string()))?;
        linker
            .func_wrap(
                "ah",
                "host_call",
                |mut caller: Caller<'_, State>, np: i32, nl: i32, ip: i32, il: i32| -> i32 {
                    let name = read_guest(&mut caller, np, nl)
                        .map(|b| String::from_utf8_lossy(&b).into_owned())
                        .unwrap_or_default();
                    let input = read_guest(&mut caller, ip, il).unwrap_or_default();
                    let out = host_call(caller.data_mut(), &name, &input);
                    let st = caller.data_mut();
                    match out {
                        Ok(bytes) => {
                            st.pending = bytes;
                            st.pending.len() as i32
                        }
                        Err(msg) => {
                            st.pending = msg.into_bytes();
                            -(st.pending.len() as i32)
                        }
                    }
                },
            )
            .map_err(|e| perr(e.to_string()))?;
        linker
            .func_wrap(
                "ah",
                "host_read",
                |mut caller: Caller<'_, State>, dst: i32, cap: i32| -> i32 {
                    let data = std::mem::take(&mut caller.data_mut().pending);
                    let n = data.len().min(cap as u32 as usize);
                    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory())
                    else {
                        return -1;
                    };
                    if mem
                        .write(&mut caller, dst as u32 as usize, &data[..n])
                        .is_err()
                    {
                        return -1;
                    }
                    n as i32
                },
            )
            .map_err(|e| perr(e.to_string()))?;

        store
            .set_fuel(settings.plugins.fuel_per_call)
            .map_err(|e| perr(e.to_string()))?;
        let instance: Instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| perr(e.to_string()))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| perr("no exported memory".into()))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&store, "ah_alloc")
            .map_err(|e| perr(format!("ah_alloc: {e}")))?;
        let free = instance
            .get_typed_func::<(i32, i32), ()>(&store, "ah_free")
            .map_err(|e| perr(format!("ah_free: {e}")))?;
        let call = instance
            .get_typed_func::<(i32, i32, i32, i32), i64>(&store, "ah_call")
            .map_err(|e| perr(format!("ah_call: {e}")))?;
        let manifest_fn = instance
            .get_typed_func::<(), i64>(&store, "ah_manifest")
            .map_err(|e| perr(format!("ah_manifest: {e}")))?;

        let packed = manifest_fn
            .call(&mut store, ())
            .map_err(|e| perr(format!("ah_manifest trapped: {e}")))?;
        let (mptr, mlen) = pack((packed >> 32) as i32, packed as i32);
        let mut buf = vec![0u8; mlen];
        memory
            .read(&store, mptr, &mut buf)
            .map_err(|e| perr(e.to_string()))?;
        let _ = free.call(&mut store, (mptr as i32, mlen as i32));
        let manifest: Manifest =
            serde_json::from_slice(&buf).map_err(|e| perr(format!("manifest json: {e}")))?;
        if manifest.abi_version != ABI_VERSION {
            return Err(perr(format!(
                "abi version {} != host {}",
                manifest.abi_version, ABI_VERSION
            )));
        }
        let mut manifest = manifest;
        if manifest.name.is_empty() {
            manifest.name = stem;
        }
        store.data_mut().name = manifest.name.clone();
        // Declaring tools or commands implies the hooks that serve them.
        if !manifest.tools.is_empty() && !manifest.hooks.contains(&Hook::ToolCall) {
            manifest.hooks.push(Hook::ToolCall);
        }
        if !manifest.commands.is_empty() && !manifest.hooks.contains(&Hook::SlashCommand) {
            manifest.hooks.push(Hook::SlashCommand);
        }
        let hooks = manifest.hooks.iter().copied().collect();
        let tool_names = manifest
            .tools
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        Ok(Plugin {
            manifest,
            path: path.into(),
            store,
            memory,
            alloc,
            free,
            call,
            hooks,
            tool_names,
            disabled_hooks: HashSet::new(),
            fuel: settings.plugins.fuel_per_call,
        })
    }

    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Push the latest merged settings into every plugin's `settings_get` view.
    pub fn update_settings(&mut self, v: &Value) {
        for p in &mut self.plugins {
            p.store.data_mut().settings = v.clone();
        }
    }

    /// Static manifest patches, in load order. Applied before `on_load`.
    pub fn manifest_patches(&self) -> Vec<(String, Value)> {
        self.plugins
            .iter()
            .filter_map(|p| {
                p.manifest
                    .settings_patch
                    .clone()
                    .map(|v| (p.manifest.name.clone(), v))
            })
            .collect()
    }

    /// Run `on_load` on every subscriber; returns `(plugin, patch)` pairs.
    pub fn on_load(&mut self, settings: &Settings, cwd: &str) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        let input = OnLoadIn {
            settings: settings.clone(),
            cwd: cwd.into(),
        };
        for p in &mut self.plugins {
            if let Some(r) = p.call_typed::<_, OnLoadOut>(Hook::OnLoad, &input)
                && let Some(v) = r.settings_patch
            {
                out.push((p.manifest.name.clone(), v));
            }
        }
        self.drain_logs();
        out
    }

    pub fn statusline(&mut self, ctx: &StatusContext) -> Option<String> {
        let mut text: Option<String> = None;
        let mut ctx = ctx.clone();
        for p in &mut self.plugins {
            if let Some(r) = p.call_typed::<_, StatuslineOut>(Hook::Statusline, &ctx) {
                ctx.rendered = r.text.clone();
                text = Some(r.text);
            }
        }
        text
    }

    pub fn keybinds(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for p in &mut self.plugins {
            if let Some(r) = p.call_typed::<_, KeybindsOut>(Hook::Keybinds, &Value::Null) {
                out.extend(r.binds);
            }
        }
        out
    }

    pub fn commands(&self) -> Vec<(String, SlashCommandSpec)> {
        self.plugins
            .iter()
            .flat_map(|p| {
                p.manifest
                    .commands
                    .iter()
                    .map(move |c| (p.manifest.name.clone(), c.clone()))
            })
            .collect()
    }

    /// Dispatch a slash command to the plugin that declared it.
    pub fn slash_command(&mut self, name: &str, args: &str, cwd: &str) -> Option<SlashCommandOut> {
        let idx = self
            .plugins
            .iter()
            .position(|p| p.manifest.commands.iter().any(|c| c.name == name))?;
        let input = SlashCommandIn {
            name: name.into(),
            args: args.into(),
            cwd: cwd.into(),
        };
        let r = self.plugins[idx].call_typed::<_, SlashCommandOut>(Hook::SlashCommand, &input);
        self.drain_logs();
        r
    }

    pub fn drain_logs(&mut self) {
        for p in &mut self.plugins {
            let name = p.manifest.name.clone();
            for (lvl, msg) in p.store.data_mut().log.drain(..) {
                crate::log::write(lvl as u8, format_args!("[{name}] {msg}"));
                self.log.push((name.clone(), lvl, msg));
            }
        }
    }

    pub fn take_logs(&mut self) -> Vec<(String, LogLevel, String)> {
        self.drain_logs();
        std::mem::take(&mut self.log)
    }
}

impl Plugin {
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    fn write_guest(&mut self, data: &[u8]) -> Result<(i32, i32)> {
        let len = data.len() as i32;
        let ptr = self
            .alloc
            .call(&mut self.store, len)
            .map_err(|e| self.err(format!("ah_alloc: {e}")))?;
        self.memory
            .write(&mut self.store, ptr as u32 as usize, data)
            .map_err(|e| self.err(e.to_string()))?;
        Ok((ptr, len))
    }

    fn err(&self, message: String) -> Error {
        Error::Plugin {
            plugin: self.manifest.name.clone(),
            message,
        }
    }

    /// Raw hook call: JSON bytes in, JSON bytes out.
    pub fn call_raw(&mut self, hook: Hook, input: &[u8]) -> Result<Vec<u8>> {
        let _ = self.store.set_fuel(self.fuel);
        let hook_name = hook.as_str().as_bytes();
        let (hp, hl) = self.write_guest(hook_name)?;
        let (ip, il) = self.write_guest(input)?;
        let packed = self.call.call(&mut self.store, (hp, hl, ip, il));
        // guest frees the inputs
        let packed = packed.map_err(|e| self.err(format!("{} trapped: {e}", hook.as_str())))?;
        let (optr, olen) = pack((packed >> 32) as i32, packed as i32);
        let mut buf = vec![0u8; olen];
        self.memory
            .read(&self.store, optr, &mut buf)
            .map_err(|e| self.err(e.to_string()))?;
        let _ = self.free.call(&mut self.store, (optr as i32, olen as i32));
        Ok(buf)
    }

    /// Typed call. A hook that traps is disabled for the rest of the session.
    pub fn call_typed<I: Serialize, O: DeserializeOwned>(
        &mut self,
        hook: Hook,
        input: &I,
    ) -> Option<O> {
        if !self.hooks.contains(&hook) || self.disabled_hooks.contains(&hook) {
            return None;
        }
        let bytes = serde_json::to_vec(input).ok()?;
        match self.call_raw(hook, &bytes) {
            Ok(out) => {
                let v: Value = match serde_json::from_slice(&out) {
                    Ok(v) => v,
                    Err(e) => {
                        self.fail(hook, format!("bad json from {}: {e}", hook.as_str()));
                        return None;
                    }
                };
                if let Some(err) = v.get("err").and_then(Value::as_str) {
                    self.store
                        .data_mut()
                        .log
                        .push((LogLevel::Error, format!("{}: {err}", hook.as_str())));
                    return None;
                }
                let ok = v.get("ok").cloned().unwrap_or(Value::Null);
                match serde_json::from_value::<O>(ok) {
                    Ok(o) => Some(o),
                    Err(e) => {
                        self.fail(
                            hook,
                            format!("unexpected shape from {}: {e}", hook.as_str()),
                        );
                        None
                    }
                }
            }
            Err(e) => {
                self.fail(hook, e.to_string());
                None
            }
        }
    }

    fn fail(&mut self, hook: Hook, msg: String) {
        crate::error!(
            "plugin {} hook {} disabled: {msg}",
            self.manifest.name,
            hook.as_str()
        );
        self.store.data_mut().log.push((
            LogLevel::Error,
            format!("hook {} disabled: {msg}", hook.as_str()),
        ));
        self.disabled_hooks.insert(hook);
    }
}

fn read_guest(caller: &mut Caller<'_, State>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    let mem = caller.get_export("memory")?.into_memory()?;
    let (p, l) = pack(ptr, len);
    let mut buf = vec![0u8; l];
    mem.read(&*caller, p, &mut buf).ok()?;
    Some(buf)
}

/// Host capabilities reachable from plugins. Inputs and outputs are JSON.
fn host_call(st: &mut State, name: &str, input: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let arg: Value = if input.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(input).map_err(|e| e.to_string())?
    };
    let out: Value = match name {
        "settings_get" => {
            let ptr = arg.as_str().unwrap_or("");
            if ptr.is_empty() {
                st.settings.clone()
            } else {
                st.settings.pointer(ptr).cloned().unwrap_or(Value::Null)
            }
        }
        "kv_get" => st
            .kv
            .get(arg.as_str().unwrap_or(""))
            .cloned()
            .map(Value::String)
            .unwrap_or(Value::Null),
        "kv_set" => {
            let k = arg
                .get("key")
                .and_then(Value::as_str)
                .ok_or("kv_set needs key")?
                .to_string();
            match arg.get("value") {
                Some(Value::String(v)) => {
                    st.kv.insert(k, v.clone());
                }
                Some(Value::Null) | None => {
                    st.kv.remove(&k);
                }
                Some(other) => {
                    st.kv.insert(k, other.to_string());
                }
            }
            if let Some(p) = st.kv_path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let _ = std::fs::write(&st.kv_path, serde_json::to_vec(&st.kv).unwrap_or_default());
            Value::Bool(true)
        }
        "cwd" => Value::String(st.cwd.clone()),
        "now_ms" => Value::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        ),
        "env_get" => std::env::var(arg.as_str().unwrap_or(""))
            .map(Value::String)
            .unwrap_or(Value::Null),
        "read_file" => {
            let p = crate::tools::resolve_path(
                Path::new(&st.cwd),
                arg.as_str().ok_or("read_file needs a path")?,
            );
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            Value::String(text)
        }
        "git_branch" => Value::String(git_branch(Path::new(&st.cwd))),
        other => return Err(format!("unknown host call `{other}`")),
    };
    serde_json::to_vec(&out).map_err(|e| e.to_string())
}

/// Current branch from `.git/HEAD` without spawning git.
pub fn git_branch(cwd: &Path) -> String {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let head = d.join(".git/HEAD");
        if let Ok(s) = std::fs::read_to_string(&head) {
            return s
                .trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_string)
                .unwrap_or_else(|| s.trim().chars().take(8).collect());
        }
        dir = d.parent();
    }
    String::new()
}

impl Hooks for PluginHost {
    fn system_prompt(&mut self, input: SystemPromptIn) -> String {
        let mut cur = input;
        for p in &mut self.plugins {
            if let Some(r) = p.call_typed::<_, SystemPromptOut>(Hook::SystemPrompt, &cur) {
                cur.prompt = r.prompt;
            }
        }
        cur.prompt
    }

    fn before_request(&mut self, req: ChatRequest, turn: u32) -> ChatRequest {
        let mut cur = BeforeRequestIn { request: req, turn };
        for p in &mut self.plugins {
            if let Some(r) = p.call_typed::<_, BeforeRequestOut>(Hook::BeforeRequest, &cur) {
                cur.request = r.request;
            }
        }
        cur.request
    }

    fn before_tool(&mut self, call: &ToolCall, cwd: &str) -> (ToolDecision, Vec<Value>) {
        let mut input = BeforeToolIn {
            call: call.clone(),
            cwd: cwd.into(),
        };
        let mut decision = ToolDecision::Allow;
        let mut patches = Vec::new();
        for p in &mut self.plugins {
            let Some(r) = p.call_typed::<_, BeforeToolOut>(Hook::BeforeTool, &input) else {
                continue;
            };
            if let Some(v) = r.settings_patch {
                patches.push(v);
            }
            match r.decision {
                ToolDecision::Allow => {}
                ToolDecision::Deny { reason } => return (ToolDecision::Deny { reason }, patches),
                ToolDecision::Replace { arguments } => {
                    input.call.function.arguments = arguments.clone();
                    decision = ToolDecision::Replace { arguments };
                }
                ToolDecision::Ask { reason } => {
                    if !matches!(decision, ToolDecision::Replace { .. }) {
                        decision = ToolDecision::Ask { reason };
                    }
                }
            }
        }
        (decision, patches)
    }

    fn after_tool(
        &mut self,
        call: &ToolCall,
        result: ToolResult,
        duration_ms: u64,
    ) -> (ToolResult, Vec<Value>) {
        let mut input = AfterToolIn {
            call: call.clone(),
            result,
            duration_ms,
        };
        let mut patches = Vec::new();
        for p in &mut self.plugins {
            let Some(r) = p.call_typed::<_, AfterToolOut>(Hook::AfterTool, &input) else {
                continue;
            };
            if let Some(v) = r.settings_patch {
                patches.push(v);
            }
            if let Some(res) = r.result {
                input.result = res;
            }
        }
        (input.result, patches)
    }

    fn plugin_tool(&mut self, call: &ToolCall, cwd: &str) -> Option<ToolResult> {
        let idx = self
            .plugins
            .iter()
            .position(|p| p.tool_names.contains(&call.function.name))?;
        let input = ToolCallIn {
            call: call.clone(),
            cwd: cwd.into(),
        };
        let p = &mut self.plugins[idx];
        Some(
            match p.call_typed::<_, ToolCallOut>(Hook::ToolCall, &input) {
                Some(r) => r.result,
                None => ToolResult::err(format!(
                    "plugin {} failed to run tool {}",
                    p.manifest.name, call.function.name
                )),
            },
        )
    }

    fn plugin_tool_specs(&self) -> Vec<ToolSpec> {
        self.plugins
            .iter()
            .flat_map(|p| p.manifest.tools.iter().cloned())
            .collect()
    }

    fn on_turn_end(&mut self, input: OnTurnEndIn) -> (Vec<Value>, Vec<String>) {
        let mut patches = Vec::new();
        let mut notices = Vec::new();
        for p in &mut self.plugins {
            if let Some(r) = p.call_typed::<_, OnTurnEndOut>(Hook::OnTurnEnd, &input) {
                if let Some(v) = r.settings_patch {
                    patches.push(v);
                }
                if let Some(m) = r.message {
                    notices.push(m);
                }
            }
        }
        (patches, notices)
    }
}
