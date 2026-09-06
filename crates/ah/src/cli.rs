//! One-shot mode and subcommands.

use std::io::Write;
use std::path::PathBuf;

use ah_core::abi::*;
use ah_core::agent::{AgentEvent, AgentIo};
use ah_core::settings::Origin;

use crate::app::{self, AnyError, Engine};
use crate::{Command, ConfigCmd, Overrides, PluginCmd};

struct PrintIo {
    json: bool,
    ask: bool,
    show_tools: bool,
}

impl AgentIo for PrintIo {
    fn emit(&self, ev: AgentEvent) {
        if self.json {
            let v = event_json(&ev);
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{v}");
            let _ = out.flush();
            return;
        }
        let mut err = std::io::stderr().lock();
        match ev {
            AgentEvent::Text(t) => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(t.as_bytes());
                let _ = out.flush();
            }
            AgentEvent::Reasoning(_) => {}
            AgentEvent::ToolStart(c) if self.show_tools => {
                let _ = writeln!(
                    err,
                    "\x1b[33m⚙ {}\x1b[0m {}",
                    c.function.name,
                    compact_args(&c.function.arguments)
                );
            }
            AgentEvent::ToolEnd {
                result,
                duration_ms,
                ..
            } if self.show_tools => {
                let first = result
                    .output
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect::<String>();
                let color = if result.is_error { "31" } else { "90" };
                let _ = writeln!(err, "\x1b[{color}m  ↳ {first} ({duration_ms} ms)\x1b[0m");
            }
            AgentEvent::ToolDenied { call, reason } => {
                let _ = writeln!(
                    err,
                    "\x1b[31m✗ {} denied: {reason}\x1b[0m",
                    call.function.name
                );
            }
            AgentEvent::Retry {
                attempt,
                wait_ms,
                error,
            } => {
                let _ = writeln!(
                    err,
                    "\x1b[33mretry {attempt} in {wait_ms} ms: {error}\x1b[0m"
                );
            }
            AgentEvent::Error(e) => {
                let _ = writeln!(err, "\x1b[31merror: {e}\x1b[0m");
            }
            AgentEvent::Notice(n) => {
                let _ = writeln!(err, "\x1b[90m{n}\x1b[0m");
            }
            AgentEvent::AssistantMessage(m) => {
                if !m.content.is_empty() {
                    let _ = writeln!(std::io::stdout());
                }
            }
            AgentEvent::TurnEnd(s) => {
                let _ = writeln!(
                    err,
                    "\x1b[90m[{} req, {} tools, ↑{} ↓{} ${:.4}]\x1b[0m",
                    s.requests,
                    s.tool_calls,
                    s.usage.prompt_tokens,
                    s.usage.completion_tokens,
                    s.usage.cost
                );
            }
            _ => {}
        }
    }

    fn ask_permission(&self, call: &ToolCall, reason: &str) -> bool {
        if !self.ask {
            return true;
        }
        let mut err = std::io::stderr().lock();
        let _ = writeln!(
            err,
            "\x1b[33m? run {} {}\x1b[0m",
            call.function.name,
            compact_args(&call.function.arguments)
        );
        if !reason.is_empty() {
            let _ = writeln!(err, "  {reason}");
        }
        let _ = write!(err, "  [y/N] ");
        let _ = err.flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes")
    }
}

pub fn compact_args(args: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    let s = match &v {
        serde_json::Value::Object(m) => m
            .iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}={}", one_line(s, 80)),
                other => format!("{k}={other}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => args.to_string(),
    };
    one_line(&s, 160)
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { '⏎' } else { c }).collect();
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

