use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ah_abi::{ToolResult, ToolSettings, ToolSpec};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, arg_str, arg_u64};

pub struct Bash;

impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "bash",
            "Run a shell command in the working directory and return combined stdout/stderr plus the exit code. \
             Non-interactive; commands that wait for input will time out.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to run"},
                    "timeout_ms": {"type": "integer", "description": "Optional timeout in milliseconds"}
                },
                "required": ["command"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let Some(cmd) = arg_str(args, "command") else {
            return ToolResult::err("missing `command`");
        };
        let timeout = Duration::from_millis(
            arg_u64(args, "timeout_ms")
                .unwrap_or(ctx.settings.bash_timeout_ms)
                .max(1),
        );
        let shell = if ctx.settings.shell.is_empty() {
            std::env::var("SHELL")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "sh".into())
        } else {
            ctx.settings.shell.clone()
        };
        run_shell(&shell, cmd, ctx.cwd, timeout)
    }

    fn parallel(&self, args: &Value, settings: &ToolSettings) -> bool {
        arg_str(args, "command")
            .is_some_and(|cmd| crate::policy::read_only(cmd, &settings.parallel_bash))
    }
}

pub fn run_shell(shell: &str, cmd: &str, cwd: &std::path::Path, timeout: Duration) -> ToolResult {
    let mut command = Command::new(shell);
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // own process group so timeouts kill the whole tree
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return ToolResult::err(format!("failed to spawn {shell}: {e}")),
    };

    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_tree(&child);
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(
                    if start.elapsed() < Duration::from_millis(200) {
                        5
                    } else {
                        25
                    },
                ));
            }
            Err(e) => return ToolResult::err(format!("wait failed: {e}")),
        }
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();

    let mut text = String::from_utf8_lossy(&stdout).into_owned();
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&stderr));
    }
    if timed_out {
        text.push_str(&format!("\n[timed out after {} ms]", timeout.as_millis()));
        return ToolResult::err(text);
    }
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    if code != 0 {
        text.push_str(&format!("\n[exit code {code}]"));
        return ToolResult::err(text);
    }
    if text.is_empty() {
        text.push_str("(no output)");
    }
    ToolResult::ok(text)
}

#[cfg(unix)]
fn kill_tree(child: &std::process::Child) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(-(child.id() as i32), 9);
    }
}

#[cfg(not(unix))]
fn kill_tree(_child: &std::process::Child) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_and_captures() {
        let r = run_shell(
            "sh",
            "echo hi; echo err 1>&2",
            std::path::Path::new("."),
            Duration::from_secs(5),
        );
        assert!(!r.is_error);
        assert!(r.output.contains("hi") && r.output.contains("err"));
    }

    #[test]
    fn nonzero_exit_is_error() {
        let r = run_shell(
            "sh",
            "exit 3",
            std::path::Path::new("."),
            Duration::from_secs(5),
        );
        assert!(r.is_error);
        assert!(r.output.contains("exit code 3"));
    }

    #[test]
    fn timeout_kills() {
        let r = run_shell(
            "sh",
            "sleep 5",
            std::path::Path::new("."),
            Duration::from_millis(100),
        );
        assert!(r.is_error);
        assert!(r.output.contains("timed out"));
    }
}
