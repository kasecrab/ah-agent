//! One-shot mode and subcommands.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use ah_core::abi::*;
use ah_core::agent::{AgentEvent, AgentIo};
use ah_core::settings::Origin;

use crate::app::{self, AnyError, Engine};
use crate::plugin_source;
use crate::{Command, ConfigCmd, Overrides, PluginCmd};

struct PrintIo {
    json: bool,
    show_tools: bool,
}

impl AgentIo for PrintIo {
    fn emit(&self, ev: AgentEvent) {
        if self.json {
            let v = ah_core::agent::event_json(&ev);
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{v}");
            let _ = out.flush();
            return;
        }
        let mut err = std::io::stderr().lock();
        match ev {
            AgentEvent::Text(t) => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(printable(&t).as_bytes());
                let _ = out.flush();
            }
            AgentEvent::Reasoning(_) => {}
            AgentEvent::ToolStart(c) if self.show_tools => {
                let _ = writeln!(err, "\x1b[33m⚙\x1b[0m {}", describe_call(&c));
            }
            AgentEvent::ToolEnd {
                result,
                duration_ms,
                ..
            } if self.show_tools => {
                let first = printable(result.output.lines().next().unwrap_or(""))
                    .chars()
                    .take(120)
                    .collect::<String>();
                let color = if result.is_error { "31" } else { "90" };
                let took = if duration_ms == 0 {
                    "<1 ms".to_string()
                } else {
                    format!("{duration_ms} ms")
                };
                let _ = writeln!(err, "\x1b[{color}m  ↳ {first} ({took})\x1b[0m");
                if let Some(d) = &result.diff {
                    for l in d.lines().take(40) {
                        let c = match l.as_bytes().first() {
                            Some(b'+') => "32",
                            Some(b'-') => "31",
                            Some(b'@') => "36",
                            _ => "90",
                        };
                        let _ = writeln!(err, "\x1b[{c}m  {l}\x1b[0m");
                    }
                    let n = d.lines().count();
                    if n > 40 {
                        let _ = writeln!(err, "\x1b[90m  … {} more lines\x1b[0m", n - 40);
                    }
                }
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
            AgentEvent::Compacting { auto } => {
                let why = if auto { " (context full)" } else { "" };
                let _ = writeln!(err, "\x1b[90mcompacting context{why}…\x1b[0m");
            }
            // A tick's worth of summary; a scrolling printer has nowhere to
            // put it, so only the TUI and the JSONL stream carry it.
            AgentEvent::CompactProgress { .. } => {}
            AgentEvent::Compacted { before, after, .. } => {
                let _ = writeln!(
                    err,
                    "\x1b[90mcontext compacted: {before} → ~{after} tokens\x1b[0m"
                );
            }
            AgentEvent::Notice(n) => {
                let _ = writeln!(err, "\x1b[90m{n}\x1b[0m");
            }
            AgentEvent::AssistantMessage(m) => {
                if !m.content.is_empty() {
                    let _ = writeln!(std::io::stdout());
                }
            }
            // The path goes to stdout, on its own line: with an image model
            // stdout would otherwise be empty, and `ah -p "draw a cat" | xargs
            // feh` is the whole point of a one-shot run.
            AgentEvent::Image {
                path,
                mime,
                width,
                height,
                bytes,
            } => {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{}", path.display());
                let _ = out.flush();
                let dims = if width > 0 {
                    format!("{width}x{height} ")
                } else {
                    String::new()
                };
                let _ = writeln!(
                    err,
                    "\x1b[90m[image {dims}{mime}, {} KB]\x1b[0m",
                    bytes.div_ceil(1024)
                );
            }
            AgentEvent::TurnEnd(s) => {
                let cached = if s.usage.cached_tokens > 0 {
                    format!(" ({} cached)", s.usage.cached_tokens)
                } else {
                    String::new()
                };
                // What the agents spent is theirs, not this loop's; it is part
                // of what the turn cost all the same.
                let agents = ah_core::agents::table().take_spent();
                let agents = (agents.cost > 0.0).then(|| format!(" +${:.4} agents", agents.cost));
                let _ = writeln!(
                    err,
                    "\x1b[90m[{} req, {} tools, ↑{}{} ↓{} ${:.4}{}]\x1b[0m",
                    s.requests,
                    s.tool_calls,
                    s.usage.prompt_tokens,
                    cached,
                    s.usage.completion_tokens,
                    s.usage.cost,
                    agents.unwrap_or_default()
                );
            }
            // A one-shot run has no settings stack to fold this into after the
            // turn has started, and quietly doing nothing would leave the
            // plugin's author believing it took. Say so instead.
            AgentEvent::SettingsPatch { plugin, patch } => {
                let keys = match patch.as_object() {
                    Some(o) => o.keys().cloned().collect::<Vec<_>>().join(", "),
                    None => String::new(),
                };
                let _ = writeln!(
                    err,
                    "\x1b[33mplugin {plugin} returned a settings patch ({keys}); a one-shot run \
                     does not apply one mid-turn\x1b[0m"
                );
            }
            _ => {}
        }
    }