fn event_json(ev: &AgentEvent) -> serde_json::Value {
    use serde_json::json;
    match ev {
        AgentEvent::RequestStart { turn } => json!({"type": "request_start", "turn": turn}),
        AgentEvent::Text(t) => json!({"type": "text", "text": t}),
        AgentEvent::Reasoning(t) => json!({"type": "reasoning", "text": t}),
        AgentEvent::AssistantMessage(m) => json!({"type": "assistant", "message": m}),
        AgentEvent::Usage(u) => json!({"type": "usage", "usage": u}),
        AgentEvent::ToolStart(c) => json!({"type": "tool_start", "call": c}),
        AgentEvent::ToolEnd {
            call,
            result,
            duration_ms,
        } => {
            json!({"type": "tool_end", "call": call, "result": result, "duration_ms": duration_ms})
        }
        AgentEvent::ToolDenied { call, reason } => {
            json!({"type": "tool_denied", "call": call, "reason": reason})
        }
        AgentEvent::ToolMessage(m) => json!({"type": "tool_message", "message": m}),
        AgentEvent::Notice(n) => json!({"type": "notice", "text": n}),
        AgentEvent::SettingsPatch(p) => json!({"type": "settings_patch", "patch": p}),
        AgentEvent::Retry {
            attempt,
            wait_ms,
            error,
        } => json!({"type": "retry", "attempt": attempt, "wait_ms": wait_ms, "error": error}),
        AgentEvent::Error(e) => json!({"type": "error", "error": e}),
        AgentEvent::TurnEnd(s) => {
            json!({"type": "turn_end", "requests": s.requests, "tool_calls": s.tool_calls, "usage": s.usage, "cancelled": s.cancelled})
        }
    }
}

pub fn one_shot(
    o: &Overrides,
    resume: Option<&str>,
    prompt: &str,
    json: bool,
) -> Result<(), AnyError> {
    let cwd = app::resolve_cwd(o)?;
    let mut stack = app::load_settings(o)?;
    let mut engine = Engine::new(&stack, cwd, resume, resume.is_some())?;
    let (reports, patches, _) = engine.load_plugins();
    for r in &reports {
        if !r.ok && r.message != "disabled" {
            eprintln!("\x1b[33mplugin {}: {}\x1b[0m", r.name, r.message);
        }
    }
    for (name, p) in patches {
        stack.push(Origin::Plugin(name), p)?;
    }
    engine.apply_settings(stack.settings().clone(), stack.value().clone());

    install_ctrlc(engine.cancel.clone());

    let io = PrintIo {
        json,
        ask: stack.settings().permissions.mode == PermissionMode::Ask,
        show_tools: true,
    };
    let res = engine.run_turn(prompt.to_string(), &io);
    for (p, lvl, m) in engine.take_plugin_logs() {
        if lvl <= LogLevel::Warn {
            eprintln!("\x1b[90m[{p}] {m}\x1b[0m");
        }
    }
    match res {
        Ok(s) if s.cancelled => std::process::exit(130),
        Ok(_) => Ok(()),
        Err(ah_core::Error::Auth(_)) => std::process::exit(2),
        Err(e) => Err(e.into()),
    }
}

fn install_ctrlc(cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    // SIGINT flips the cancel flag; a second one exits.
    #[cfg(unix)]
    {
        use std::sync::atomic::Ordering;
        static CANCEL: std::sync::OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> =
            std::sync::OnceLock::new();
        let _ = CANCEL.set(cancel);
        extern "C" fn handler(_: i32) {
            if let Some(c) = CANCEL.get()
                && c.swap(true, Ordering::SeqCst)
            {
                std::process::exit(130);
            }
        }
        unsafe extern "C" {
            fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
        }
        unsafe {
            signal(2, handler);
        }
    }
    #[cfg(not(unix))]
    let _ = cancel;
}

pub fn subcommand(cmd: Command, o: &Overrides) -> Result<(), AnyError> {
    match cmd {
        Command::Login { key } => login(o, key),
        Command::Logout => {
            ah_core::auth::clear_key()?;
            println!("removed {}", ah_core::paths::credentials_file().display());
            Ok(())
        }
        Command::Models {
            tools,
            filter,
            refresh,
        } => models(o, tools, filter, refresh),
        Command::Plugin { cmd } => plugin(o, cmd),
        Command::Config { cmd } => config(o, cmd.unwrap_or(ConfigCmd::Show { origins: false })),
        Command::Sessions => {
            for s in ah_core::session::summaries() {
                println!(
                    "{}\t{:>4} msg\t{}\t{}\t{}",
                    s.id, s.messages, s.model, s.cwd, s.title
                );
            }
            Ok(())
        }
    }
}

