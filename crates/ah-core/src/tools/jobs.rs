use std::time::Duration;

use ah_abi::{ToolResult, ToolSettings, ToolSpec};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, arg_str, arg_u64};
use crate::jobs::{self, Job, State};

pub struct JobsTool;

impl Tool for JobsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "jobs",
            "Look after background shell commands started with bash(background) or moved \
             to the background after a timeout. Actions: list, output (id, from_line or \
             tail), wait (id, timeout_ms; returns as soon as the job ends), kill (id).",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "output", "wait", "kill"]},
                    "id": {"type": "integer", "description": "Job id, for every action but list"},
                    "from_line": {"type": "integer", "description": "First line to return; use the next_line of the previous read to see only what is new"},
                    "tail": {"type": "integer", "description": "Return only the last N lines (default 200)"},
                    "timeout_ms": {"type": "integer", "description": "How long wait may block"}
                },
                "required": ["action"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let action = arg_str(args, "action").unwrap_or("list");
        let table = jobs::table();
        if action == "list" {
            let all = table.all();
            if all.is_empty() {
                return ToolResult::ok("no background jobs");
            }
            let lines: Vec<String> = all
                .iter()
                .map(|j| format!("{} · {}", j.summary(), j.command))
                .collect();
            return ToolResult::ok(lines.join("\n"));
        }
        let Some(id) = arg_u64(args, "id").map(|n| n as u32) else {
            return ToolResult::err("missing `id`");
        };
        let Some(job) = table.get(id) else {
            return ToolResult::err(format!("no job {id}; use action list"));
        };
        match action {
            "output" => ToolResult::ok(output(&job, args)),
            "wait" => {
                let ms = arg_u64(args, "timeout_ms").unwrap_or(ctx.settings.job_wait_ms);
                job.wait_while(Duration::from_millis(ms), ctx.cancel);
                if ctx.cancel.load(std::sync::atomic::Ordering::Relaxed) && job.running() {
                    return ToolResult::err("[cancelled by user]");
                }
                ToolResult::ok(output(&job, args))
            }
            "kill" => {
                if !job.running() {
                    return ToolResult::ok(job.summary());
                }
                job.kill(Duration::from_millis(ctx.settings.job_kill_grace_ms));
                job.wait(Duration::from_millis(ctx.settings.job_kill_grace_ms + 500));
                ToolResult::ok(format!("stopped {}", job.summary()))
            }
            other => ToolResult::err(format!(
                "unknown action `{other}`; use list, output, wait or kill"
            )),
        }
    }

    fn parallel(&self, args: &Value, _settings: &ToolSettings) -> bool {
        matches!(arg_str(args, "action"), Some("list") | Some("output"))
    }
}

/// Status line, then the requested slice of the job's output.
fn output(job: &Job, args: &Value) -> String {
    let (lines, next) = match arg_u64(args, "from_line") {
        Some(from) => job.view(from, 2000),
        None => job.tail(arg_u64(args, "tail").unwrap_or(200) as usize),
    };
    let (total, dropped) = job.counts();
    let mut head = job.summary();
    if dropped > 0 {
        head.push_str(&format!(" · {dropped} oldest lines dropped"));
    }
    if job.state() == State::Running {
        head.push_str(&format!(" · next_line {next} of {total}"));
    }
    if lines.is_empty() {
        head.push_str("\n(no new output)");
        return head;
    }
    format!("{head}\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: Value) -> ToolResult {
        let settings = ToolSettings::default();
        let cwd = std::env::current_dir().unwrap();
        let ctx = ToolCtx {
            cwd: &cwd,
            settings: &settings,
            cancel: crate::tools::never(),
            ask: crate::tools::no_user(),
        };
        JobsTool.run(&args, &ctx)
    }

    #[test]
    fn waits_reads_and_stops() {
        let job = jobs::table()
            .spawn(
                "sh",
                "for i in 1 2 3; do echo tick $i; sleep 0.1; done; sleep 30",
                &std::env::current_dir().unwrap(),
                65536,
            )
            .unwrap();
        let id = job.id;
        let listed = run(json!({"action": "list"}));
        assert!(listed.output.contains(&format!("job {id} running")));

        // wait returns when the timeout passes, with what has been printed
        let r = run(json!({"action": "wait", "id": id, "timeout_ms": 500}));
        assert!(r.output.contains("tick 3"), "{}", r.output);
        assert!(r.output.contains("next_line"), "{}", r.output);

        // from_line only returns what is new
        let r = run(json!({"action": "output", "id": id, "from_line": 3}));
        assert!(!r.output.contains("tick 1"), "{}", r.output);

        let r = run(json!({"action": "kill", "id": id}));
        assert!(r.output.contains("stopped"), "{}", r.output);
        assert!(!jobs::table().get(id).unwrap().running());
        jobs::table().remove(id);

        let r = run(json!({"action": "output", "id": id}));
        assert!(r.is_error);
    }

    #[test]
    fn unknown_action_is_reported() {
        let r = run(json!({"action": "explode", "id": 1}));
        assert!(r.is_error);
    }
}
