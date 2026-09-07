//! Command deny rules. A rule matches a shell command when any segment of it
//! (split on `&&`, `||`, `;`, `|`), with a leading `sudo` or `env` prefix
//! removed, equals the rule or starts with the rule followed by a space. A
//! rule ending in `*` matches any continuation.

/// Segments of `command`, whitespace-normalised, `sudo`/`env` prefixes dropped.
fn segments(command: &str) -> Vec<String> {
    command
        .split(['\n', ';', '|', '&'])
        .map(|s| {
            let mut words: Vec<&str> = s.split_whitespace().collect();
            while let Some(first) = words.first() {
                let wrapper = matches!(*first, "sudo" | "env" | "nohup" | "time");
                let assignment = first.contains('=') && !first.starts_with('-');
                if !(wrapper || assignment) {
                    break;
                }
                words.remove(0);
            }
            words.join(" ")
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn rule_matches(rule: &str, segment: &str) -> bool {
    let rule: String = rule.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(prefix) = rule.strip_suffix('*') {
        return segment.starts_with(prefix.trim_end());
    }
    segment == rule || segment.starts_with(&format!("{rule} "))
}

/// The first rule that matches `command`, if any.
pub fn denied<'a>(command: &str, rules: &'a [String]) -> Option<&'a str> {
    let segs = segments(command);
    rules
        .iter()
        .find(|r| segs.iter().any(|s| rule_matches(r, s)))
        .map(String::as_str)
}

/// True when every segment of `command` matches one of `rules` and the command
/// neither redirects nor substitutes, which is as close to "reads only" as a
/// shell command can be judged without running it.
pub fn read_only(command: &str, rules: &[String]) -> bool {
    if command.contains('>') || command.contains('`') || command.contains("$(") {
        return false;
    }
    let segs = segments(command);
    !segs.is_empty()
        && segs
            .iter()
            .all(|s| rules.iter().any(|r| rule_matches(r, s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_segments_and_boundaries() {
        let rules: Vec<String> = ["git reset --hard", "rm -rf /", "git push --force", "mkfs*"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            denied("git reset --hard HEAD~1", &rules),
            Some("git reset --hard")
        );
        assert_eq!(denied("cd x && sudo rm -rf /", &rules), Some("rm -rf /"));
        assert_eq!(denied("rm -rf /tmp/build", &rules), None);
        assert_eq!(denied("git push --force-with-lease", &rules), None);
        assert_eq!(denied("mkfs.ext4 /dev/sdb", &rules), Some("mkfs*"));
        assert_eq!(denied("git reset --soft HEAD~1", &rules), None);
    }

    #[test]
    fn read_only_needs_every_segment() {
        let rules: Vec<String> = ["rg", "git log", "wc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(read_only("rg -n todo src | wc -l", &rules));
        assert!(read_only("git log --oneline -5", &rules));
        assert!(!read_only("rg -n todo > out.txt", &rules));
        assert!(!read_only("rg -n $(cat f)", &rules));
        assert!(!read_only("rg -n todo && rm -rf x", &rules));
        assert!(!read_only("git commit -m x", &rules));
        assert!(!read_only("", &rules));
    }
}
