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

/// Programs that take somebody else's privileges, and ask for a password at a
/// terminal to do it.
///
/// The asking is what matters. A password prompt goes to whatever terminal the
/// agent was started from, so in a session nobody is sitting in front of — one
/// the daemon started for a phone — the command does not fail, it waits, and
/// keeps waiting until the tool times out with nothing to show for it.
const ESCALATORS: [&str; 6] = ["sudo", "doas", "pkexec", "su", "sudoedit", "run0"];

/// Words that pass the start of a command along to the next word rather than
/// being the command themselves.
const WRAPPERS: [&str; 12] = [
    "env", "nohup", "time", "command", "exec", "setsid", "xargs", "nice", "ionice", "timeout",
    "stdbuf", "eval",
];

/// The first privilege escalation `command` would run, if any.
///
/// Only words in command position count, so `grep sudo /etc/group` is a search
/// and `sudo apt update` is not. A command substitution counts as a position of
/// its own, because it is one. This reads a shell command without a shell, so
/// it is a guard against the ordinary case and not a sandbox: something built
/// to get past it, `sh -c` with the name in a string, will.
pub fn escalates(command: &str) -> Option<&'static str> {
    let mut found = None;
    let mut at_start = true;
    let mut word = String::new();
    // The extra newline closes whatever word the command ends on.
    for ch in command.chars().chain(core::iter::once('\n')) {
        let breaks = matches!(ch, ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}' | '`');
        if !breaks && !ch.is_whitespace() {
            if ch != '"' && ch != '\'' {
                word.push(ch);
            }
            continue;
        }
        if !word.is_empty() && at_start {
            // A leading backslash is how an alias is stepped around, `^` is
            // how nushell says "the real program", and a full path is the same
            // program by a longer name.
            let head = word.trim_start_matches(['\\', '^']);
            // A redirection is written where a command goes and is not one:
            // `2>&1 sudo id` and `>/dev/null sudo id` both run sudo. Asked
            // before the path is trimmed, because `>/dev/null` trimmed to its
            // last segment is `null`, which looks like a program.
            let redirect = head.contains('>') || head.contains('<');
            let head = head.rsplit('/').next().unwrap_or(head);
            // `timeout 5 sudo id` is why a bare number passes along too: it
            // only ever reaches here as a wrapper's argument.
            let passes_along = WRAPPERS.contains(&head)
                || head.starts_with('-')
                || redirect
                || head.chars().all(|c| c.is_ascii_digit() || c == '.')
                || (head.contains('=') && !head.starts_with('-'));
            if !passes_along {
                if let Some(name) = ESCALATORS.iter().find(|e| **e == head) {
                    found = Some(*name);
                    break;
                }
                at_start = false;
            }
        }
        word.clear();
        if breaks {
            at_start = true;
        }
    }
    found
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
    fn a_password_prompt_is_seen_coming() {
        assert_eq!(escalates("sudo apt update"), Some("sudo"));
        assert_eq!(escalates("cd /tmp && sudo make install"), Some("sudo"));
        assert_eq!(escalates("env FOO=1 sudo sh"), Some("sudo"));
        assert_eq!(escalates("BAR=2 \\sudo -k id"), Some("sudo"));
        assert_eq!(escalates("/usr/bin/sudo id"), Some("sudo"));
        assert_eq!(escalates("echo $(sudo id)"), Some("sudo"));
        assert_eq!(escalates("pkexec id"), Some("pkexec"));
        assert_eq!(escalates("su - rabe"), Some("su"));
        assert_eq!(escalates("sudoedit /etc/hosts"), Some("sudoedit"));
        assert_eq!(escalates("run0 id"), Some("run0"));
    }

    /// Every one of these was written where a command goes and is not one, so
    /// the word after it is still the command.
    #[test]
    fn a_word_that_is_not_the_command_does_not_stand_in_for_it() {
        assert_eq!(escalates("2>&1 sudo id"), Some("sudo"));
        assert_eq!(escalates(">/dev/null sudo id"), Some("sudo"));
        assert_eq!(escalates("<input sudo id"), Some("sudo"));
        // nushell spells "the real program, not a builtin" with a caret, and
        // an empty `tools.shell` means whatever $SHELL is.
        assert_eq!(escalates("^sudo id"), Some("sudo"));
        // Wrappers that take an argument of their own.
        assert_eq!(escalates("timeout 5 sudo id"), Some("sudo"));
        assert_eq!(escalates("nice -n 10 sudo id"), Some("sudo"));
        assert_eq!(escalates("xargs sudo rm"), Some("sudo"));
        assert_eq!(escalates("eval sudo id"), Some("sudo"));
    }

    #[test]
    fn the_word_somewhere_else_in_a_command_is_just_a_word() {
        // Every one of these is somebody reading about sudo, not running it.
        assert_eq!(escalates("grep -rn sudo /etc/group"), None);
        assert_eq!(escalates("man sudo"), None);
        assert_eq!(escalates("git commit -m \"say why sudo is refused\""), None);
        assert_eq!(escalates("ls -l /usr/bin/sudo"), None);
        assert_eq!(escalates("cat /etc/sudoers"), None);
        assert_eq!(escalates("apt-get install -y sudo"), None);
        assert_eq!(escalates(""), None);
        // And a program whose name merely begins the same way.
        assert_eq!(escalates("superimpose a.png b.png"), None);
        assert_eq!(escalates("sudoku --solve puzzle.txt"), None);
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