    fn ask_user(&self, ask: &Ask) -> Reply {
        // A piped or scripted run has nobody to type an answer, and a question
        // nobody can see would stop the turn for good.
        if self.json || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            return Reply::Unavailable;
        }
        let mut err = std::io::stderr().lock();
        let mut answers = Vec::with_capacity(ask.questions.len());
        for q in &ask.questions {
            let _ = writeln!(err, "\x1b[33m? {}\x1b[0m", q.question.trim());
            for (i, o) in q.options.iter().enumerate() {
                let tail = if o.description.is_empty() {
                    String::new()
                } else {
                    format!("  \x1b[90m{}\x1b[0m", o.description)
                };
                let _ = writeln!(err, "  {}) {}{tail}", i + 1, o.label);
            }
            let how = match (q.options.is_empty(), q.multi) {
                (true, _) => "answer",
                (false, false) => "number or answer",
                (false, true) => "numbers or answer",
            };
            let _ = write!(err, "  [{how}] ");
            let _ = err.flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return Reply::Dismissed;
            }
            let line = line.trim();
            if line.is_empty() {
                return Reply::Dismissed;
            }
            answers.push(answer(q, line));
        }
        Reply::Answered { answers }
    }

    fn ask_permission(&self, call: &ToolCall, reason: &str) -> bool {
        // Whether this call needed asking about was decided before the
        // question reached here. A plugin's `ask`, and the prompt for a write
        // to a file that decides what runs next, are raised in every mode — so
        // answering `true` on the strength of `permissions.mode` would make
        // the one mechanism a plugin has for forcing a human decision a no-op
        // in exactly the mode where it matters.
        //
        // With nobody to ask, the answer is no. A one-shot run in a pipeline
        // that refuses is a turn wasted; one that silently approves is the
        // thing the question existed to prevent.
        if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(
                err,
                "\x1b[33m✗ {} needed asking about{}{} and there is no terminal to ask at\x1b[0m",
                describe_call(call),
                if reason.is_empty() { "" } else { " — " },
                printable(reason),
            );
            return false;
        }
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "\x1b[33m? {}\x1b[0m", describe_call(call));
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

/// One typed line as an answer: the numbers of the options it names, or, when
/// it names none of them, the line itself.
fn answer(q: &Question, line: &str) -> Answer {
    let picked: Vec<String> = line
        .split([',', ' '])
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<usize>().ok())
        .filter(|n| (1..=q.options.len()).contains(n))
        .map(|n| q.options[n - 1].label.clone())
        .take(if q.multi { q.options.len() } else { 1 })
        .fold(Vec::new(), |mut v, l| {
            if !v.contains(&l) {
                v.push(l);
            }
            v
        });
    // Every word a number means the line picked options; anything else is an
    // answer of the user's own, even where it happens to contain a number.
    let numbers = line
        .split([',', ' '])
        .filter(|t| !t.is_empty())
        .all(|t| t.parse::<usize>().is_ok());
    if picked.is_empty() || !numbers {
        Answer {
            picked: Vec::new(),
            note: line.to_string(),
        }
    } else {
        Answer {
            picked,
            note: String::new(),
        }
    }
}

