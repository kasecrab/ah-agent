//! Built-in tools and the registry that dispatches model tool calls.

pub mod agent;
pub mod ask;
pub mod bash;
pub mod describe;
pub mod diff;
pub mod edit;
pub mod fs;
pub mod jobs;
pub mod plan;

use std::collections::HashMap;
use std::path::PathBuf;

use ah_abi::{Ask, Reply, ToolCall, ToolResult, ToolSettings, ToolSpec};
use serde_json::Value;

/// Execution context handed to every tool call.
pub struct ToolCtx<'a> {
    pub cwd: &'a PathBuf,
    pub settings: &'a ToolSettings,
    /// Which agent is calling: 0 for the main one, else a subagent id. It tags
    /// the background jobs a call starts, so news of one reaches the agent that
    /// started it and nobody else.
    pub agent: u32,
    /// Set when the user cancels the turn. A tool that waits for anything
    /// watches this and gives up at once.
    pub cancel: &'a std::sync::atomic::AtomicBool,
    /// The way to start subagents, for the one tool that does. `None` where
    /// there is nothing to start them with: a bare tool run, or a test.
    pub spawn: Option<&'a (dyn crate::agents::Spawner + Sync)>,
    /// The way to the person at the keyboard, for the one tool that asks them
    /// something. Read-only calls run side by side on scoped threads, so this
    /// is shared between threads even though only one tool ever uses it.
    pub ask: &'a (dyn AskUser + Sync),
}

/// How a tool reaches the user. The call blocks until they answer.
pub trait AskUser {
    fn ask(&self, ask: &Ask) -> Reply;
}

/// The asker for callers with nobody behind them: one-off tool runs and tests.
pub struct NoUser;

impl AskUser for NoUser {
    fn ask(&self, _ask: &Ask) -> Reply {
        Reply::Unavailable
    }
}

/// A borrow of [`NoUser`], to fill in [`ToolCtx::ask`].
pub fn no_user() -> &'static NoUser {
    &NoUser
}

/// A cancel flag for callers with nothing to cancel (one-off tool runs, tests).
pub fn never() -> &'static std::sync::atomic::AtomicBool {
    static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    &NEVER
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult;
    /// True when this call only reads, so it may run beside other calls from
    /// the same model message.
    fn parallel(&self, _args: &Value, _settings: &ToolSettings) -> bool {
        false
    }
}

/// Built-in tools plus dynamically registered ones (plugins).
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
    index: HashMap<String, usize>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// All built-ins, filtered by `settings.enabled` / `settings.disabled`.
    pub fn builtins(settings: &ToolSettings) -> Self {
        let mut r = Self::new();
        let all: Vec<Box<dyn Tool>> = vec![
            Box::new(ask::AskTool),
            Box::new(bash::Bash),
            Box::new(fs::ReadFile),
            Box::new(fs::WriteFile),
            Box::new(fs::EditFile),
            Box::new(jobs::JobsTool),
            Box::new(plan::PlanTool),
        ];
        for t in all {
            let name = t.spec().function.name.clone();
            if settings.enabled.contains(&name) && !settings.disabled.contains(&name) {
                r.register(t);
            }
        }
        r
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.spec().function.name.clone();
        if let Some(&i) = self.index.get(&name) {
            self.tools[i] = tool;
        } else {
            self.index.insert(name, self.tools.len());
            self.tools.push(tool);
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.spec().function.name).collect()
    }

    pub fn has(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// Whether `call` may run at the same time as its neighbours. Unknown tools
    /// (plugin tools, which run on the host's single interpreter) never do.
    pub fn is_parallel(&self, call: &ToolCall, settings: &ToolSettings) -> bool {
        let Some(&i) = self.index.get(&call.function.name) else {
            return false;
        };
        match serde_json::from_str::<Value>(&call.function.arguments) {
            Ok(args) => self.tools[i].parallel(&args, settings),
            Err(_) => false,
        }
    }

    pub fn run(&self, call: &ToolCall, ctx: &ToolCtx<'_>) -> ToolResult {
        let Some(&i) = self.index.get(&call.function.name) else {
            return ToolResult::err(format!("unknown tool: {}", call.function.name));
        };
        let args: Value = match serde_json::from_str(&call.function.arguments) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(format!("invalid JSON arguments: {e}")),
        };
        let mut res = self.tools[i].run(&args, ctx);
        res.output = truncate(&res.output, ctx.settings.max_output_bytes);
        res
    }
}

/// Keep head and tail when output exceeds `max` bytes.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max || max == 0 {
        return s.to_string();
    }
    let head_len = max * 2 / 3;
    let tail_len = max - head_len;
    let head = &s[..s.floor_char_boundary(head_len)];
    let tail = &s[s.ceil_char_boundary(s.len() - tail_len)..];
    format!(
        "{head}\n\n… [{} bytes truncated] …\n\n{tail}",
        s.len() - head.len() - tail.len()
    )
}

pub(crate) fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

pub(crate) fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

pub fn resolve_path(cwd: &std::path::Path, p: &str) -> PathBuf {
    let path = if let Some(rest) = p.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else {
        PathBuf::from(p)
    };
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_head_and_tail() {
        let s = "a".repeat(100) + &"b".repeat(100);
        let t = truncate(&s, 60);
        assert!(t.starts_with("aaaa"));
        assert!(t.ends_with("bbbb"));
        assert!(t.contains("truncated"));
        assert_eq!(truncate("short", 60), "short");
    }
}
