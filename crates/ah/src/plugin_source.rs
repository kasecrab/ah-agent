//! `ah plugin install`: fetch a plugin from a git repository, build it when it
//! ships as source, copy the `.wasm` into the user plugin directory and
//! remember where it came from so `ah plugin update` can do it again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AnyError;

/// Where a plugin came from. Stored in `<plugin dir>/sources.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    /// Directory inside the repository, empty for the root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Branch, tag or commit; the remote default branch when `None`.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// The commit the installed module was actually built from.
    ///
    /// A branch ref, or none at all, is a moving target: `ah plugin update`
    /// against one fetches whatever the author has pushed since, and installs
    /// it. Remembering what was installed is what lets an update say that the
    /// code changed, and ask, rather than swapping it silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl Source {
    pub fn describe(&self) -> String {
        let mut s = self.url.clone();
        if !self.path.is_empty() {
            s.push(' ');
            s.push_str(&self.path);
        }
        if let Some(r) = &self.git_ref {
            s.push_str(" @");
            s.push_str(r);
        }
        s
    }
}

/// Turn what the user typed into a repository, a subdirectory and a ref.
///
/// Accepted: any git URL or local path; `owner/repo` for GitHub; a GitHub or
/// GitLab `.../tree/<ref>/<path>` link, which fills in the ref and path.
/// Explicit `subdir` and `git_ref` win over what the link carried.
pub fn parse_source(spec: &str, subdir: Option<&str>, git_ref: Option<&str>) -> Source {
    let spec = spec.trim().trim_end_matches('/');
    let mut src = Source {
        url: spec.to_string(),
        path: String::new(),
        git_ref: None,
        commit: None,
    };
    if let Some((repo, rest)) = spec.split_once("/tree/") {
        src.url = repo.trim_end_matches("/-").to_string();
        let (r, p) = rest.split_once('/').unwrap_or((rest, ""));
        if !r.is_empty() {
            src.git_ref = Some(r.to_string());
        }
        src.path = p.trim_matches('/').to_string();
    } else if is_github_shorthand(spec) {
        src.url = format!("https://github.com/{spec}");
    }
    if let Some(d) = subdir {
        src.path = d.trim_matches('/').to_string();
    }
    if let Some(r) = git_ref {
        src.git_ref = Some(r.to_string());
    }
    src
}

fn is_github_shorthand(spec: &str) -> bool {
    let mut parts = spec.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    ok(owner) && ok(repo) && !Path::new(spec).exists()
}