/// What a tool call is doing, in plain English where ah knows the tool.
/// Text on its way to a terminal, with anything that is not text removed.
///
/// The TUI is safe here because ratatui draws characters and drops the rest.
/// This path writes straight to the terminal, so a tool result — the contents
/// of a file, the output of a command, anything a model was told to say — could
/// otherwise set the window title, rewrite the line above, or ask the terminal
/// to put something in the clipboard.
///
/// Newlines and tabs stay: they are what the output is made of.
pub fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

pub fn describe_call(call: &ah_core::abi::ToolCall) -> String {
    // The arguments are whatever the model wrote, and this line goes to a
    // terminal.
    printable(
        &serde_json::from_str(&call.function.arguments)
            .ok()
            .and_then(|v| ah_core::tools::describe::describe(&call.function.name, &v))
            .unwrap_or_else(|| {
                format!(
                    "{}({})",
                    call.function.name,
                    compact_args(&call.function.arguments)
                )
            }),
    )
}

#[cfg(test)]
mod printing {
    use super::*;

    #[test]
    fn nothing_a_tool_returns_can_drive_the_terminal() {
        let nasty = "ok\u{1b}]0;owned\u{7}\u{1b}]52;c;cGF5bG9hZA==\u{7}\rgone";
        let out = printable(nasty);
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(!out.contains('\u{7}'), "{out:?}");
        assert!(!out.contains('\r'), "{out:?}");
        // What the output is actually made of stays.
        assert_eq!(printable("one\ntwo\tthree"), "one\ntwo\tthree");
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

pub fn one_shot(
    o: &Overrides,
    resume: Option<&str>,
    prompt: &str,
    json: bool,
) -> Result<(), AnyError> {
    let cwd = app::resolve_cwd(o)?;
    let mut stack = app::load_settings(o)?;
    let mut said = 0;
    for line in app::refusals(&stack) {
        said += 1;
        eprintln!("\x1b[33m{line}\x1b[0m");
    }
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
    // Again, because a plugin is one of the things that can be refused and it
    // was not on the stack when the first pass ran.
    for line in app::refusals(&stack).into_iter().skip(said) {
        eprintln!("\x1b[33m{line}\x1b[0m");
    }
    engine.apply_settings(stack.settings().clone(), stack.value().clone());

    install_ctrlc(engine.cancel.clone());

    let io = PrintIo {
        json,
        show_tools: true,
    };
    let res = engine.run_turn(prompt.to_string(), Vec::new(), &io);
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

pub fn install_ctrlc(cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) {
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
            modality,
            filter,
            refresh,
        } => models(o, tools, modality, filter, refresh),
        Command::Plugin { cmd } => plugin(o, cmd),
        Command::Config { cmd } => config(o, cmd.unwrap_or(ConfigCmd::Show { origins: false })),
        Command::Docs { topic } => docs(topic.as_deref()),
        #[cfg(feature = "remote")]
        Command::Remote { cmd } => crate::remote::cli::subcommand(cmd, o),
        Command::Sessions => {
            for s in ah_core::session::summaries() {
                println!(
                    "{}\t{:>4} msg\t{}\t{}\t{}",
                    s.id,
                    s.messages,
                    s.model,
                    s.cwd,
                    s.name.as_deref().unwrap_or(&s.title)
                );
            }
            Ok(())
        }
    }
}

fn login(o: &Overrides, key: Option<String>) -> Result<(), AnyError> {
    use ah_core::auth::{self, Source};
    let stack = app::load_settings(o)?;
    let base = stack.settings().model.base_url.clone();
    let settings_key = stack.settings().model.api_key.clone();
    let creds = ah_core::paths::credentials_file();

    let active = auth::resolve(settings_key.as_deref());
    match &active {
        Some((k, src)) => println!("logged in: {} ({})", auth::masked(k), src.describe()),
        None => println!("not logged in"),
    }

    let interactive = std::io::stdin().is_terminal();
    let piped = (key.is_none() && !interactive).then(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_string()
    });
    let stored = auth::stored_key();
    let key = match (key, piped) {
        (Some(k), _) => k,
        (None, Some(k)) if !k.is_empty() => k,
        // an exported key becomes the stored one without any typing
        _ if matches!(&active, Some((k, Source::Env)) if stored.as_deref() != Some(k.as_str())) => {
            println!("storing the environment key so it works without the variable");
            active.as_ref().map(|(k, _)| k.clone()).unwrap_or_default()
        }
        _ if !interactive => String::new(),
        _ if active.is_some() => {
            println!("paste a new key to replace it, or press Enter to keep it");
            read_secret("OpenRouter API key: ")?
        }
        _ => {
            println!("create a key at https://openrouter.ai/settings/keys");
            read_secret("OpenRouter API key: ")?
        }
    };
    let key = key.trim().to_string();
    if key.is_empty() {
        if active.is_none() {
            return Err("no key given".into());
        }
        println!("kept the current key");
        return Ok(());
    }
    if key.contains(char::is_whitespace) {
        return Err("a key has no spaces; check what was pasted".into());
    }

