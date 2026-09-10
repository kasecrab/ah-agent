use ah_abi::{ToolResult, ToolSettings, ToolSpec};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, arg_str, arg_u64};
use crate::plan::{self, NewTask, Status};

pub struct PlanTool;

/// What the reply says, and which tasks it shows beneath: `None` for the
/// whole plan, `Some(ids)` for the summary and just those lines.
type Reply = (String, Option<Vec<u16>>);

impl Tool for PlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "plan",
            "Keep the task list for work that takes several steps. `set` writes the whole \
             plan (ids are handed out in order, so `parent` and `needs` of 2 mean the second \
             task you list); `add` appends; `start`, `done` and `drop` move tasks; `update` \
             changes one; `list` shows it. Give a task a `parent` to make it a subtask and \
             `needs` for the tasks that must finish first. Start a task before working on it \
             and finish it as soon as it is done, one at a time. `set` and `list` answer with \
             the whole plan and the others with the summary and the tasks they touched, so \
             read the reply instead of keeping a copy.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["set", "add", "start", "done", "drop", "update", "list"]},
                    "tasks": {
                        "type": "array",
                        "description": "For set and add",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "parent": {"type": "integer", "description": "Id of the task this one belongs to"},
                                "needs": {"type": "array", "items": {"type": "integer"}, "description": "Ids that must finish first"},
                                "status": {"type": "string", "enum": ["todo", "doing", "done", "dropped"]},
                                "note": {"type": "string"}
                            },
                            "required": ["title"]
                        }
                    },
                    "ids": {"type": "array", "items": {"type": "integer"}, "description": "Tasks to start, finish or drop"},
                    "id": {"type": "integer", "description": "One task, for update or in place of ids"},
                    "title": {"type": "string", "description": "New title, for update"},
                    "parent": {"type": "integer", "description": "New parent, for update; 0 detaches"},
                    "needs": {"type": "array", "items": {"type": "integer"}, "description": "New dependencies, for update"},
                    "note": {"type": "string", "description": "Short line about the outcome or the blocker"}
                },
                "required": ["action"]
            }),
        )
    }

    fn run(&self, args: &Value, _ctx: &ToolCtx<'_>) -> ToolResult {
        let action = arg_str(args, "action").unwrap_or("list");
        let note = arg_str(args, "note").unwrap_or("").trim();
        let store = plan::store();
        let outcome: Result<Reply, String> = match action {
            "list" => Ok((String::new(), None)),
            "set" | "add" => match tasks(args) {
                Err(e) => Err(e),
                Ok(items) => store.edit(|p| {
                    if action == "set" {
                        p.set(&items)?;
                        Ok(((format!("plan set: {} task(s)", items.len()), None), true))
                    } else {
                        let ids = p.add(&items)?;
                        Ok(((format!("added {}", join(&ids)), Some(ids)), true))
                    }
                }),
            },
            "start" | "done" | "drop" => {
                let ids = ids(args);
                if ids.is_empty() {
                    Err("missing `ids`".into())
                } else {
                    let status = match action {
                        "start" => Status::Doing,
                        "done" => Status::Done,
                        _ => Status::Dropped,
                    };
                    store.edit(|p| {
                        let moved = p.set_status(&ids, status, note)?;
                        // Saying so beats a reply that looks like work: a
                        // model told nothing changed stops asking again.
                        if moved.is_empty() {
                            let head = format!(
                                "no change: {} {} already {}",
                                join(&ids),
                                if ids.len() == 1 { "is" } else { "are" },
                                status.name()
                            );
                            return Ok(((head, Some(ids.clone())), false));
                        }
                        Ok((
                            (format!("{} {}", status.name(), join(&moved)), Some(moved)),
                            true,
                        ))
                    })
                }
            }
            "update" => match arg_u64(args, "id").map(|n| n as u16) {
                None => Err("missing `id`".into()),
                Some(id) => store.edit(|p| {
                    let changed = p.update(
                        id,
                        arg_str(args, "title"),
                        args.get("parent")
                            .and_then(Value::as_u64)
                            .map(|n| (n as u16 != 0).then_some(n as u16)),
                        args.get("needs").and_then(Value::as_array).map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_u64())
                                .map(|n| n as u16)
                                .collect()
                        }),
                        (!note.is_empty()).then_some(note),
                    )?;
                    let head = if changed {
                        format!("updated {id}")
                    } else {
                        format!("no change: {id} already reads that way")
                    };
                    Ok(((head, Some(vec![id])), changed))
                }),
            },
            other => Err(format!(
                "unknown action `{other}`; use set, add, start, done, drop, update or list"
            )),
        };
        // A move shows what it moved; anything else, and every error, shows
        // the whole plan, which is exactly what a model needs to correct itself.
        let view = match &outcome {
            Ok((_, Some(ids))) => store.with(|p| p.render_some(ids)),
            _ => store.with(|p| p.render()),
        };
        match outcome {
            Ok((head, _)) if head.is_empty() => ToolResult::ok(view),
            Ok((head, _)) => ToolResult::ok(format!("{head}\n{view}")),
            Err(e) => ToolResult::err(format!("{e}\n{view}")),
        }
    }

    fn parallel(&self, args: &Value, _settings: &ToolSettings) -> bool {
        arg_str(args, "action") == Some("list")
    }
}

