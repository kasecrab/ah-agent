//! Documentation pages embedded in the binary, served by `ah docs` and
//! pointed at from the system prompt so the model can look ah up.

pub struct Topic {
    pub name: &'static str,
    pub summary: &'static str,
    pub text: &'static str,
}

pub const TOPICS: &[Topic] = &[
    Topic {
        name: "index",
        summary: "overview, file locations, environment variables, customisation map",
        text: include_str!("../../../docs/index.md"),
    },
    Topic {
        name: "config",
        summary: "every setting with its default, config files, layering, --set and /set",
        text: include_str!("../../../docs/config.md"),
    },
    Topic {
        name: "keys",
        summary: "key bindings and the key string syntax",
        text: include_str!("../../../docs/keys.md"),
    },
    Topic {
        name: "commands",
        summary: "CLI flags and subcommands, slash commands, built-in tools, --json events",
        text: include_str!("../../../docs/commands.md"),
    },
    Topic {
        name: "instructions",
        summary: "AGENTS.md and CLAUDE.md project instruction files, /init",
        text: include_str!("../../../docs/instructions.md"),
    },
    Topic {
        name: "skills",
        summary: "saved prompts in skills directories, /skills",
        text: include_str!("../../../docs/skills.md"),
    },
    Topic {
        name: "plugins",
        summary: "writing, building and installing wasm plugins: manifest, hooks, host calls",
        text: include_str!("../../../docs/plugins.md"),
    },
    Topic {
        name: "sessions",
        summary: "session files, resume, naming, compaction, history and model cache",
        text: include_str!("../../../docs/sessions.md"),
    },
];

pub fn find(name: &str) -> Option<&'static Topic> {
    let name = name.trim().trim_end_matches(".md").to_ascii_lowercase();
    TOPICS.iter().find(|t| t.name == name)
}

/// `name  summary` lines for `ah docs`.
pub fn list() -> String {
    TOPICS
        .iter()
        .map(|t| format!("{:<13} {}", t.name, t.summary))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Paragraph appended to the system prompt when `prompt.docs_hint` is on.
pub fn hint() -> String {
    let names: Vec<&str> = TOPICS.iter().map(|t| t.name).collect();
    format!(
        "Documentation for ah itself (the agent you are running in) is built into the binary. \
         Only when the user asks about ah or wants to configure or extend it, run `ah docs <topic>` \
         with the bash tool and read the whole page before answering; do not guess ah settings. \
         Topics: {}.",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ah_abi::{Hook, Settings};

    #[test]
    fn topics_are_pages() {
        for t in TOPICS {
            assert!(t.text.starts_with("# "), "{} has no title", t.name);
            assert!(t.text.len() > 500, "{} is too short", t.name);
        }
        assert_eq!(find("Config").map(|t| t.name), Some("config"));
        assert_eq!(find("plugins.md").map(|t| t.name), Some("plugins"));
        assert!(find("nope").is_none());
        assert!(hint().contains("ah docs"));
        assert!(list().contains("sessions"));
    }

    fn leaf_keys(prefix: &str, v: &serde_json::Value, out: &mut Vec<String>) {
        match v.as_object() {
            Some(m) if !m.is_empty() => {
                for (k, v) in m {
                    let p = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    leaf_keys(&p, v, out);
                }
            }
            _ => out.push(prefix.to_string()),
        }
    }

    #[test]
    fn config_page_names_every_setting() {
        let config = find("config").unwrap().text;
        let keys = find("keys").unwrap().text;
        let v = serde_json::to_value(Settings::default()).unwrap();
        let mut leaves = Vec::new();
        leaf_keys("", &v, &mut leaves);
        for key in leaves {
            let leaf = key.rsplit('.').next().unwrap();
            let page = if key.starts_with("keys.") {
                keys
            } else {
                config
            };
            assert!(
                page.contains(&format!("`{leaf}`")),
                "setting `{key}` is not documented"
            );
        }
    }

    #[test]
    fn plugins_page_names_every_hook_and_host_call() {
        let page = find("plugins").unwrap().text;
        for h in Hook::ALL {
            assert!(page.contains(&format!("`{}`", h.as_str())), "{h:?}");
        }
        for call in [
            "settings_get",
            "kv_get",
            "kv_set",
            "cwd",
            "now_ms",
            "env_get",
            "read_file",
            "git_branch",
        ] {
            assert!(page.contains(&format!("| `{call}` |")), "{call}");
        }
    }
}