    match auth::verify(&base, &key) {
        Ok(info) => {
            let label = if info.label.is_empty() {
                String::new()
            } else {
                format!(" `{}`", info.label)
            };
            let limit = match info.limit {
                Some(l) => format!(" of ${l:.2}"),
                None => String::new(),
            };
            println!("key ok:{label} ${:.4} used{limit}", info.usage);
        }
        Err(ah_core::Error::Api { status: 401, .. }) => {
            return Err("OpenRouter rejected this key (401); nothing saved".into());
        }
        Err(e) => println!("could not verify the key ({e}); saving anyway"),
    }
    auth::save_key(&key)?;
    println!(
        "saved {} to {} (mode 600)",
        auth::masked(&key),
        creds.display()
    );
    Ok(())
}

/// Read a line without echo. Falls back to a plain line when the terminal
/// cannot enter raw mode.
pub fn read_secret(prompt: &str) -> Result<String, AnyError> {
    use crossterm::event::{Event, KeyCode, KeyModifiers, read};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "{prompt}");
    let _ = err.flush();
    if enable_raw_mode().is_err() {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        return Ok(line);
    }
    let mut buf = String::new();
    let mut cancelled = false;
    loop {
        let Ok(Event::Key(k)) = read() else { continue };
        if !k.kind.is_press() {
            continue;
        }
        match (k.code, k.modifiers) {
            (KeyCode::Enter, _) => break,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
                cancelled = true;
                break;
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => buf.clear(),
            (KeyCode::Backspace, _) => {
                buf.pop();
            }
            (KeyCode::Char(c), m) if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                buf.push(c)
            }
            _ => {}
        }
    }
    let _ = disable_raw_mode();
    let _ = writeln!(err);
    if cancelled {
        return Err("cancelled".into());
    }
    Ok(buf)
}

