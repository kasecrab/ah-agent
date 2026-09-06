use std::fmt::Write as _;

use ah_abi::{ToolResult, ToolSpec};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, arg_str, arg_u64, resolve_path};

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
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolResult::err(format!("mkdir {}: {e}", parent.display()));
        }
        let existed = path.exists();
        match std::fs::write(&path, content) {
            Ok(()) => ToolResult::ok(format!(
                "{} {} ({} bytes, {} lines)",
                if existed { "overwrote" } else { "created" },
                path.display(),
                content.len(),
                content.lines().count()
            )),
            Err(e) => ToolResult::err(format!("{}: {e}", path.display())),
        }
    }
}

impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "edit_file",
            "Replace an exact string in a file. `old_string` must match exactly once unless `replace_all` is true. \
             Include enough surrounding lines to make it unique.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        )
    }

    fn run(&self, args: &Value, ctx: &ToolCtx<'_>) -> ToolResult {
        let Some(p) = arg_str(args, "path") else {
            return ToolResult::err("missing `path`");
        };
        let Some(old) = arg_str(args, "old_string") else {
            return ToolResult::err("missing `old_string`");
        };
        let Some(new) = arg_str(args, "new_string") else {
            return ToolResult::err("missing `new_string`");
        };
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = resolve_path(ctx.cwd, p);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("{}: {e}", path.display())),
        };
        match apply_edit(&text, old, new, replace_all) {
            Ok((updated, n)) => match std::fs::write(&path, updated) {
                Ok(()) => ToolResult::ok(format!(
                    "edited {} ({n} replacement{})",
                    path.display(),
                    if n == 1 { "" } else { "s" }
                )),
                Err(e) => ToolResult::err(format!("{}: {e}", path.display())),
            },
            Err(e) => ToolResult::err(e),
        }
    }
}

pub fn apply_edit(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("old_string must not be empty".into());
    }
    if old == new {
        return Err("old_string and new_string are identical".into());
    }
    let count = text.matches(old).count();
    if count == 0 {
        return Err("old_string not found in file".into());
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old_string matches {count} times; add context to make it unique or set replace_all"
        ));
    }
    Ok(if replace_all {
        (text.replace(old, new), count)
    } else {
        (text.replacen(old, new, 1), 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_rules() {
        assert_eq!(
            apply_edit("a b a", "b", "c", false).unwrap(),
            ("a c a".into(), 1)
        );
        assert!(
            apply_edit("a b a", "a", "c", false)
                .unwrap_err()
                .contains("2 times")
        );
        assert_eq!(
            apply_edit("a b a", "a", "c", true).unwrap(),
            ("c b c".into(), 2)
        );
        assert!(apply_edit("a", "z", "c", false).is_err());
        assert!(apply_edit("a", "", "c", false).is_err());
    }

    #[test]
    fn read_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ah-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let settings = ah_abi::ToolSettings::default();
        let ctx = ToolCtx {
            cwd: &dir,
            settings: &settings,
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
            std::fs::read_to_string(dir.join("sub/f.txt")).unwrap(),
            "one\n2\nthree\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
