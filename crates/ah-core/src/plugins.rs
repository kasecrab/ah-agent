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

/// The settings as a plugin is allowed to see them.
///
/// A plugin is a program somebody else wrote. It has no business with the API
/// key, and handing it over to every one of them turns a theme into something
/// that can bill the user's account.
fn without_secrets(settings: &Value) -> Value {
    let mut out = settings.clone();
    if let Some(model) = out.get_mut("model").and_then(Value::as_object_mut) {
        model.remove("api_key");
    }
    out
}

/// Every `*.wasm` the settings point at, in load order. `load` walks this and
/// the cache keys itself on it, so the two cannot disagree about which files a
/// load would have read.
pub fn discover(settings: &Settings, cwd: &str) -> Vec<PathBuf> {
    if !settings.plugins.enabled {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = Vec::new();
    // The user's own directory always; the working directory's only if the
    // user's own config said so. A plugin reaches every variable in the
    // environment and every file this process can read, and it runs before
    // anything has been asked.
    let mut dirs = vec![crate::paths::config_dir().join("plugins")];
    if settings.plugins.trust_project {
        dirs.push(crate::paths::project_plugin_dir());
    }
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
    files
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
        for f in discover(settings, cwd) {
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
            settings: without_secrets(settings_value),
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
            if let Some(r) = p.call_typed::<_, OnLoadOut>(Hook::OnLoad, &input).ok()
                && let Some(v) = r.settings_patch
            {
                out.push((p.manifest.name.clone(), v));
            }
        }
        self.drain_logs();
        out
    }

    /// The last plugin to answer wins; each one sees what the one before it
    /// made in `rendered`, so a plugin can decorate rather than replace. A
    /// row given as coloured spans also carries their joined text.
    pub fn statusline(&mut self, ctx: &StatusContext) -> Option<StatuslineOut> {
        let mut out: Option<StatuslineOut> = None;
        let mut ctx = ctx.clone();
        for p in &mut self.plugins {
            if let Some(mut r) = p
                .call_typed::<_, StatuslineOut>(Hook::Statusline, &ctx)
                .ok()
            {
                if !r.spans.is_empty() {
                    r.text = r.spans.iter().map(|s| s.text.as_str()).collect();
                }
                ctx.rendered = r.text.clone();
                out = Some(r);
            }
        }
        out
    }

    pub fn keybinds(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for p in &mut self.plugins {
            if let Some(r) = p
                .call_typed::<_, KeybindsOut>(Hook::Keybinds, &Value::Null)
                .ok()
            {
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
    pub fn slash_command(
        &mut self,
        name: &str,
        args: &str,
        cwd: &str,
        stage: SlashStage,
    ) -> Option<SlashCommandOut> {
        let idx = self
            .plugins
            .iter()
            .position(|p| p.manifest.commands.iter().any(|c| c.name == name))?;
        let input = SlashCommandIn {
            name: name.into(),
            args: args.into(),
            cwd: cwd.into(),
            stage,
        };
        let r = self.plugins[idx]
            .call_typed::<_, SlashCommandOut>(Hook::SlashCommand, &input)
            .ok();
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

    /// Typed call. A hook that traps is disabled for the rest of the session,
    /// unless it is one whose silence would be an answer — see `fail`.
    pub fn call_typed<I: Serialize, O: DeserializeOwned>(
        &mut self,
        hook: Hook,
        input: &I,
    ) -> Called<O> {
        if !self.hooks.contains(&hook) || self.disabled_hooks.contains(&hook) {
            return Called::Nothing;
        }
        let bytes = match serde_json::to_vec(input) {
            Ok(b) => b,
            Err(e) => return Called::Failed(format!("input json: {e}")),
        };
        match self.call_raw(hook, &bytes) {
            Ok(out) => {
                let v: Value = match serde_json::from_slice(&out) {
                    Ok(v) => v,
                    Err(e) => {
                        let why = format!("bad json from {}: {e}", hook.as_str());
                        self.fail(hook, why.clone());
                        return Called::Failed(why);
                    }
                };
                if let Some(err) = v.get("err").and_then(Value::as_str) {
                    self.store
                        .data_mut()
                        .log
                        .push((LogLevel::Error, format!("{}: {err}", hook.as_str())));
                    return Called::Failed(err.to_string());
                }
                let ok = v.get("ok").cloned().unwrap_or(Value::Null);
                // `null` is the documented "no change" answer.
                if ok.is_null() {
                    return Called::Nothing;
                }
                match serde_json::from_value::<O>(ok) {
                    Ok(o) => Called::Answered(o),
                    Err(e) => {
                        let why = format!("unexpected shape from {}: {e}", hook.as_str());
                        self.fail(hook, why.clone());
                        Called::Failed(why)
                    }
                }
            }
            Err(e) => {
                let why = e.to_string();
                self.fail(hook, why.clone());
                Called::Failed(why)
            }
        }
    }

    fn fail(&mut self, hook: Hook, msg: String) {
        // A hook whose job is to say no is not switched off when it breaks.
        // Turning `before_tool` off would turn a policy into a no-op for the
        // rest of the session, and the way to reach that state is to send it
        // one command it cannot handle — which is the command you would want
        // it switched off for. It stays on, and every call it fails is a
        // refusal (see `Plugins::before_tool`).
        let policy = matches!(hook, Hook::BeforeTool);
        let what = if policy { "failed" } else { "disabled" };
        crate::error!(
            "plugin {} hook {} {what}: {msg}",
            self.manifest.name,
            hook.as_str()
        );
        self.store.data_mut().log.push((
            LogLevel::Error,
            format!("hook {} {what}: {msg}", hook.as_str()),
        ));
        if !policy {
            self.disabled_hooks.insert(hook);
        }
    }
}

/// What a hook call came back with.
pub enum Called<O> {
    /// The hook answered.
    Answered(O),
    /// Nothing to do: the plugin does not handle this hook, or answered with
    /// the documented `null` for "no change".
    Nothing,
    /// The hook was called and did not come back with an answer this host can
    /// use — a trap, out of fuel, an `Err`, or a shape that is not the one the
    /// hook returns. What that means is the caller's to decide, and for a hook
    /// that decides whether something runs it means no.
    Failed(String),
}

impl<O> Called<O> {
    /// The answer, if there was one. For every hook but the policy one, a
    /// failure and a "no change" mean the same thing to the caller.
    pub fn ok(self) -> Option<O> {
        match self {
            Called::Answered(o) => Some(o),
            Called::Nothing | Called::Failed(_) => None,
        }
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
        // A plugin may read the environment, and not the parts of it that are
        // credentials. It is a program somebody else wrote; a theme has no
        // business with the key that pays for the model.
        "env_get" => match arg.as_str().unwrap_or("") {
            name if crate::jobs::SECRETS.contains(&name) => Value::Null,
            name => std::env::var(name)
                .map(Value::String)
                .unwrap_or(Value::Null),
        },
        "read_file" => {
            let p = crate::tools::resolve_path(
                Path::new(&st.cwd),
                arg.as_str().ok_or("read_file needs a path")?,
            );
            // Not the credentials file, by any spelling of it. Everything else
            // this user can read, a plugin they installed can read too —
            // installing one is the trust decision, and it is asked about.
            if p.canonicalize().unwrap_or_else(|_| p.clone()) == crate::paths::credentials_file() {
                return Err("read_file: not that one".to_string());
            }
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            Value::String(text)
        }
        // The same reading of a shell command the harness itself uses, so a
        // policy plugin matches on what the command *is* rather than on what
        // its text happens to contain. Without this every plugin writes its own
        // `cmd.contains(pattern)`, which refuses prose in a commit message and
        // lets `rm -fr /` past.
        "command_segments" => Value::from(crate::policy::segments(
            arg.as_str().ok_or("command_segments needs a command")?,
        )),
        "command_matches" => {
            let cmd = arg
                .get("command")
                .and_then(Value::as_str)
                .ok_or("command_matches needs a command")?;
            let rules: Vec<String> = arg
                .get("rules")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            match crate::policy::denied(cmd, &rules) {
                Some(rule) => Value::String(rule.to_string()),
                None => Value::Null,
            }
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
            if let Some(r) = p
                .call_typed::<_, SystemPromptOut>(Hook::SystemPrompt, &cur)
                .ok()
            {
                cur.prompt = r.prompt;
            }
        }
        cur.prompt
    }

    fn before_request(&mut self, req: ChatRequest, turn: u32) -> ChatRequest {
        let mut cur = BeforeRequestIn { request: req, turn };
        for p in &mut self.plugins {
            if let Some(r) = p
                .call_typed::<_, BeforeRequestOut>(Hook::BeforeRequest, &cur)
                .ok()
            {
                cur.request = r.request;
            }
        }
        cur.request
    }

    fn before_tool(&mut self, call: &ToolCall, cwd: &str) -> (ToolDecision, crate::agent::Patches) {
        let mut input = BeforeToolIn {
            call: call.clone(),
            cwd: cwd.into(),
        };
        let mut decision = ToolDecision::Allow;
        let mut patches = Vec::new();
        for p in &mut self.plugins {
            let name = p.manifest.name.clone();
            let r = match p.call_typed::<_, BeforeToolOut>(Hook::BeforeTool, &input) {
                Called::Answered(r) => r,
                Called::Nothing => continue,
                // A policy that could not answer has not allowed anything. The
                // way to make a plugin fail is to send it something it cannot
                // handle, so treating a failure as an allow would mean the one
                // command a policy chokes on is the one command it lets past.
                Called::Failed(why) => {
                    return (
                        ToolDecision::Deny {
                            reason: format!("the {name} plugin could not decide: {why}"),
                        },
                        patches,
                    );
                }
            };
            if let Some(v) = r.settings_patch {
                patches.push((name, v));
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
    ) -> (ToolResult, crate::agent::Patches) {
        let mut input = AfterToolIn {
            call: call.clone(),
            result,
            duration_ms,
        };
        let mut patches = Vec::new();
        for p in &mut self.plugins {
            let Some(r) = p
                .call_typed::<_, AfterToolOut>(Hook::AfterTool, &input)
                .ok()
            else {
                continue;
            };
            if let Some(v) = r.settings_patch {
                patches.push((p.manifest.name.clone(), v));
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
            match p.call_typed::<_, ToolCallOut>(Hook::ToolCall, &input).ok() {
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

    fn on_turn_end(&mut self, input: OnTurnEndIn) -> (crate::agent::Patches, Vec<String>) {
        let mut patches = Vec::new();
        let mut notices = Vec::new();
        for p in &mut self.plugins {
            if let Some(r) = p
                .call_typed::<_, OnTurnEndOut>(Hook::OnTurnEnd, &input)
                .ok()
            {
                if let Some(v) = r.settings_patch {
                    patches.push((p.manifest.name.clone(), v));
                }
                if let Some(m) = r.message {
                    notices.push(m);
                }
            }
        }
        (patches, notices)
    }
}

/// Last load's settings patches, kept on disk so the next launch can paint the
/// right theme before a single module has been read.
///
/// A plugin's `on_load` output is a pure function of the wasm bytes, the
/// settings it was shown and the working directory. Key on all three and a hit
/// is exactly what this launch's load is about to produce; a miss just costs
/// what every launch used to cost.
pub mod cache {
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};

    use ah_abi::{Settings, SlashCommandSpec};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    /// What a load produced, minus the reports: a stale error is worse than no
    /// error, and the real load re-reports a few milliseconds later anyway.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct Cached {
        key: String,
        /// Plugins that loaded, for the startup banner.
        pub count: u32,
        /// `(plugin name, patch)` in apply order, as `load_plugins` returns them.
        pub patches: Vec<(String, Value)>,
        pub commands: Vec<(String, SlashCommandSpec)>,
    }

    fn file() -> PathBuf {
        crate::paths::data_dir().join("plugin-cache.json")
    }

    /// Wasm identity plus the inputs `on_load` is shown. Mtime and length stand
    /// in for the bytes: hashing 200 KB per launch would cost more than the
    /// load this saves.
    fn key(settings: &Settings, settings_value: &Value, cwd: &str) -> String {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // A different binary may drive the same plugin to a different answer.
        env!("CARGO_PKG_VERSION").hash(&mut h);
        cwd.hash(&mut h);
        for p in super::discover(settings, cwd) {
            p.hash(&mut h);
            match std::fs::metadata(&p) {
                Ok(m) => {
                    m.len().hash(&mut h);
                    m.modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .hash(&mut h);
                }
                // Unreadable now, unreadable at load: still a stable key.
                Err(_) => 0u64.hash(&mut h),
            }
        }
        // serde_json holds objects in a BTreeMap, so this text is stable.
        serde_json::to_string(settings_value)
            .unwrap_or_default()
            .hash(&mut h);
        format!("{:016x}", h.finish())
    }

    /// The cached load for these inputs, or `None` if anything has moved.
    pub fn read(settings: &Settings, settings_value: &Value, cwd: &str) -> Option<Cached> {
        read_at(&file(), settings, settings_value, cwd)
    }

    fn read_at(
        path: &Path,
        settings: &Settings,
        settings_value: &Value,
        cwd: &str,
    ) -> Option<Cached> {
        let text = std::fs::read_to_string(path).ok()?;
        let c: Cached = serde_json::from_str(&text).ok()?;
        (c.key == key(settings, settings_value, cwd)).then_some(c)
    }

    /// Record a load. Failure is silent: a cache that cannot be written costs
    /// the next launch some milliseconds, nothing else.
    pub fn write(
        settings: &Settings,
        settings_value: &Value,
        cwd: &str,
        count: u32,
        patches: &[(String, Value)],
        commands: &[(String, SlashCommandSpec)],
    ) {
        write_at(
            &file(),
            settings,
            settings_value,
            cwd,
            count,
            patches,
            commands,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn write_at(
        path: &Path,
        settings: &Settings,
        settings_value: &Value,
        cwd: &str,
        count: u32,
        patches: &[(String, Value)],
        commands: &[(String, SlashCommandSpec)],
    ) {
        let c = Cached {
            key: key(settings, settings_value, cwd),
            count,
            patches: patches.to_vec(),
            commands: commands.to_vec(),
        };
        let Ok(text) = serde_json::to_string(&c) else {
            return;
        };
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        // Via a temporary: a half-written cache must never be read back.
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn dir(tag: &str) -> PathBuf {
            let d = std::env::temp_dir().join(format!("ah-pcache-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        /// Settings pointing at one wasm file, so the key has something to key on.
        fn with(wasm: &Path) -> (Settings, Value) {
            let mut s = Settings::default();
            s.plugins.paths = vec![wasm.display().to_string()];
            let v = serde_json::to_value(&s).unwrap();
            (s, v)
        }

        fn patches() -> Vec<(String, Value)> {
            vec![(
                "themes".into(),
                serde_json::json!({"theme": {"accent": "blue"}}),
            )]
        }

        #[test]
        fn what_was_written_comes_back() {
            let d = dir("hit");
            let wasm = d.join("themes.wasm");
            std::fs::write(&wasm, b"\0asm").unwrap();
            let (s, v) = with(&wasm);
            let cache = d.join("cache.json");
            let cmds = vec![("themes".to_string(), SlashCommandSpec::default())];
            write_at(&cache, &s, &v, "/repo", 1, &patches(), &cmds);
            let got = read_at(&cache, &s, &v, "/repo").expect("hit");
            assert_eq!(got.count, 1);
            assert_eq!(got.patches, patches());
            assert_eq!(got.commands.len(), 1);
        }

        #[test]
        fn a_touched_plugin_misses() {
            let d = dir("mtime");
            let wasm = d.join("themes.wasm");
            std::fs::write(&wasm, b"\0asm").unwrap();
            let (s, v) = with(&wasm);
            let cache = d.join("cache.json");
            write_at(&cache, &s, &v, "/repo", 1, &patches(), &[]);
            assert!(read_at(&cache, &s, &v, "/repo").is_some());
            // Longer bytes: a rebuild that landed in the same nanosecond.
            std::fs::write(&wasm, b"\0asm\x01\x02\x03").unwrap();
            assert!(read_at(&cache, &s, &v, "/repo").is_none());
        }

        #[test]
        fn another_directory_or_setting_misses() {
            let d = dir("inputs");
            let wasm = d.join("themes.wasm");
            std::fs::write(&wasm, b"\0asm").unwrap();
            let (s, v) = with(&wasm);
            let cache = d.join("cache.json");
            write_at(&cache, &s, &v, "/repo", 1, &patches(), &[]);
            assert!(read_at(&cache, &s, &v, "/other").is_none());
            let mut other = s.clone();
            other.model.id = "some/other-model".into();
            let other_v = serde_json::to_value(&other).unwrap();
            assert!(read_at(&cache, &other, &other_v, "/repo").is_none());
        }

        #[test]
        fn a_corrupt_cache_is_simply_a_miss() {
            let d = dir("corrupt");
            let wasm = d.join("themes.wasm");
            std::fs::write(&wasm, b"\0asm").unwrap();
            let (s, v) = with(&wasm);
            let cache = d.join("cache.json");
            std::fs::write(&cache, "{ half a fi").unwrap();
            assert!(read_at(&cache, &s, &v, "/repo").is_none());
        }
    }
}
