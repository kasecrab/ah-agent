use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use ah_abi::{ToolResult, ToolSettings, ToolSpec};
use serde_json::{Value, json};

use super::{Tool, ToolCtx, arg_str, arg_u64, edit, resolve_path};

pub struct ReadFile;
pub struct WriteFile;
pub struct EditFile;

/// The most of one file these tools hold in memory at a time.
///
/// The line limit and the truncation that follow a read are no help at all
/// while the file is still being read: they run once the whole thing is
/// already in memory. `/dev/zero` hands out bytes until the allocator gives
/// up — and with `panic = "abort"` that is the whole program, every agent and
/// every background job with it — and a FIFO simply never ends. A text file
/// worth reading is far below this; a file above it is one to look at a piece
/// at a time through a shell command.
const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;

/// Read a file whole, with a bound on what it may be as well as how much of
/// it there is.
///
/// Only a regular file is read: a device, a directory, a socket or a FIFO is
/// refused by name rather than waited on. The size is taken from the open file
/// rather than from a second look at the path, so the file that is measured is
/// the file that is read, and the read still stops at the cap in case it grew
/// in between.
fn read_capped(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(too_large(meta.len()));
    }
    let mut buf = Vec::with_capacity(meta.len() as usize + 1);
    file.take(MAX_READ_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_READ_BYTES {
        return Err(too_large(buf.len() as u64));
    }
    Ok(buf)
}

fn too_large(len: u64) -> std::io::Error {
    std::io::Error::other(format!(
        "{len} bytes is more than this tool reads at once ({MAX_READ_BYTES}); \
         read it a piece at a time with a shell command such as `sed -n`"
    ))
}

/// The same, as text, so that a caller can tell a file that is not there from
/// a file that is not text.
fn read_capped_text(path: &Path) -> std::io::Result<String> {
    let bytes = read_capped(path)?;
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not text (invalid UTF-8 bytes)",
        )
    })
}

/// Where a write through `path` would actually land, when that is somewhere
/// outside the directory being worked in.
///
/// `std::fs::write` opens what a symbolic link points at and truncates that.
/// The person approving the call is shown the path the model asked for, so a
/// link planted in a checkout — `docs/CHANGELOG.md` pointing at `~/.bashrc` —
/// turns an approved write inside the project into a rewrite of a file
/// somewhere else entirely. A link that stays inside the working directory is
/// the ordinary kind and is followed as before; one that leaves it is refused,
/// and the refusal names the real destination, so the model can ask for that
/// path outright and have it put to the person under its own name.
///
/// The link is looked at and then written through, so in principle it can be
/// swapped in between. Closing that would mean opening with `O_NOFOLLOW` and
/// writing through the handle; this is the check the permission prompt is
/// missing, not a defence against somebody who can already race files in the
/// working directory.
fn leads_outside(cwd: &Path, path: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    let target = link_target(path)?;
    let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| tidy(cwd));
    (!target.starts_with(&root)).then_some(target)
}

/// What a link points at, resolved as far as the filesystem allows. A link
/// whose target does not exist yet cannot be canonicalised and the write would
/// create it, so that name is put together by hand instead.
fn link_target(link: &Path) -> Option<PathBuf> {
    if let Ok(real) = std::fs::canonicalize(link) {
        return Some(real);
    }
    let raw = std::fs::read_link(link).ok()?;
    let joined = if raw.is_absolute() {
        raw
    } else {
        link.parent()?.join(raw)
    };
    // The directory the target would be created in is often reachable even
    // when the target is not; resolving it settles any `..` and any link
    // further up.
    match (
        joined.parent().map(std::fs::canonicalize),
        joined.file_name(),
    ) {
        (Some(Ok(dir)), Some(name)) => Some(dir.join(name)),
        _ => Some(tidy(&joined)),
    }
}

