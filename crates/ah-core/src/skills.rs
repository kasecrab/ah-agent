//! Saved prompts. A skill is `<name>.md` or `<name>/SKILL.md` under
//! `~/.config/ah/skills` or `.ah/skills`, optionally with a front matter
//! block holding `description:`. `$ARGUMENTS` in the body is replaced by
//! whatever the user typed after the name.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub body: String,
    /// `user` or `project`.
    pub scope: &'static str,
}

impl Skill {
    pub fn takes_args(&self) -> bool {
        self.body.contains("$ARGUMENTS")
    }

    /// Prompt to send: `$ARGUMENTS` substituted, or the arguments appended
    /// when the body has no placeholder.
    pub fn render(&self, args: &str) -> String {
        let args = args.trim();
        if self.takes_args() {
            self.body.replace("$ARGUMENTS", args)
        } else if args.is_empty() {
            self.body.clone()
        } else {
            format!("{}\n\n{args}", self.body)
        }
    }
}

/// Split `---` front matter off; returns (description, body).
fn parse(text: &str) -> (String, String) {
    let text = text.trim_start_matches('\u{feff}');
    let mut description = String::new();
    let body = if let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        && let Some(end) = rest.find("\n---")
    {
        for line in rest[..end].lines() {
            if let Some(d) = line.strip_prefix("description:") {
                description = d.trim().trim_matches('"').trim_matches('\'').to_string();
            }
        }
        let after = &rest[end + 4..];
        after
            .trim_start_matches(['-', '\r'])
            .trim_start_matches('\n')
    } else {
        text
    };
    let body = body.trim().to_string();
    if description.is_empty() {
        description = body
            .lines()
            .map(|l| l.trim().trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
    }
    (description, body)
}

fn read_dir(dir: &Path, scope: &'static str, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for p in paths {
        let (name, file) = if p.is_dir() {
            let f = p.join("SKILL.md");
            if !f.is_file() {
                continue;
            }
            (p.file_name(), f)
        } else if p.extension().is_some_and(|e| e == "md") {
            (p.file_stem(), p.clone())
        } else {
            continue;
        };
        let Some(name) = name.and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let (description, body) = parse(&text);
        if body.is_empty() {
            continue;
        }
        out.push(Skill {
            name,
            description,
            path: file,
            body,
            scope,
        });
    }
}

/// User skills first, then project skills; a project skill with the same
/// name replaces the user one.
pub fn load(cwd: &Path) -> Vec<Skill> {
    load_dirs(
        &crate::paths::config_dir().join("skills"),
        &cwd.join(crate::paths::project_dir()).join("skills"),
    )
}

pub fn load_dirs(user_dir: &Path, project_dir: &Path) -> Vec<Skill> {
    let mut out = Vec::new();
    read_dir(user_dir, "user", &mut out);
    let mut project = Vec::new();
    read_dir(project_dir, "project", &mut project);
    for s in project {
        out.retain(|u| u.name != s.name);
        out.push(s);
    }
    out
}

pub fn find<'a>(skills: &'a [Skill], name: &str) -> Option<&'a Skill> {
    skills.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_and_arguments() {
        let (d, b) =
            parse("---\ndescription: \"Review a file\"\n---\nReview $ARGUMENTS carefully.");
        assert_eq!(d, "Review a file");
        assert_eq!(b, "Review $ARGUMENTS carefully.");
        let (d, b) = parse("# Plan it\n\nMake a plan.");
        assert_eq!(d, "Plan it");
        assert_eq!(b, "# Plan it\n\nMake a plan.");
        let s = Skill {
            name: "r".into(),
            description: d,
            path: PathBuf::new(),
            body: b,
            scope: "user",
        };
        assert!(!s.takes_args());
        assert_eq!(s.render(""), "# Plan it\n\nMake a plan.");
        assert_eq!(s.render("x"), "# Plan it\n\nMake a plan.\n\nx");
    }

    #[test]
    fn loads_files_and_dirs_project_wins() {
        let t = std::env::temp_dir().join(format!("ah-skills-{}", std::process::id()));
        let user = t.join("cfg").join("skills");
        let proj = t.join("proj").join(".ah").join("skills");
        std::fs::create_dir_all(user.join("deploy")).unwrap();
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(user.join("review.md"), "Review $ARGUMENTS").unwrap();
        std::fs::write(user.join("deploy").join("SKILL.md"), "Deploy it").unwrap();
        std::fs::write(user.join("notes.txt"), "ignored").unwrap();
        std::fs::write(proj.join("review.md"), "Project review $ARGUMENTS").unwrap();
        let skills = load_dirs(&user, &proj);
        let names: Vec<(&str, &str)> = skills.iter().map(|s| (s.name.as_str(), s.scope)).collect();
        assert_eq!(names, vec![("deploy", "user"), ("review", "project")]);
        assert_eq!(
            find(&skills, "review").unwrap().render("a.rs"),
            "Project review a.rs"
        );
        let _ = std::fs::remove_dir_all(&t);
    }
}
