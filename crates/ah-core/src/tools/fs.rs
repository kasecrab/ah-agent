use std::fmt::Write as _;

use ah_abi::{ToolResult, ToolSettings, ToolSpec};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, arg_str, arg_u64, edit, resolve_path};

pub struct ReadFile;
pub struct WriteFile;
pub struct EditFile;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "read_file",
            "Read a text file. Returns numbered lines. Use offset/limit for large files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                    "offset": {"type": "integer", "description": "1-based first line to return (default 1)"},
                    "limit": {"type": "integer", "description": "Max lines to return"}
                },
                "required": ["path"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let Some(p) = arg_str(args, "path") else {
            return ToolResult::err("missing `path`");
        };
        let path = resolve_path(ctx.cwd, p);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return ToolResult::err(format!("{}: {e}", path.display())),
        };
        if bytes.iter().take(8192).any(|&b| b == 0) {
            return ToolResult::err(format!(
                "{}: binary file ({} bytes)",
                path.display(),
                bytes.len()
            ));
        }
        let text = String::from_utf8_lossy(&bytes);
        let offset = arg_u64(args, "offset").unwrap_or(1).max(1) as usize;
        let limit = arg_u64(args, "limit")
            .map(|l| l as usize)
            .unwrap_or(ctx.settings.read_default_limit);
        let total = text.lines().count();
        let mut out = String::with_capacity(text.len().min(64 * 1024));
        let mut shown = 0;
        for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            let _ = writeln!(out, "{:>6}\t{}", i + 1, line);
            shown += 1;
        }
        if shown == 0 {
            return ToolResult::ok(format!("(empty: file has {total} lines, offset {offset})"));
        }
        if offset - 1 + shown < total {
            let _ = write!(
                out,
                "… ({} more lines; total {total})",
                total - (offset - 1 + shown)
            );
        }
        ToolResult::ok(out)
    }

    fn parallel(&self, _args: &Value, _settings: &ToolSettings) -> bool {
        true
    }
}

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "write_file",
            "Create or overwrite a file with the given content. Parent directories are created.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let Some(p) = arg_str(args, "path") else {
            return ToolResult::err("missing `path`");
        };
        let Some(content) = arg_str(args, "content") else {
            return ToolResult::err("missing `content`");
        };
        let path = resolve_path(ctx.cwd, p);
        // Agents run side by side; one file is written by one of them at a time.
        let lock = super::path_lock(&path);
        let _held = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolResult::err(format!("mkdir {}: {e}", parent.display()));
        }
        let before = std::fs::read_to_string(&path).ok();
        match std::fs::write(&path, content) {
            Ok(()) => {
                let mut r = ToolResult::ok(format!(
                    "{} {} ({} bytes, {} lines)",
                    if before.is_some() {
                        "overwrote"
                    } else {
                        "created"
                    },
                    path.display(),
                    content.len(),
                    content.lines().count()
                ));
                r.diff = Some(super::diff::unified(
                    before.as_deref().unwrap_or(""),
                    content,
                    2,
                ));
                r
            }
            Err(e) => ToolResult::err(format!("{}: {e}", path.display())),
        }
    }
}

impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "edit_file",
            "Replace text in a file. Give one edit with old_string/new_string, or several in \
             `edits`, applied in order to the same file. Each old_string must match one place \
             unless replace_all is set; include enough surrounding lines to make it unique. \
             Trailing whitespace, carriage returns, how far a block is indented and the amount \
             of space between words may differ from the file. Nothing is written unless every \
             edit applies.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "default": false},
                    "edits": {
                        "type": "array",
                        "description": "Several replacements, applied in order",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean", "default": false}
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let Some(p) = arg_str(args, "path") else {
            return ToolResult::err("missing `path`");
        };
        let edits = match edits_from(args) {
            Ok(e) => e,
            Err(e) => return ToolResult::err(e),
        };
        let path = resolve_path(ctx.cwd, p);
        // Read, apply and write are one step as far as other agents are concerned.
        let lock = super::path_lock(&path);
        let _held = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ToolResult::err(format!(
                    "{}: no such file; use write_file to create it",
                    path.display()
                ));
            }
            Err(e) => return ToolResult::err(format!("{}: {e}", path.display())),
        };
        let borrowed: Vec<edit::Edit<'_>> = edits
            .iter()
            .map(|(o, n, all)| edit::Edit {
                old: o,
                new: n,
                replace_all: *all,
            })
            .collect();
        let applied = match edit::apply(&text, &borrowed) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(e),
        };
        if let Err(e) = std::fs::write(&path, &applied.text) {
            return ToolResult::err(format!("{}: {e}", path.display()));
        }
        let mut out = format!(
            "edited {} ({} edit{}, {} replacement{})",
            path.display(),
            borrowed.len(),
            if borrowed.len() == 1 { "" } else { "s" },
            applied.replacements,
            if applied.replacements == 1 { "" } else { "s" }
        );
        for n in &applied.notes {
            out.push_str(&format!("\n{n}"));
        }
        let mut r = ToolResult::ok(out);
        r.diff = Some(super::diff::unified(&text, &applied.text, 2));
        r
    }
}