/// `.` dropped and `..` folded away, for a path that cannot be canonicalised
/// because it is not there.
fn tidy(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The refusal shown for a write that a link would carry out of the tree.
fn wrong_destination(path: &Path, target: &Path, cwd: &Path) -> ToolResult {
    ToolResult::err(format!(
        "{}: a symbolic link to {}, which is outside {}. Writing through it would change that \
         file instead of this one; name the path you mean if that is what you want.",
        path.display(),
        target.display(),
        cwd.display()
    ))
}

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
        let bytes = match read_capped(&path) {
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
        if let Some(target) = leads_outside(ctx.cwd, &path) {
            return wrong_destination(&path, &target, ctx.cwd);
        }
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolResult::err(format!("mkdir {}: {e}", parent.display()));
        }
        let existed = std::fs::symlink_metadata(&path).is_ok();
        let before = read_capped_text(&path).ok();
        match std::fs::write(&path, content) {
            Ok(()) => {
                let mut r = ToolResult::ok(format!(
                    "{} {} ({} bytes, {} lines)",
                    if existed { "overwrote" } else { "created" },
                    path.display(),
                    content.len(),
                    content.lines().count()
                ));
                // A file that was there and could not be read — too large for
                // one read, or not text — has no old side to show, and a diff
                // against nothing would claim it was empty.
                r.diff = match (&before, existed) {
                    (Some(b), _) => Some(super::diff::unified(b, content, 2)),
                    (None, false) => Some(super::diff::unified("", content, 2)),
                    (None, true) => None,
                };
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
        if let Some(target) = leads_outside(ctx.cwd, &path) {
            return wrong_destination(&path, &target, ctx.cwd);
        }
        let text = match read_capped_text(&path) {
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
    fn a_file_that_is_not_a_regular_file_or_is_too_large_is_refused() {
        let dir = std::env::temp_dir().join(format!("ah-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings = ah_abi::ToolSettings::default();
        let ctx = ToolCtx {
            cwd: &dir,
            settings: &settings,
            agent: 0,
            cancel: crate::tools::never(),
            ask: crate::tools::no_user(),
            spawn: None,
        };
        // Sparse: as large as anything reading it is concerned, and free to
        // make.
        let f = std::fs::File::create(dir.join("big.log")).unwrap();
        f.set_len(MAX_READ_BYTES + 4096).unwrap();
        drop(f);
        let r = ReadFile.run(&json!({"path": "big.log"}), &ctx);
        assert!(r.is_error, "{}", r.output);
        assert!(
            r.output.contains("more than this tool reads"),
            "{}",
            r.output
        );
        let e = EditFile.run(
            &json!({"path": "big.log", "old_string": "a", "new_string": "b"}),
            &ctx,
        );
        assert!(e.is_error, "{}", e.output);

        // Something that is not a file at all is refused rather than read
        // until the process dies of it.
        #[cfg(unix)]
        {
            let r = ReadFile.run(&json!({"path": "/dev/zero"}), &ctx);
            assert!(r.is_error, "{}", r.output);
            assert!(r.output.contains("regular file"), "{}", r.output);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_write_through_a_link_out_of_the_tree_is_refused() {
        let base = std::env::temp_dir().join(format!("ah-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let outside = base.join("outside.txt");
        std::fs::write(&outside, "keep me\n").unwrap();
        std::os::unix::fs::symlink(&outside, work.join("notes.md")).unwrap();
        let settings = ah_abi::ToolSettings::default();
        let ctx = ToolCtx {
            cwd: &work,
            settings: &settings,
            agent: 0,
            cancel: crate::tools::never(),
            ask: crate::tools::no_user(),
            spawn: None,
        };

        let w = WriteFile.run(&json!({"path": "notes.md", "content": "theirs\n"}), &ctx);
        assert!(w.is_error, "{}", w.output);
        assert!(w.output.contains("outside.txt"), "{}", w.output);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep me\n");

        let e = EditFile.run(
            &json!({"path": "notes.md", "old_string": "keep", "new_string": "drop"}),
            &ctx,
        );
        assert!(e.is_error, "{}", e.output);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep me\n");

        // A link that stays inside the directory being worked in is the
        // ordinary kind and still works.
        std::fs::write(work.join("real.txt"), "one\n").unwrap();
        std::os::unix::fs::symlink(work.join("real.txt"), work.join("link.txt")).unwrap();
        let ok = WriteFile.run(&json!({"path": "link.txt", "content": "two\n"}), &ctx);
        assert!(!ok.is_error, "{}", ok.output);
        assert_eq!(
            std::fs::read_to_string(work.join("real.txt")).unwrap(),
            "two\n"
        );
        let _ = std::fs::remove_dir_all(&base);
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