fn tasks(args: &Value) -> Result<Vec<NewTask>, String> {
    let Some(list) = args.get("tasks") else {
        return Err("missing `tasks`".into());
    };
    let items: Vec<NewTask> =
        serde_json::from_value(list.clone()).map_err(|e| format!("bad `tasks`: {e}"))?;
    if items.is_empty() {
        return Err("`tasks` is empty".into());
    }
    Ok(items)
}

/// `ids`, or a single `id`.
fn ids(args: &Value) -> Vec<u16> {
    if let Some(a) = args.get("ids").and_then(Value::as_array) {
        return a
            .iter()
            .filter_map(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .map(|n| n as u16)
            .collect();
    }
    arg_u64(args, "id")
        .map(|n| vec![n as u16])
        .unwrap_or_default()
}

fn join(ids: &[u16]) -> String {
    ids.iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: Value) -> ToolResult {
        let cwd = std::env::current_dir().unwrap();
        let settings = ToolSettings::default();
        let ctx = ToolCtx {
            cwd: &cwd,
            settings: &settings,
            agent: 0,
            cancel: crate::tools::never(),
            ask: crate::tools::no_user(),
            spawn: None,
        };
        PlanTool.run(&args, &ctx)
    }

    #[test]
    fn a_plan_is_written_worked_through_and_reported() {
        let _guard = plan::test_lock();
        let r = run(json!({"action": "set", "tasks": [
            {"title": "parser"},
            {"title": "lexer", "parent": 1},
            {"title": "tests", "needs": [1]}
        ]}));
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("plan set: 3 task(s)"), "{}", r.output);
        assert!(r.output.contains("waits for 1"), "{}", r.output);

        let r = run(json!({"action": "start", "ids": [3]}));
        assert!(r.is_error, "{}", r.output);
        assert!(r.output.contains("waits for 1"), "{}", r.output);

        assert!(!run(json!({"action": "start", "id": 2})).is_error);
        assert!(!run(json!({"action": "done", "id": 2, "note": "hand written"})).is_error);
        let r = run(json!({"action": "done", "id": 1}));
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("next: 3 tests"), "{}", r.output);

        let r = run(json!({"action": "add", "tasks": [{"title": "docs", "needs": [3]}]}));
        assert!(r.output.contains("added 4"), "{}", r.output);
        assert_eq!(plan::store().snapshot().counts(), (2, 4));
    }

    #[test]
    fn bad_calls_explain_themselves_and_show_the_plan() {
        let _guard = plan::test_lock();
        let r = run(json!({"action": "wat"}));
        assert!(r.is_error);
        assert!(r.output.contains("unknown action"), "{}", r.output);
        let r = run(json!({"action": "set", "tasks": []}));
        assert!(r.output.contains("`tasks` is empty"), "{}", r.output);
        let r = run(json!({"action": "done", "ids": [7]}));
        assert!(r.output.contains("no task 7"), "{}", r.output);
        assert!(!run(json!({"action": "list"})).is_error);
    }

    #[test]
    fn a_move_answers_with_what_it_moved_and_not_the_rest() {
        let _guard = plan::test_lock();
        run(json!({"action": "set", "tasks": [
            {"title": "parser"},
            {"title": "lexer"},
            {"title": "tests"}
        ]}));
        run(json!({"action": "done", "id": 1}));

        let r = run(json!({"action": "done", "id": 2, "note": "hand written"}));
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("done 2"), "{}", r.output);
        assert!(r.output.contains("lexer"), "{}", r.output);
        assert!(!r.output.contains("parser"), "{}", r.output);

        // Asking again says so, rather than reading like work that happened.
        let r = run(json!({"action": "done", "id": 2}));
        assert!(!r.is_error, "{}", r.output);
        assert!(
            r.output.contains("no change: 2 is already done"),
            "{}",
            r.output
        );

        // The whole plan is still there for the asking, and after a bad call.
        let r = run(json!({"action": "list"}));
        assert!(r.output.contains("parser"), "{}", r.output);
        let r = run(json!({"action": "done", "ids": [9]}));
        assert!(r.is_error);
        assert!(r.output.contains("parser"), "{}", r.output);
    }

    #[test]
    fn only_reading_the_plan_joins_a_batch() {
        let s = ToolSettings::default();
        assert!(PlanTool.parallel(&json!({"action": "list"}), &s));
        assert!(!PlanTool.parallel(&json!({"action": "done", "id": 1}), &s));
    }
}