/// Removes the clone directory when dropped.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Clone, build if needed, and copy the module(s) into `user_dir`.
///
/// `known` is the commit the installed copy was built from, when there is one
/// — an update rather than a first install. If the repository has moved since,
/// this is where the person is told and asked, because the new code is code
/// they have not agreed to run.
///
/// Returns the installed files and the commit they came from.
pub fn install(
    src: &Source,
    user_dir: &Path,
    known: Option<&str>,
) -> Result<(Vec<PathBuf>, String), AnyError> {
    let data = ah_core::paths::data_dir();
    let tmp = data
        .join("tmp")
        .join(format!("plugin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.parent().unwrap())?;
    let tmp = TempDir(tmp);
    let commit = clone(&src.url, src.git_ref.as_deref(), &tmp.0)?;
    if let Some(had) = known
        && had != commit
        && !agreed_to_move(&src.url, had, &commit)?
    {
        return Err("nothing was updated".into());
    }
    let dir = if src.path.is_empty() {
        tmp.0.clone()
    } else {
        tmp.0.join(&src.path)
    };
    if !dir.is_dir() {
        return Err(format!("`{}` is not a directory in {}", src.path, src.url).into());
    }
    let mut prebuilt = wasm_files(&dir);
    prebuilt.sort();
    let built = if !prebuilt.is_empty() {
        prebuilt
    } else if dir.join("Cargo.toml").exists() {
        // Building is not sandboxed and cannot be. `cargo build` runs the
        // crate's `build.rs` and every proc-macro it depends on, as this user,
        // before a single byte reaches the wasm interpreter — so this is the
        // moment to say so, rather than after. A prebuilt `.wasm` above needs
        // no such warning: it never runs outside the sandbox.
        if !agreed_to_build(&src.url, &dir)? {
            return Err("nothing was built or installed".into());
        }
        // A persistent target dir keeps updates incremental.
        build_crate(&dir, Some(&data.join("plugin-build")))?
    } else {
        return Err(format!(
            "{} has neither a .wasm file nor a Cargo.toml",
            if src.path.is_empty() {
                src.url.clone()
            } else {
                src.path.clone()
            }
        )
        .into());
    };
    std::fs::create_dir_all(user_dir)?;
    let mut installed = Vec::new();
    for f in built {
        let dest = user_dir.join(f.file_name().ok_or("bad wasm path")?);
        std::fs::copy(&f, &dest)?;
        installed.push(dest);
    }
    Ok((installed, commit))
}

/// Ask before replacing an installed plugin with code the author has pushed
/// since.
///
/// Pinning to a tag or a commit is the way not to be asked: what a tag or a
/// commit names does not change, so an update against one has nothing to say.
/// A branch, or no ref at all, is somebody else's newest work, and the run it
/// is about to do is a run of code nobody here has looked at.
///
/// With nothing to ask at — a script, a pipe — it refuses rather than assumes.
/// `AH_PLUGIN_UPDATE_YES=1` is how a script says yes on purpose.
fn agreed_to_move(url: &str, had: &str, now: &str) -> Result<bool, AnyError> {
    let short = |c: &str| c.chars().take(8).collect::<String>();
    if std::env::var("AH_PLUGIN_UPDATE_YES").is_ok_and(|v| v == "1") {
        return Ok(true);
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(format!(
            "{url} has moved from {} to {}, which is code this machine has not run before. Run \
             this where it can ask, pin it with `--ref <tag or commit>`, or set \
             AH_PLUGIN_UPDATE_YES=1 if you mean it.",
            short(had),
            short(now)
        )
        .into());
    }
    println!(
        "{url} has moved from {} to {} since it was installed.",
        short(had),
        short(now)
    );
    println!("That is code this machine has not run before.");
    print!("update it? [y/N] ");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Clone into `dest` and return the commit that was checked out.
fn clone(url: &str, git_ref: Option<&str>, dest: &Path) -> Result<String, AnyError> {
    // Local paths as file:// URLs so shallow clones work for them too.
    let url = match std::fs::canonicalize(url) {
        Ok(p) if p.is_dir() => format!("file://{}", p.display()),
        _ => url.to_string(),
    };
    let run = |args: &[&str]| -> Result<bool, AnyError> {
        let status = Command::new("git")
            .args(args)
            .status()
            .map_err(|e| format!("git: {e} (is git installed?)"))?;
        Ok(status.success())
    };
    let dest_s = dest.display().to_string();
    let mut args = vec!["clone", "--quiet", "--depth", "1"];
    if let Some(r) = git_ref {
        args.extend(["--branch", r]);
    }
    args.extend([url.as_str(), dest_s.as_str()]);
    if run(&args)? {
        return head_of(&dest_s);
    }
    // `--branch` rejects commit hashes: full clone, then check out.
    if let Some(r) = git_ref {
        let _ = std::fs::remove_dir_all(dest);
        if run(&["clone", "--quiet", &url, &dest_s])?
            && run(&["-C", &dest_s, "checkout", "--quiet", r])?
        {
            return head_of(&dest_s);
        }
    }
    Err(format!("git clone of {url} failed").into())
}

/// The commit a clone ended up on.
fn head_of(dir: &str) -> Result<String, AnyError> {
    let out = Command::new("git")
        .args(["-C", dir, "rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err("git rev-parse failed after a clone that succeeded".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn wasm_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "wasm"))
                .collect()
        })
        .unwrap_or_default()
}

/// Ask before compiling somebody else's crate on this machine.
///
/// The wasm sandbox is what makes a plugin safe to run. It does nothing for
/// building one: `build.rs` and proc-macros are ordinary programs, run as this
/// user, with this user's files and this user's network. Installing from a
/// source repository is therefore a decision about trusting the author, and it
/// is one worth being asked for out loud.
///
/// With nothing to ask at — a script, a pipe — it refuses rather than assumes.
/// `AH_PLUGIN_BUILD_YES=1` is how a script says yes on purpose.
fn agreed_to_build(url: &str, dir: &Path) -> Result<bool, AnyError> {
    if std::env::var("AH_PLUGIN_BUILD_YES").is_ok_and(|v| v == "1") {
        return Ok(true);
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(format!(
            "{url} ships no .wasm, so installing it means compiling it, and compiling runs its \
             build script and proc-macros on this machine as you. Run this where it can ask, or \
             set AH_PLUGIN_BUILD_YES=1 if you mean it."
        )
        .into());
    }
    println!("{url} ships no built plugin, only source.");
    println!(
        "Building it runs {}'s build script and every proc-macro it depends on,",
        dir.display()
    );
    println!("on this machine, as you, before any of it reaches the wasm sandbox.");
    print!("build it? [y/N] ");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// `cargo build --release --target wasm32-unknown-unknown` in `dir` and
/// return the `.wasm` of every `cdylib` package that crate defines.
pub fn build_crate(dir: &Path, target_dir: Option<&Path>) -> Result<Vec<PathBuf>, AnyError> {
    if let Ok(out) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        && !String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == "wasm32-unknown-unknown")
    {
        return Err("the wasm32-unknown-unknown target is not installed; run `rustup target add wasm32-unknown-unknown`".into());
    }
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(dir);
    if let Some(t) = target_dir {
        cmd.env("CARGO_TARGET_DIR", t);
    }
    let out = cmd.output().map_err(|e| format!("cargo: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata failed in {}:\n{}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    let meta: Value = serde_json::from_slice(&out.stdout)?;
    let out_dir = PathBuf::from(
        meta["target_directory"]
            .as_str()
            .ok_or("cargo metadata: no target_directory")?,
    )
    .join("wasm32-unknown-unknown/release");
    let names = cdylib_names(&meta, &dir.join("Cargo.toml"));
    if names.is_empty() {
        return Err(format!(
            "{} has no package with `crate-type = [\"cdylib\"]`",
            dir.display()
        )
        .into());
    }
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(dir);
    if let Some(t) = target_dir {
        cmd.env("CARGO_TARGET_DIR", t);
    }
    if !cmd.status().map_err(|e| format!("cargo: {e}"))?.success() {
        return Err("cargo build failed".into());
    }
    let found: Vec<PathBuf> = names
        .iter()
        .map(|n| out_dir.join(format!("{}.wasm", n.replace('-', "_"))))
        .filter(|p| p.exists())
        .collect();
    if found.is_empty() {
        return Err(format!("build succeeded but no .wasm in {}", out_dir.display()).into());
    }
    Ok(found)
}

/// Names of the `cdylib` packages defined by `manifest`; every `cdylib` in
/// the workspace when `manifest` is a virtual workspace root.
fn cdylib_names(meta: &Value, manifest: &Path) -> Vec<String> {
    let manifest = std::fs::canonicalize(manifest).unwrap_or(manifest.to_path_buf());
    let packages = meta["packages"].as_array().cloned().unwrap_or_default();
    let is_cdylib = |p: &Value| {
        p["targets"].as_array().is_some_and(|ts| {
            ts.iter().any(|t| {
                t["kind"]
                    .as_array()
                    .is_some_and(|k| k.iter().any(|x| x == "cdylib"))
            })
        })
    };
    let name = |p: &Value| p["name"].as_str().map(str::to_string);
    let own: Vec<String> = packages
        .iter()
        .filter(|p| {
            p["manifest_path"]
                .as_str()
                .map(PathBuf::from)
                .and_then(|m| std::fs::canonicalize(m).ok())
                .is_some_and(|m| m == manifest)
        })
        .filter(|p| is_cdylib(p))
        .filter_map(name)
        .collect();
    if !own.is_empty() {
        return own;
    }
    packages
        .iter()
        .filter(|p| is_cdylib(p))
        .filter_map(name)
        .collect()
}

// ---- sources record ------------------------------------------------------

pub type Sources = BTreeMap<String, Source>;

pub fn sources_file(user_dir: &Path) -> PathBuf {
    user_dir.join("sources.json")
}

pub fn load_sources(user_dir: &Path) -> Sources {
    std::fs::read(sources_file(user_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_sources(user_dir: &Path, sources: &Sources) -> Result<(), AnyError> {
    std::fs::create_dir_all(user_dir)?;
    let file = sources_file(user_dir);
    if sources.is_empty() {
        let _ = std::fs::remove_file(file);
        return Ok(());
    }
    std::fs::write(file, serde_json::to_vec_pretty(sources)?)?;
    Ok(())
}

/// File stem used as the key in `sources.json` and by `ah plugin rm`.
pub fn stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_tree_links_carry_ref_and_path() {
        let s = parse_source(
            "https://github.com/kasecrab/ah-agent/tree/main/plugins/themes",
            None,
            None,
        );
        assert_eq!(s.url, "https://github.com/kasecrab/ah-agent");
        assert_eq!(s.git_ref.as_deref(), Some("main"));
        assert_eq!(s.path, "plugins/themes");
        let s = parse_source("https://github.com/o/r/tree/v1.2", None, None);
        assert_eq!(s.git_ref.as_deref(), Some("v1.2"));
        assert_eq!(s.path, "");
        let s = parse_source("https://gitlab.com/o/r/-/tree/dev/sub/dir/", None, None);
        assert_eq!(s.url, "https://gitlab.com/o/r");
        assert_eq!(s.git_ref.as_deref(), Some("dev"));
        assert_eq!(s.path, "sub/dir");
    }

    #[test]
    fn shorthand_and_overrides() {
        let s = parse_source("kasecrab/ah-agent", Some("plugins/themes"), Some("main"));
        assert_eq!(s.url, "https://github.com/kasecrab/ah-agent");
        assert_eq!(s.path, "plugins/themes");
        assert_eq!(s.git_ref.as_deref(), Some("main"));
        let s = parse_source("git@github.com:o/r.git", None, None);
        assert_eq!(s.url, "git@github.com:o/r.git");
        assert!(s.git_ref.is_none());
        let s = parse_source("https://github.com/o/r/tree/main/a", Some("b"), Some("x"));
        assert_eq!(s.path, "b");
        assert_eq!(s.git_ref.as_deref(), Some("x"));
        assert_eq!(s.describe(), "https://github.com/o/r b @x");
        let s = parse_source("/tmp", None, None);
        assert_eq!(s.url, "/tmp");
    }

    #[test]
    fn sources_round_trip() {
        let dir = std::env::temp_dir().join(format!("ah-sources-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_sources(&dir).is_empty());
        let mut s = Sources::new();
        s.insert("themes".into(), parse_source("o/r", Some("p"), None));
        save_sources(&dir, &s).unwrap();
        assert_eq!(load_sources(&dir), s);
        let text = std::fs::read_to_string(sources_file(&dir)).unwrap();
        assert!(!text.contains("\"ref\""), "{text}");
        save_sources(&dir, &Sources::new()).unwrap();
        assert!(!sources_file(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cdylib_packages_from_metadata() {
        let dir = std::env::temp_dir().join(format!("ah-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let own = dir.join("Cargo.toml");
        std::fs::write(&own, "").unwrap();
        let own_s = std::fs::canonicalize(&own).unwrap().display().to_string();
        let meta = serde_json::json!({"packages": [
            {"name": "a-lib", "manifest_path": own_s, "targets": [{"kind": ["cdylib"]}]},
            {"name": "bin", "manifest_path": own_s, "targets": [{"kind": ["bin"]}]},
            {"name": "other", "manifest_path": "/nowhere/Cargo.toml", "targets": [{"kind": ["cdylib"]}]}
        ]});
        assert_eq!(cdylib_names(&meta, &own), vec!["a-lib".to_string()]);
        let virtual_root = dir.join("sub/Cargo.toml");
        assert_eq!(
            cdylib_names(&meta, &virtual_root),
            vec!["a-lib".to_string(), "other".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