fn login(o: &Overrides, key: Option<String>) -> Result<(), AnyError> {
    if let Some(k) = key {
        ah_core::auth::save_key(k.trim())?;
        println!(
            "saved key to {}",
            ah_core::paths::credentials_file().display()
        );
        return Ok(());
    }
    let stack = app::load_settings(o)?;
    let base = stack.settings().model.base_url.clone();
    let key = ah_core::auth::login_pkce(
        &base,
        &|url| {
            eprintln!("Open this URL to authorize ah:\n\n  {url}\n");
            let _ = std::process::Command::new("xdg-open")
                .arg(url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        },
        std::time::Duration::from_secs(300),
    )?;
    println!(
        "logged in; key {}… saved to {}",
        &key[..key.len().min(12)],
        ah_core::paths::credentials_file().display()
    );
    Ok(())
}

fn models(
    o: &Overrides,
    tools_only: bool,
    filter: Option<String>,
    refresh: bool,
) -> Result<(), AnyError> {
    let stack = app::load_settings(o)?;
    let key = ah_core::auth::api_key(stack.settings().model.api_key.as_deref());
    let base = &stack.settings().model.base_url;
    let max_age = if refresh {
        std::time::Duration::ZERO
    } else {
        std::time::Duration::from_secs(24 * 3600)
    };
    let all = ah_core::models::load(base, key.as_deref(), max_age)?;
    let filtered: Vec<&ah_core::models::ModelInfo> = match &filter {
        Some(q) => ah_core::models::rank(q, &all, |m| format!("{} {}", m.id, m.name)),
        None => all.iter().collect(),
    };
    println!(
        "{:<50} {:>8} {:>8} {:>8}  caps",
        "id", "context", "$in/M", "$out/M"
    );
    for m in filtered.into_iter().filter(|m| !tools_only || m.tools) {
        println!(
            "{:<50} {:>8} {:>8.2} {:>8.2}  {}{}",
            m.id,
            m.context_length,
            m.prompt_per_m,
            m.completion_per_m,
            if m.tools { "tools " } else { "" },
            if m.reasoning { "reasoning" } else { "" }
        );
    }
    eprintln!("(cache: {})", ah_core::models::cache_path().display());
    Ok(())
}

fn plugin(o: &Overrides, cmd: PluginCmd) -> Result<(), AnyError> {
    let user_dir = ah_core::paths::config_dir().join("plugins");
    match cmd {
        PluginCmd::List => {
            let mut stack = app::load_settings(o)?;
            let cwd = app::resolve_cwd(o)?;
            let mut engine = Engine::new(&stack, cwd, None, false)?;
            let (reports, patches, commands) = engine.load_plugins();
            for (name, p) in patches {
                stack.push(Origin::Plugin(name), p)?;
            }
            if reports.is_empty() {
                println!("no plugins found in:");
                for d in ah_core::paths::plugin_dirs() {
                    println!("  {}", d.display());
                }
                return Ok(());
            }
            for r in reports {
                println!(
                    "{} {:<20} {}  [{}]",
                    if r.ok { "✓" } else { "✗" },
                    r.name,
                    r.message,
                    r.path.display()
                );
            }
            if let Some(h) = &engine.host {
                for p in h.plugins() {
                    if !p.manifest.tools.is_empty() {
                        println!(
                            "    {} tools: {}",
                            p.name(),
                            p.manifest
                                .tools
                                .iter()
                                .map(|t| t.name().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }
            }
            for (p, c) in commands {
                println!("    /{:<12} {} ({p})", c.name, c.description);
            }
            Ok(())
        }
        PluginCmd::Add { path } => {
            std::fs::create_dir_all(&user_dir)?;
            let name = path.file_name().ok_or("bad path")?;
            let dest = user_dir.join(name);
            std::fs::copy(&path, &dest)?;
            println!("installed {}", dest.display());
            Ok(())
        }
        PluginCmd::Rm { name } => {
            let file = if name.ends_with(".wasm") {
                name.clone()
            } else {
                format!("{name}.wasm")
            };
            let p = user_dir.join(&file);
            if !p.exists() {
                let alt = user_dir.join(file.replace('-', "_"));
                if alt.exists() {
                    std::fs::remove_file(&alt)?;
                    println!("removed {}", alt.display());
                    return Ok(());
                }
                return Err(format!("{} not found", p.display()).into());
            }
            std::fs::remove_file(&p)?;
            println!("removed {}", p.display());
            Ok(())
        }
        PluginCmd::Build { dir, no_install } => {
            let dir = dir.unwrap_or(PathBuf::from("."));
            let status = std::process::Command::new("cargo")
                .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
                .current_dir(&dir)
                .status()?;
            if !status.success() {
                return Err("cargo build failed (is the wasm32-unknown-unknown target installed? `rustup target add wasm32-unknown-unknown`)".into());
            }
            let mut candidates = vec![dir.join("target/wasm32-unknown-unknown/release")];
            if let Some(parent) = dir.parent() {
                candidates.push(parent.join("target/wasm32-unknown-unknown/release"));
            }
            let mut found = Vec::new();
            for c in candidates {
                if let Ok(rd) = std::fs::read_dir(&c) {
                    found.extend(
                        rd.flatten()
                            .map(|e| e.path())
                            .filter(|p| p.extension().is_some_and(|e| e == "wasm")),
                    );
                }
            }
            if found.is_empty() {
                return Err("build succeeded but no .wasm found".into());
            }
            for f in found {
                if no_install {
                    println!("{}", f.display());
                } else {
                    std::fs::create_dir_all(&user_dir)?;
                    let dest = user_dir.join(f.file_name().unwrap());
                    std::fs::copy(&f, &dest)?;
                    println!("installed {}", dest.display());
                }
            }
            Ok(())
        }
    }
}

fn config(o: &Overrides, cmd: ConfigCmd) -> Result<(), AnyError> {
    match cmd {
        ConfigCmd::Show { origins } => {
            let mut stack = app::load_settings(o)?;
            if stack.settings().plugins.enabled {
                let cwd = app::resolve_cwd(o)?;
                let mut engine = Engine::new(&stack, cwd, None, false)?;
                let (_, patches, _) = engine.load_plugins();
                for (name, p) in patches {
                    stack.push(Origin::Plugin(name), p)?;
                }
            }
            if origins {
                println!("# layers, in order:");
                for l in stack.layers() {
                    println!("#   {:?}", l.origin);
                }
            }
            print!("{}", stack.to_toml());
            Ok(())
        }
        ConfigCmd::Path => {
            println!(
                "user config:    {}",
                ah_core::paths::user_config_file().display()
            );
            println!(
                "project config: {}",
                ah_core::paths::project_config_file().display()
            );
            println!(
                "credentials:    {}",
                ah_core::paths::credentials_file().display()
            );
            println!(
                "plugins:        {}",
                ah_core::paths::plugin_dirs()
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!(
                "sessions:       {}",
                ah_core::paths::sessions_dir().display()
            );
            Ok(())
        }
        ConfigCmd::Init { force } => {
            let path = ah_core::paths::user_config_file();
            if path.exists() && !force {
                return Err(format!("{} exists (use --force)", path.display()).into());
            }
            std::fs::create_dir_all(path.parent().unwrap())?;
            let body = format!(
                "# ah configuration. Every key is optional; these are the defaults.\n# Plugins and `--set key=value` layer on top. `\"__unset__\"` removes a key.\n\n{}",
                ah_core::settings::SettingsStack::new().to_toml()
            );
            std::fs::write(&path, body)?;
            println!("wrote {}", path.display());
            Ok(())
        }
    }
}
