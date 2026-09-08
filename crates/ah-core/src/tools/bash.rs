use std::time::Duration;

use ah_abi::{ToolResult, ToolSettings, ToolSpec};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, arg_str, arg_u64};
use crate::jobs::{self, Job, State};

pub struct Bash;

impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "bash",
            "Run a shell command in the working directory. Returns stdout and stderr. \
             Set background for a command that should keep running (a server, a long \
             build or download); it returns a job id at once and the `jobs` tool reads, \
             waits for or stops it.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command"},
                    "timeout_ms": {"type": "integer", "description": "Optional timeout in milliseconds"},
                    "background": {"type": "boolean", "description": "Start as a background job and return immediately"}
                },
                "required": ["command"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let Some(cmd) = arg_str(args, "command") else {
            return ToolResult::err("missing `command`");
        };
        let shell = if ctx.settings.shell.is_empty() {
            std::env::var("SHELL").unwrap_or_else(|_| "sh".into())
        } else {
            ctx.settings.shell.clone()
        };
        let timeout = Duration::from_millis(
            arg_u64(args, "timeout_ms")
                .unwrap_or(ctx.settings.bash_timeout_ms)
                .max(1),
        );
        let background = args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let table = jobs::table();
        let job = match table.spawn(&shell, cmd, ctx.cwd, ctx.settings.job_buffer_bytes) {
            Ok(j) => j,
            Err(e) => return ToolResult::err(format!("failed to spawn {shell}: {e}")),
        };
        if background {
            return ToolResult::ok(format!(
                "started job {} in the background: {cmd}\n\
                 Read it with the jobs tool: {{\"action\": \"output\", \"id\": {}}}. \
                 It keeps running until it exits or you stop it.",
                job.id, job.id
            ));
        }
        if job.wait_while(timeout, ctx.cancel) {
            let text = finished_text(&job);
            table.remove(job.id);
            return text;
        }
        // Esc during a command stops the command, not just the turn.
        if ctx.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            job.kill(Duration::from_millis(ctx.settings.job_kill_grace_ms));
            job.wait(Duration::from_millis(ctx.settings.job_kill_grace_ms + 500));
            table.remove(job.id);
            return ToolResult::err("[cancelled by user]");
        }
        if !ctx.settings.background_on_timeout {
            job.kill(Duration::from_millis(ctx.settings.job_kill_grace_ms));
            job.wait(Duration::from_millis(ctx.settings.job_kill_grace_ms + 500));
            let mut text = job.text();
            table.remove(job.id);
            text.push_str(&format!("\n[timed out after {} ms]", timeout.as_millis()));
            return ToolResult::err(text);
        }
        let (tail, _) = job.tail(20);
        let mut text = format!(
            "still running after {} ms, moved to the background as job {}.\n\
             Read it with the jobs tool: {{\"action\": \"output\", \"id\": {}}}, \
             wait for it with {{\"action\": \"wait\", \"id\": {}}}, \
             stop it with {{\"action\": \"kill\", \"id\": {}}}.",
            timeout.as_millis(),
            job.id,
            job.id,
            job.id,
            job.id
        );
        if !tail.is_empty() {
            text.push_str("\nOutput so far:\n");
            text.push_str(&tail.join("\n"));
        }
        ToolResult::ok(text)
    }

    fn parallel(&self, args: &Value, settings: &ToolSettings) -> bool {
        args.get("background").and_then(Value::as_bool) != Some(true)
            && arg_str(args, "command")
                .is_some_and(|cmd| crate::policy::read_only(cmd, &settings.parallel_bash))
    }
}

/// What a finished job reports back: its output, then its exit code if nonzero.
fn finished_text(job: &Job) -> ToolResult {
    let mut text = job.text();
    let code = match job.state() {
        State::Done(c) => c,
        State::Running => 0,
    };
    if code != 0 {
        text.push_str(&format!("\n[exit code {code}]"));
        return ToolResult::err(text);
    }
    if text.is_empty() {
        text.push_str("(no output)");
    }
    ToolResult::ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> ToolSettings {
        ToolSettings::default()
    }

    fn run(args: Value, settings: &ToolSettings) -> ToolResult {
        let cwd = std::env::current_dir().unwrap();
        let ctx = ToolCtx {
            cwd: &cwd,
            settings,
            cancel: crate::tools::never(),
            ask: crate::tools::no_user(),
        };
        Bash.run(&args, &ctx)
    }

    fn job_id(output: &str, after: &str) -> u32 {
        output
            .split(after)
            .nth(1)
            .and_then(|s| {
                s.split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|d| d.parse().ok())
            })
            .unwrap_or_else(|| panic!("no job id in {output:?}"))
    }

    fn stop(id: u32) {
        let job = jobs::table().get(id).expect("job in the table");
        job.kill(Duration::from_millis(10));
        assert!(job.wait(Duration::from_secs(5)), "job would not stop");
        jobs::table().remove(id);
    }

    #[test]
    fn runs_and_captures() {
        let r = run(json!({"command": "echo hello"}), &settings());
        assert!(!r.is_error);
        assert!(r.output.contains("hello"));
    }

    #[test]
    fn nonzero_exit_is_error() {
        let r = run(json!({"command": "echo oops 1>&2; exit 2"}), &settings());
        assert!(r.is_error);
        assert!(r.output.contains("oops"));
        assert!(r.output.contains("exit code 2"));
    }

    #[test]
    fn a_slow_command_moves_to_the_background() {
        let r = run(
            json!({"command": "sleep 20", "timeout_ms": 100}),
            &settings(),
        );
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("moved to the background"), "{}", r.output);
        stop(job_id(&r.output, "as job "));
    }

    #[test]
    fn timeout_kills_when_promotion_is_off() {
        let mut s = settings();
        s.background_on_timeout = false;
        s.job_kill_grace_ms = 50;
        let r = run(json!({"command": "sleep 20", "timeout_ms": 100}), &s);
        assert!(r.is_error);
        assert!(r.output.contains("timed out"), "{}", r.output);
    }

    #[test]
    fn background_returns_a_job_id() {
        let r = run(
            json!({"command": "sleep 20", "background": true}),
            &settings(),
        );
        assert!(!r.is_error);
        let id = job_id(&r.output, "started job ");
        assert!(jobs::table().get(id).unwrap().running());
        stop(id);
    }

    #[test]
    fn cancelling_the_turn_stops_the_command() {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let settings = settings();
        let cwd = std::env::current_dir().unwrap();
        let ctx = ToolCtx {
            cwd: &cwd,
            settings: &settings,
            cancel: &cancel,
            ask: crate::tools::no_user(),
        };
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let start = std::time::Instant::now();
            let r = Bash.run(&json!({"command": "sleep 30"}), &ctx);
            assert!(r.is_error);
            assert!(r.output.contains("cancelled"), "{}", r.output);
            assert!(start.elapsed() < Duration::from_secs(5), "cancel was slow");
        });
    }

    #[test]
    fn a_background_call_never_joins_a_batch() {
        let s = settings();
        assert!(Bash.parallel(&json!({"command": "rg todo"}), &s));
        assert!(!Bash.parallel(&json!({"command": "rg todo", "background": true}), &s));
    }
}