/// `(old, new, replace_all)` from either the single-edit arguments or `edits`.
fn edits_from(args: &Value) -> Result<Vec<(String, String, bool)>, String> {
    let all = |v: &Value| {
        v.get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    if let Some(list) = args.get("edits").and_then(Value::as_array) {
        if list.is_empty() {
            return Err("`edits` is empty".into());
        }
        return list
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let old = arg_str(e, "old_string")
                    .ok_or_else(|| format!("edit {}: missing `old_string`", i + 1))?;
                let new = arg_str(e, "new_string")
                    .ok_or_else(|| format!("edit {}: missing `new_string`", i + 1))?;
                Ok((old.to_string(), new.to_string(), all(e)))
            })
            .collect();
    }
    let old = arg_str(args, "old_string").ok_or("missing `old_string`")?;
    let new = arg_str(args, "new_string").ok_or("missing `new_string`")?;
    Ok(vec![(old.to_string(), new.to_string(), all(args))])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_agents_editing_one_file_do_not_lose_each_others_work() {
        let dir = std::env::temp_dir().join(format!("ah-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "one\ntwo\n").unwrap();
        let settings = ah_abi::ToolSettings::default();
        let edit = |old: &str, new: &str| {
            let ctx = ToolCtx {
                cwd: &dir,
                settings: &settings,
                agent: 0,
                cancel: crate::tools::never(),
                ask: crate::tools::no_user(),
                spawn: None,
            };
            EditFile.run(
                &json!({"path": "f.txt", "old_string": old, "new_string": new}),
                &ctx,
            )
        };
        std::thread::scope(|s| {
            s.spawn(|| edit("one", "ONE"));
            s.spawn(|| edit("two", "TWO"));
        });
        let text = std::fs::read_to_string(dir.join("f.txt")).unwrap();
        assert_eq!(text, "ONE\nTWO\n", "an edit was lost: {text:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn several_edits_in_one_call() {
        let dir = std::env::temp_dir().join(format!("ah-edits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let settings = ah_abi::ToolSettings::default();
        let ctx = ToolCtx {
            cwd: &dir,
            settings: &settings,
            agent: 0,
            cancel: crate::tools::never(),
            ask: crate::tools::no_user(),
            spawn: None,
        };
        WriteFile.run(
            &json!({"path": "f.rs", "content": "fn a() {\n    one();\n    two();\n}\n"}),
            &ctx,
        );
        let r = EditFile.run(
            &json!({"path": "f.rs", "edits": [
                {"old_string": "one();", "new_string": "ONE();"},
                {"old_string": "two();", "new_string": "TWO();"}
            ]}),
            &ctx,
        );
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("2 edits, 2 replacements"), "{}", r.output);
        assert_eq!(
            std::fs::read_to_string(dir.join("f.rs")).unwrap(),
            "fn a() {\n    ONE();\n    TWO();\n}\n"
        );

        // A failing edit leaves the file exactly as it was.
        let r = EditFile.run(
            &json!({"path": "f.rs", "edits": [
                {"old_string": "ONE();", "new_string": "1();"},
                {"old_string": "nope", "new_string": "x"}
            ]}),
            &ctx,
        );
        assert!(r.is_error, "{}", r.output);
        assert!(r.output.starts_with("edit 2: "), "{}", r.output);
        assert_eq!(
            std::fs::read_to_string(dir.join("f.rs")).unwrap(),
            "fn a() {\n    ONE();\n    TWO();\n}\n"
        );

        // Indentation the model got wrong is fixed up, and reported.
        let r = EditFile.run(
            &json!({"path": "f.rs", "old_string": "ONE();\nTWO();\n", "new_string": "done();\n"}),
            &ctx,
        );
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("indentation"), "{}", r.output);
        assert_eq!(
            std::fs::read_to_string(dir.join("f.rs")).unwrap(),
            "fn a() {\n    done();\n}\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editing_a_missing_file_points_at_write_file() {
        let dir = std::env::temp_dir().join(format!("ah-missing-{}", std::process::id()));
        let settings = ah_abi::ToolSettings::default();
        let ctx = ToolCtx {
            cwd: &dir,
            settings: &settings,
            agent: 0,
            cancel: crate::tools::never(),
            ask: crate::tools::no_user(),
            spawn: None,
        };
        let r = EditFile.run(
            &json!({"path": "nope.txt", "old_string": "a", "new_string": "b"}),
            &ctx,
        );
        assert!(r.is_error);
        assert!(r.output.contains("write_file"), "{}", r.output);
    }

    #[test]
    fn read_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ah-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let settings = ah_abi::ToolSettings::default();
        let ctx = ToolCtx {
            cwd: &dir,
            settings: &settings,
            agent: 0,
            cancel: crate::tools::never(),
            ask: crate::tools::no_user(),
            spawn: None,
        };
        let w = WriteFile.run(
            &json!({"path": "sub/f.txt", "content": "one\ntwo\nthree\n"}),
            &ctx,
        );
        assert!(!w.is_error, "{}", w.output);
        let r = ReadFile.run(&json!({"path": "sub/f.txt", "offset": 2, "limit": 1}), &ctx);
        assert!(r.output.contains("     2\ttwo"), "{}", r.output);
        assert!(r.output.contains("1 more lines"));
        let e = EditFile.run(
            &json!({"path": "sub/f.txt", "old_string": "two", "new_string": "2"}),
            &ctx,
        );
        assert!(!e.is_error, "{}", e.output);
        assert_eq!(
            e.diff.as_deref(),
            Some("@@ -1,3 +1,3 @@\n one\n-two\n+2\n three\n")
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/f.txt")).unwrap(),
            "one\n2\nthree\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