fn models(
    o: &Overrides,
    tools_only: bool,
    modality: Option<String>,
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
    // The catalogue holds every kind of model, so an unknown word here would
    // quietly list nothing at all.
    if let Some(m) = &modality
        && !ah_core::models::outputs().any(|(n, _)| n == m)
    {
        let known: Vec<&str> = ah_core::models::outputs().map(|(n, _)| n).collect();
        return Err(format!("unknown modality {m:?}; try one of: {}", known.join(", ")).into());
    }
    let all = ah_core::models::load(base, key.as_deref(), max_age)?;
    let filtered: Vec<&ah_core::models::ModelInfo> = match &filter {
        Some(q) => ah_core::models::rank(q, &all, |m| format!("{} {}", m.id, m.name)),
        None => all.iter().collect(),
    };
    println!(
        "{:<50} {:>8} {:>8} {:>8}  caps",
        "id", "context", "$in/M", "$out/M"
    );
    for m in filtered
        .into_iter()
        .filter(|m| !tools_only || m.tools)
        .filter(|m| modality.as_deref().is_none_or(|k| m.produces(k)))
    {
        // A model that only draws prices nothing under completion; printing
        // that as 0.00 would read as free.
        let out = if m.completion_per_m > 0.0 {
            m.completion_per_m
        } else {
            m.image_out_per_m
        };
        println!(
            "{:<50} {:>8} {:>8.2} {:>8.2}  {}{}{}",
            m.id,
            m.context_length,
            m.prompt_per_m,
            out,
            m.modality_icons(),
            if m.tools { " tools" } else { "" },
            if m.reasoning { " reasoning" } else { "" }
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
            let mut p = user_dir.join(&file);
            if !p.exists() {
                let alt = user_dir.join(file.replace('-', "_"));
                if !alt.exists() {
                    return Err(format!("{} not found", p.display()).into());
                }
                p = alt;
            }
            std::fs::remove_file(&p)?;
            println!("removed {}", p.display());
            let mut sources = plugin_source::load_sources(&user_dir);
            if sources.remove(&plugin_source::stem(&p)).is_some() {
                plugin_source::save_sources(&user_dir, &sources)?;
            }
            Ok(())
        }
        PluginCmd::Build { dir, no_install } => {
            let dir = dir.unwrap_or(PathBuf::from("."));
            for f in plugin_source::build_crate(&dir, None)? {
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
        PluginCmd::Install {
            source,
            subdir,
            git_ref,
        } => {
            let src = plugin_source::parse_source(&source, subdir.as_deref(), git_ref.as_deref());
            install_from(&src, &user_dir)
        }
        PluginCmd::Update { name } => {
            let sources = plugin_source::load_sources(&user_dir);
            let wanted: Vec<(String, plugin_source::Source)> = match name {
                Some(n) => {
                    let key = n.trim_end_matches(".wasm").replace('-', "_");
                    let src = sources
                        .get(&key)
                        .or_else(|| sources.get(n.trim_end_matches(".wasm")))
                        .ok_or_else(|| {
                            format!(
                                "no recorded source for `{n}`; install it with `ah plugin install`"
                            )
                        })?;
                    vec![(key, src.clone())]
                }
                None => sources.into_iter().collect(),
            };
            if wanted.is_empty() {
                println!("no plugins were installed from git");
                return Ok(());
            }
            for (name, src) in wanted {
                println!("updating {name} from {}", src.describe());
                install_from(&src, &user_dir)?;
            }
            Ok(())
        }
    }
}

fn install_from(src: &plugin_source::Source, user_dir: &std::path::Path) -> Result<(), AnyError> {
    // What was installed last time, so an update can tell whether the author
    // has pushed since and ask before running what they pushed.
    let known = src.commit.clone();
    let (installed, commit) = plugin_source::install(src, user_dir, known.as_deref())?;
    let mut sources = plugin_source::load_sources(user_dir);
    let mut src = src.clone();
    src.commit = Some(commit);
    for f in &installed {
        sources.insert(plugin_source::stem(f), src.clone());
        println!("installed {} (from {})", f.display(), src.describe());
    }
    plugin_source::save_sources(user_dir, &sources)?;
    println!("run /reload in a running ah, or start it again, to load the plugin");
    Ok(())
}

fn docs(topic: Option<&str>) -> Result<(), AnyError> {
    use ah_core::docs;
    match topic {
        None => {
            println!("{}\n\nah docs <topic> prints a page.", docs::list());
            Ok(())
        }
        Some(t) => match docs::find(t) {
            Some(page) => {
                print!("{}", page.text);
                Ok(())
            }
            None => Err(format!("no docs topic `{t}`; topics:\n{}", docs::list()).into()),
        },
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
            // This is the command somebody runs to ask why a setting did
            // nothing, so it is the one place the answer has to be.
            for line in app::refusals(&stack) {
                println!("# {line}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use ah_core::abi::Choice;

    fn question(multi: bool) -> Question {
        Question {
            header: String::new(),
            question: "Which?".into(),
            options: ["a", "b", "c"]
                .iter()
                .map(|l| Choice {
                    label: (*l).into(),
                    description: String::new(),
                })
                .collect(),
            multi,
        }
    }

    #[test]
    fn a_typed_line_is_read_as_numbers_or_as_an_answer() {
        let one = question(false);
        assert_eq!(answer(&one, "2").picked, ["b"]);
        assert_eq!(answer(&one, "2 3").picked, ["b"], "one choice, the first");
        // Out of range, or words: the line stands as the answer itself.
        assert_eq!(answer(&one, "9").note, "9");
        assert_eq!(
            answer(&one, "b, but only for now").note,
            "b, but only for now"
        );
        assert!(answer(&one, "b, but only for now").picked.is_empty());
        let many = question(true);
        assert_eq!(answer(&many, "1, 3").picked, ["a", "c"]);
        assert_eq!(answer(&many, "1 1 3").picked, ["a", "c"], "no repeats");
    }
}
