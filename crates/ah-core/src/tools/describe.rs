//! Short `Verb(what)` headers for tool calls, for the transcript and the CLI.
//! The model's own JSON stays out of the user's way; `None` means there is
//! nothing better to show than the arguments themselves.

use serde_json::Value;

use super::{arg_str, arg_u64};

pub fn describe(name: &str, args: &Value) -> Option<String> {
    match name {
        "bash" => {
            let cmd = one_line(unwrap_shell(arg_str(args, "command")?));
            Some(
                if args.get("background").and_then(Value::as_bool) == Some(true) {
                    format!("Bash({cmd}, background)")
                } else {
                    format!("Bash({cmd})")
                },
            )
        }
        "read_file" => {
            let path = arg_str(args, "path")?;
            Some(match (arg_u64(args, "offset"), arg_u64(args, "limit")) {
                (Some(o), Some(l)) => format!("Read({path}:{o}-{})", o + l - 1),
                (Some(o), None) => format!("Read({path}:{o}-)"),
                _ => format!("Read({path})"),
            })
        }
        "write_file" => Some(format!("Write({})", arg_str(args, "path")?)),
        "edit_file" => {
            let path = arg_str(args, "path")?;
            let n = args
                .get("edits")
                .and_then(Value::as_array)
                .map_or(1, Vec::len);
            Some(if n > 1 {
                format!("Edit({path}, {n} changes)")
            } else {
                format!("Edit({path})")
            })
        }
        "jobs" => {
            let id = arg_u64(args, "id").unwrap_or(0);
            Some(match arg_str(args, "action").unwrap_or("list") {
                "output" => format!("Jobs(read {id})"),
                "wait" => format!("Jobs(wait {id})"),
                "kill" => format!("Jobs(stop {id})"),
                _ => "Jobs(list)".into(),
            })
        }
        "plan" => Some(format!("Plan({})", plan(args))),
        "ask_user" => Some(format!("Ask({})", asked(args)?)),
        "agent" => {
            let tasks = args.get("tasks").and_then(Value::as_array)?;
            let first = tasks.first().and_then(|t| arg_str(t, "task"))?;
            Some(match tasks.len() {
                1 => format!("Agent({})", one_line(first)),
                n => format!("Agent({n} tasks: {})", one_line(first)),
            })
        }
        "agents" => {
            let id = arg_u64(args, "id").unwrap_or(0);
            Some(match arg_str(args, "action").unwrap_or("list") {
                "status" => "Agents(status)".into(),
                "wait" => "Agents(wait)".into(),
                "kill" => format!("Agents(stop {id})"),
                "say" => format!("Agents(say to {id})"),
                _ => "Agents(list)".into(),
            })
        }
        _ => None,
    }
}

fn plan(args: &Value) -> String {
    let ids: Vec<u16> = match args.get("ids").and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .filter_map(Value::as_u64)
            .map(|n| n as u16)
            .collect(),
        None => arg_u64(args, "id")
            .map(|n| vec![n as u16])
            .unwrap_or_default(),
    };
    // A task reads better by name than by number, when the plan has one.
    let tasks = |verb: &str| {
        let plan = crate::plan::store().snapshot();
        let named: Vec<String> = ids
            .iter()
            .map(|id| match plan.get(*id) {
                Some(t) => format!("{id} {}", one_line(&t.title)),
                None => id.to_string(),
            })
            .collect();
        if named.is_empty() {
            verb.to_string()
        } else {
            format!("{verb} {}", named.join(", "))
        }
    };
    let count = args
        .get("tasks")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    match args.get("action").and_then(Value::as_str).unwrap_or("list") {
        "set" => format!("{count} task{}", plural(count)),
        "add" => match args
            .get("tasks")
            .and_then(Value::as_array)
            .filter(|a| a.len() == 1)
            .and_then(|a| a[0].get("title"))
            .and_then(Value::as_str)
        {
            Some(title) => format!("add {}", one_line(title)),
            None => format!("add {count} task{}", plural(count)),
        },
        "start" => tasks("start"),
        "done" => tasks("done"),
        "drop" => tasks("drop"),
        "update" => tasks("update"),
        _ => "list".into(),
    }
}

