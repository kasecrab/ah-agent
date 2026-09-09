//! `agent` starts subagents; `agents` looks after them.
//!
//! The pair works like `bash` and `jobs`: one starts the work and can wait for
//! it, the other lists, reads, waits, stops and talks to what is still going.

use std::sync::Arc;
use std::time::Duration;

use ah_abi::{ToolResult, ToolSettings, ToolSpec};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, arg_str, arg_u64};
use crate::agents::{Child, SpawnRequest, State, follow_up, table};

/// What the model is told about the agent types it can choose between.
#[derive(Debug, Clone, Default)]
pub struct Types(pub Vec<(String, String)>);

impl Types {
    fn sentence(&self) -> String {
        if self.0.is_empty() {
            return String::new();
        }
        let list: Vec<String> = self
            .0
            .iter()
            .map(|(name, what)| {
                if what.trim().is_empty() {
                    format!("`{name}`")
                } else {
                    format!("`{name}` — {}", what.trim())
                }
            })
            .collect();
        format!(" Types: {}.", list.join("; "))
    }
}

pub struct AgentTool {
    pub types: Types,
    pub allow_model_arg: bool,
    pub max_spawn: usize,
    pub timeout_ms: u64,
}

impl Tool for AgentTool {
    fn spec(&self) -> ToolSpec {
        let mut task = json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string", "description": "Which type to start; omit for `default`"},
                "task": {"type": "string", "description": "The whole brief: what to look at, what to do, and what to report back"},
                "cwd": {"type": "string", "description": "Directory to work in, inside this one; omit for the same one"}
            },
            "required": ["task"]
        });
        if self.allow_model_arg {
            task["properties"]["model"] = json!({"type": "string", "description": "Model for this one, instead of its type's"});
        }
        ToolSpec::new(
            "agent",
            format!(
                "Hand work to agents that run on their own and report back. Each one starts \
                 fresh: it sees its brief and nothing of this conversation, so write the brief \
                 whole. Several tasks in one call run at the same time.{} Worth doing when what \
                 comes back is far smaller than what has to be read to find it, when the tasks \
                 do not depend on each other, or when a cheaper model will do; otherwise do the \
                 work here. An agent cannot ask you or the user anything, so leave it nothing to \
                 guess. Give agents that write to files separate areas, or they will overwrite \
                 each other. With `background` you get the ids at once and the news reaches you \
                 when they finish; a wait that runs out leaves them running in the background too.",
                self.types.sentence()
            ),
            json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": format!("Up to {} briefs, one per agent", self.max_spawn),
                        "items": task
                    },
                    "background": {"type": "boolean", "description": "Return the ids instead of waiting"},
                    "timeout_ms": {"type": "integer", "description": "How long to wait before leaving them in the background"}
                },
                "required": ["tasks"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let Some(spawner) = ctx.spawn else {
            return ToolResult::err("subagents are not available here");
        };
        let Some(tasks) = args.get("tasks").and_then(Value::as_array) else {
            return ToolResult::err("missing `tasks`");
        };
        if tasks.is_empty() {
            return ToolResult::err("`tasks` is empty; give each agent a brief");
        }
        if tasks.len() > self.max_spawn {
            return ToolResult::err(format!(
                "{} tasks is more than the {} allowed at once",
                tasks.len(),
                self.max_spawn
            ));
        }

        let mut started: Vec<Arc<Child>> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for t in tasks {
            let Some(task) = arg_str(t, "task").map(str::trim).filter(|s| !s.is_empty()) else {
                refused.push("a task with no `task` text was left out".into());
                continue;
            };
            let req = SpawnRequest {
                kind: arg_str(t, "agent").unwrap_or("").to_string(),
                task: task.to_string(),
                cwd: arg_str(t, "cwd").map(str::to_string),
                model: arg_str(t, "model").map(str::to_string),
            };
            match spawner.spawn(req) {
                Ok(c) => started.push(c),
                Err(e) => refused.push(e),
            }
        }
        if started.is_empty() {
            return ToolResult::err(if refused.is_empty() {
                "no agents were started".to_string()
            } else {
                refused.join("\n")
            });
        }

        let background = args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut out: Vec<String> = refused;
        if background {
            for c in &started {
                out.push(format!(
                    "started agent {} ({}): {}",
                    c.id,
                    c.kind,
                    first_line(&c.task)
                ));
            }
            out.push(format!(
                "Wait for them with {{\"action\": \"wait\", \"ids\": {:?}}}.",
                started.iter().map(|c| c.id).collect::<Vec<_>>()
            ));
            return ToolResult::ok(out.join("\n"));
        }

        let ms = arg_u64(args, "timeout_ms")
            .unwrap_or(self.timeout_ms)
            .max(1);
        let ids: Vec<u32> = started.iter().map(|c| c.id).collect();
        table().wait_any(&ids, true, Duration::from_millis(ms), ctx.cancel);

        if ctx.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            for c in &started {
                c.cancel();
            }
            return ToolResult::err("[cancelled by user]");
        }
        for c in &started {
            out.push(block(c));
            if c.running() {
                out.push(format!(
                    "agent {} is still running after {ms} ms and was left in the background; \
                     wait for it with {{\"action\": \"wait\", \"ids\": [{}]}}",
                    c.id, c.id
                ));
            }
        }
        ToolResult::ok(out.join("\n\n"))
    }
}

