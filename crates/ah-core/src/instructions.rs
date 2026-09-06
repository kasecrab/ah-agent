//! Project instruction files (`AGENTS.md` and friends) folded into the system
//! prompt. Looked up in the user config dir, then in every directory from the
//! repository root down to the working directory, then in `.ah/`.

use std::path::{Path, PathBuf};

/// One instruction file that was found and read.
#[derive(Debug, Clone, PartialEq)]
pub struct Instructions {
    pub path: PathBuf,
    pub text: String,
}

/// Directories searched, outermost first. Stops walking up at the directory
/// holding `.git`, or at the filesystem root.
fn dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut d = Some(cwd.to_path_buf());
    while let Some(p) = d {
        let is_root = p.join(".git").exists();
        chain.push(p.clone());
        if is_root {
            break;
        }
        d = p.parent().map(Path::to_path_buf);
    }
    chain.reverse();
    let mut out = vec![crate::paths::config_dir()];
    out.extend(chain);
    out.push(cwd.join(crate::paths::project_dir()));
    out
}

/// First readable `names` entry in each searched directory.
pub fn load(cwd: &Path, names: &[String]) -> Vec<Instructions> {
    let mut out = Vec::new();
    for dir in dirs(cwd) {
        for n in names {
            let path = dir.join(n);
            if let Ok(text) = std::fs::read_to_string(&path) {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    out.push(Instructions { path, text });
                }
                break;
            }
        }
    }
    out
}

/// Text appended to the system prompt.
pub fn render(found: &[Instructions]) -> String {
    let mut s = String::new();
    for i in found {
        s.push_str(&format!(
            "\n\n# Instructions from {}\n\n{}",
            i.path.display(),
            i.text
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_up_to_git_root_and_prefers_first_name() {
        let t = std::env::temp_dir().join(format!("ah-instr-{}", std::process::id()));
        let sub = t.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(t.join(".git")).unwrap();
        std::fs::write(t.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(t.join("a").join("CLAUDE.md"), "mid rules").unwrap();
        std::fs::write(t.join("a").join("AGENTS.md"), "mid agents").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "   ").unwrap();
        let names = vec!["AGENTS.md".to_string(), "CLAUDE.md".to_string()];
        let found = load(&sub, &names);
        let texts: Vec<&str> = found.iter().map(|i| i.text.as_str()).collect();
        assert!(texts.ends_with(&["root rules", "mid agents"]), "{texts:?}");
        assert!(render(&found).contains("# Instructions from"));
        let _ = std::fs::remove_dir_all(&t);
    }
}