/// `bash -c 'cd x && make'` is the shell repeating itself; show the command.
fn unwrap_shell(cmd: &str) -> &str {
    let cmd = cmd.trim();
    for prefix in ["bash -c ", "sh -c ", "zsh -c "] {
        let Some(rest) = cmd.strip_prefix(prefix) else {
            continue;
        };
        let rest = rest.trim();
        for quote in ['\'', '"'] {
            if let Some(inner) = rest.strip_prefix(quote).and_then(|r| r.strip_suffix(quote))
                && !inner.contains(quote)
            {
                return inner;
            }
        }
        return rest;
    }
    cmd
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Newlines out, long text cut; the header is one line.
/// The first question, by its header where the model wrote one: the whole
/// question is on screen in the box anyway.
fn asked(args: &Value) -> Option<String> {
    let first = match args.get("questions") {
        Some(Value::Array(a)) => a.first()?,
        _ => args,
    };
    if let Some(s) = first.as_str() {
        return Some(one_line(s));
    }
    let text = ["header", "question", "prompt", "text"]
        .iter()
        .find_map(|k| first.get(*k).and_then(Value::as_str))?;
    Some(one_line(text))
}

fn one_line(s: &str) -> String {
    let flat: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect();
    if flat.chars().count() == 120 {
        format!("{flat}…")
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(name: &str, args: Value) -> String {
        describe(name, &args).unwrap_or_default()
    }

    #[test]
    fn every_built_in_call_is_a_short_header() {
        assert_eq!(d("bash", json!({"command": "ls -la"})), "Bash(ls -la)");
        assert_eq!(
            d("bash", json!({"command": "bash -c 'cd ~/x && make'"})),
            "Bash(cd ~/x && make)"
        );
        assert_eq!(
            d(
                "bash",
                json!({"command": "npm run dev", "background": true})
            ),
            "Bash(npm run dev, background)"
        );
        assert_eq!(
            d("read_file", json!({"path": "hello.md"})),
            "Read(hello.md)"
        );
        assert_eq!(
            d(
                "ask_user",
                json!({"questions": [{"header": "Auth method", "question": "Which one?"}]})
            ),
            "Ask(Auth method)"
        );
        assert_eq!(
            d("ask_user", json!({"question": "Which one?"})),
            "Ask(Which one?)"
        );
        assert_eq!(
            d(
                "read_file",
                json!({"path": "src/main.rs", "offset": 10, "limit": 20})
            ),
            "Read(src/main.rs:10-29)"
        );
        assert_eq!(d("write_file", json!({"path": "a.rs"})), "Write(a.rs)");
        assert_eq!(
            d("edit_file", json!({"path": "a.rs", "edits": [1, 2, 3]})),
            "Edit(a.rs, 3 changes)"
        );
        assert_eq!(
            d("jobs", json!({"action": "kill", "id": 2})),
            "Jobs(stop 2)"
        );
        assert!(describe("some_plugin_tool", &json!({"x": 1})).is_none());
    }

    #[test]
    fn plan_calls_name_the_task_they_touch() {
        let _guard = crate::plan::test_lock();
        crate::plan::store()
            .edit(|p| {
                p.set(&[crate::plan::NewTask {
                    title: "write the parser".into(),
                    ..Default::default()
                }])
            })
            .unwrap();
        assert_eq!(
            d("plan", json!({"action": "set", "tasks": [1, 2]})),
            "Plan(2 tasks)"
        );
        assert_eq!(
            d(
                "plan",
                json!({"action": "add", "tasks": [{"title": "tests"}]})
            ),
            "Plan(add tests)"
        );
        assert_eq!(
            d("plan", json!({"action": "done", "id": 1})),
            "Plan(done 1 write the parser)"
        );
        assert_eq!(d("plan", json!({"action": "list"})), "Plan(list)");
    }
}