pub struct AgentsTool {
    pub kill_grace_ms: u64,
    pub timeout_ms: u64,
    pub max_concurrent: u32,
}

impl Tool for AgentsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "agents",
            "Look after the agents you started. `list` shows them, `status` reads how far the \
             given ones have got, `wait` blocks until they finish (or the first of them, without \
             `all`), `kill` stops one, and `say` gives a running one something more to go on. \
             Waiting costs nothing until something happens, so wait rather than ask again.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "status", "wait", "kill", "say"]},
                    "id": {"type": "integer", "description": "The agent, for `kill` and `say`"},
                    "ids": {"type": "array", "items": {"type": "integer"},
                            "description": "Agents for `status` and `wait`; omit to mean all of yours"},
                    "all": {"type": "boolean", "description": "wait: return only when every one has finished"},
                    "timeout_ms": {"type": "integer", "description": "How long `wait` may block"},
                    "text": {"type": "string", "description": "say: what to tell it"}
                },
                "required": ["action"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let t = table();
        let action = arg_str(args, "action").unwrap_or("list");
        let mine = t.owned_by(ctx.agent);
        match action {
            "list" => {
                if mine.is_empty() {
                    return ToolResult::ok("no agents; start one with the agent tool");
                }
                let lines: Vec<String> = mine
                    .iter()
                    .map(|c| format!("{} · {}", c.summary(), first_line(&c.task)))
                    .collect();
                ToolResult::ok(lines.join("\n"))
            }
            "status" => {
                let picked = pick(&mine, args, false);
                if picked.is_empty() {
                    return ToolResult::err("no agents of yours to report on");
                }
                let mut out = Vec::new();
                for c in picked {
                    let mut lines = vec![c.summary()];
                    lines.extend(c.log(10));
                    if c.state().over() {
                        lines.push(c.report());
                    }
                    out.push(lines.join("\n"));
                }
                ToolResult::ok(out.join("\n\n"))
            }
            "wait" => {
                let picked = pick(&mine, args, true);
                if picked.is_empty() {
                    return ToolResult::ok("nothing of yours is running");
                }
                let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
                let ms = arg_u64(args, "timeout_ms")
                    .unwrap_or(self.timeout_ms)
                    .max(1);
                let ids: Vec<u32> = picked.iter().map(|c| c.id).collect();
                t.wait_any(&ids, all, Duration::from_millis(ms), ctx.cancel);
                if ctx.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return ToolResult::err("[cancelled by user]");
                }
                let mut out = Vec::new();
                for c in picked {
                    out.push(if c.running() {
                        format!("{} · still going", c.summary())
                    } else {
                        block(&c)
                    });
                }
                ToolResult::ok(out.join("\n\n"))
            }
            "kill" => {
                let Some(c) = one(&mine, args) else {
                    return ToolResult::err("missing or unknown `id`; use action list");
                };
                c.cancel();
                c.wait(Duration::from_millis(self.kill_grace_ms));
                ToolResult::ok(block(&c))
            }
            "say" => {
                let Some(c) = one(&mine, args) else {
                    return ToolResult::err("missing or unknown `id`; use action list");
                };
                let Some(text) = arg_str(args, "text")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    return ToolResult::err("missing `text`");
                };
                if c.state().over() {
                    if let Err(e) = follow_up(&c, text, self.max_concurrent) {
                        return ToolResult::err(e);
                    }
                    return ToolResult::ok(format!(
                        "agent {} picked its work back up with that",
                        c.id
                    ));
                }
                c.say(text);
                ToolResult::ok(format!(
                    "agent {} will read that before its next step",
                    c.id
                ))
            }
            other => ToolResult::err(format!(
                "unknown action `{other}`; use list, status, wait, kill or say"
            )),
        }
    }

    fn parallel(&self, args: &Value, _settings: &ToolSettings) -> bool {
        matches!(arg_str(args, "action"), Some("list") | Some("status"))
    }
}

/// The agents an action is about: those named in `ids`, else all of the
/// caller's, or only the ones still going when `running_only`.
fn pick(mine: &[Arc<Child>], args: &Value, running_only: bool) -> Vec<Arc<Child>> {
    match args.get("ids").and_then(Value::as_array) {
        Some(ids) => {
            let want: Vec<u32> = ids
                .iter()
                .filter_map(Value::as_u64)
                .map(|n| n as u32)
                .collect();
            mine.iter()
                .filter(|c| want.contains(&c.id))
                .cloned()
                .collect()
        }
        None => mine
            .iter()
            .filter(|c| !running_only || c.running())
            .cloned()
            .collect(),
    }
}

fn one(mine: &[Arc<Child>], args: &Value) -> Option<Arc<Child>> {
    let id = arg_u64(args, "id")? as u32;
    mine.iter().find(|c| c.id == id).cloned()
}

/// What an agent has to say for itself, with the line that says how it went.
fn block(c: &Arc<Child>) -> String {
    let report = c.report();
    let head = c.summary();
    match (c.state(), report.is_empty()) {
        (State::Running | State::Queued, _) => format!("{head} · nothing back yet"),
        (_, true) => format!("{head}\n(said nothing)"),
        _ => format!("{head}\n{report}"),
    }
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(79).collect::<String>())
    } else {
        line.to_string()
    }
}
