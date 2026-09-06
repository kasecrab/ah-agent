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
}
