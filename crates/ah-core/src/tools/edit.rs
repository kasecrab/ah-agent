//! Matching for `edit_file`.
//!
//! Models reproduce code from memory, so an exact string is often nearly but
//! not quite what the file holds: a tab became spaces, a line picked up
//! trailing whitespace, a block was copied out of its indentation. Each of
//! those is tried in turn, and only a single unambiguous match is accepted, so
//! leniency never turns into editing the wrong place.

/// One replacement. Several are applied in order to the same buffer.
pub struct Edit<'a> {
    pub old: &'a str,
    pub new: &'a str,
    pub replace_all: bool,
}

/// How the text was found. Anything but `Exact` is reported back so the model
/// can see what it got wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    Exact,
    /// Same lines, ignoring trailing whitespace and carriage returns.
    LineEnds,
    /// Same lines, ignoring how far the block is indented.
    Indent,
    /// Same text, ignoring how much whitespace sits between words.
    Spacing,
}

impl How {
    fn note(self) -> Option<&'static str> {
        match self {
            How::Exact => None,
            How::LineEnds => Some("matched ignoring trailing whitespace"),
            How::Indent => Some("matched ignoring indentation, and re-indented to the file"),
            How::Spacing => Some("matched ignoring spacing between words"),
        }
    }
}

#[derive(Debug)]
pub struct Applied {
    pub text: String,
    pub replacements: usize,
    /// One line per edit that needed more than an exact match.
    pub notes: Vec<String>,
}

/// Apply every edit in order. Any failure leaves the text untouched.
pub fn apply(text: &str, edits: &[Edit<'_>]) -> Result<Applied, String> {
    if edits.is_empty() {
        return Err("no edits given".into());
    }
    let mut cur = text.to_string();
    let mut replacements = 0;
    let mut notes = Vec::new();
    for (i, e) in edits.iter().enumerate() {
        let label = if edits.len() == 1 {
            String::new()
        } else {
            format!("edit {}: ", i + 1)
        };
        let (next, n, how) = apply_one(&cur, e).map_err(|m| format!("{label}{m}"))?;
        if let Some(note) = how.note() {
            notes.push(format!("{label}{note}"));
        }
        cur = next;
        replacements += n;
    }
    Ok(Applied {
        text: cur,
        replacements,
        notes,
    })
}

struct Hit {
    start: usize,
    end: usize,
    replacement: String,
}

fn apply_one(text: &str, e: &Edit<'_>) -> Result<(String, usize, How), String> {
    if e.old.is_empty() {
        return Err("old_string must not be empty".into());
    }
    if e.old == e.new {
        return Err("old_string and new_string are identical".into());
    }
    let tiers = [
        (How::Exact, exact as fn(&str, &Edit<'_>) -> Vec<Hit>),
        (How::LineEnds, line_ends),
        (How::Indent, indent),
        (How::Spacing, spacing),
    ];
    for (how, find) in tiers {
        let hits = find(text, e);
        if hits.is_empty() {
            continue;
        }
        if hits.len() > 1 && !e.replace_all {
            let lines: Vec<String> = hits
                .iter()
                .take(5)
                .map(|h| line_of(text, h.start))
                .collect();
            return Err(format!(
                "old_string matches {} times (lines {}); add surrounding lines to make it unique, or set replace_all",
                hits.len(),
                lines.join(", ")
            ));
        }
        let take = if e.replace_all { hits.len() } else { 1 };
        return Ok((splice(text, &hits[..take]), take, how));
    }
    Err(not_found(text, e.old))
}

/// Rebuild `text` with every hit replaced. Hits are ordered and never overlap.
fn splice(text: &str, hits: &[Hit]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for h in hits {
        out.push_str(&text[at..h.start]);
        out.push_str(&h.replacement);
        at = h.end;
    }
    out.push_str(&text[at..]);
    out
}

fn exact(text: &str, e: &Edit<'_>) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut at = 0;
    while let Some(i) = text[at..].find(e.old) {
        let start = at + i;
        hits.push(Hit {
            start,
            end: start + e.old.len(),
            replacement: e.new.to_string(),
        });
        at = start + e.old.len();
    }
    hits
}

/// Byte range of every line, without its newline.
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            spans.push((start, i));
            start = i + 1;
        }
    }
    spans.push((start, text.len()));
    spans
}

/// The needle as lines, and whether it ended with a newline.
fn needle_lines(old: &str) -> (Vec<&str>, bool) {
    match old.strip_suffix('\n') {
        Some(rest) => (rest.split('\n').collect(), true),
        None => (old.split('\n').collect(), false),
    }
}

/// Match whole lines with `same`, and build the replacement with `build`.
fn by_lines(
    text: &str,
    e: &Edit<'_>,
    same: impl Fn(&str, &str) -> bool,
    build: impl Fn(&[&str], &str) -> String,
) -> Vec<Hit> {
    let (needle, trailing) = needle_lines(e.old);
    let spans = line_spans(text);
    if needle.is_empty() || needle.len() > spans.len() {
        return Vec::new();
    }
    let line = |i: usize| &text[spans[i].0..spans[i].1];
    let mut hits: Vec<Hit> = Vec::new();
    let mut i = 0;
    while i + needle.len() <= spans.len() {
        let matched = (0..needle.len()).all(|k| same(line(i + k), needle[k]));
        if !matched {
            i += 1;
            continue;
        }
        let last = i + needle.len() - 1;
        let mut end = spans[last].1;
        if trailing && end < text.len() {
            end += 1; // the newline the needle carried
        }
        let window: Vec<&str> = (i..=last).map(line).collect();
        let mut replacement = build(&window, e.new);
        if trailing && end <= text.len() && !replacement.ends_with('\n') {
            replacement.push('\n');
        }
        hits.push(Hit {
            start: spans[i].0,
            end,
            replacement,
        });
        i = last + 1;
    }
    hits
}

fn line_ends(text: &str, e: &Edit<'_>) -> Vec<Hit> {
    by_lines(
        text,
        e,
        |a, b| a.trim_end() == b.trim_end(),
        |_, new| new.to_string(),
    )
}

/// Leading whitespace shared by every non-empty line.
fn common_indent<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    let mut indent: Option<&str> = None;
    for l in lines {
        if l.trim().is_empty() {
            continue;
        }
        let lead = &l[..l.len() - l.trim_start().len()];
        indent = Some(match indent {
            None => lead,
            Some(cur) => {
                let n = cur
                    .bytes()
                    .zip(lead.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                // Two indentations can share the first bytes of a character
                // without sharing the character: U+2000 and U+2001 are
                // `E2 80 80` and `E2 80 81`, so the count above lands in the
                // middle of one of them and slicing there would panic on
                // whichever thread the edit is running on. What the two lines
                // really begin with ends at the last character boundary at or
                // before that byte.
                &cur[..cur.floor_char_boundary(n)]
            }
        });
    }
    indent.unwrap_or("").to_string()
}

fn indent(text: &str, e: &Edit<'_>) -> Vec<Hit> {
    let (needle, _) = needle_lines(e.old);
    let strip = common_indent(needle.iter().copied());
    if needle.iter().all(|l| l.trim().is_empty()) {
        return Vec::new();
    }
    by_lines(
        text,
        e,
        move |a, b| a.trim_end() != b.trim_end() && a.trim() == b.trim(),
        move |window, new| {
            let to = common_indent(window.iter().copied());
            reindent(new, &strip, &to)
        },
    )
}

/// Swap the block's own indentation for the file's.
fn reindent(new: &str, from: &str, to: &str) -> String {
    new.split('\n')
        .map(|l| match l.strip_prefix(from) {
            Some(rest) if !l.trim().is_empty() => format!("{to}{rest}"),
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text with runs of whitespace collapsed, and a map back to byte offsets.
fn collapse(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len());
    let mut space = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
            map.push(i);
        }
        space = false;
        let start = out.len();
        out.push(c);
        for _ in start..out.len() {
            map.push(i);
        }
    }
    map.push(text.len());
    (out, map)
}

fn spacing(text: &str, e: &Edit<'_>) -> Vec<Hit> {
    let (flat, map) = collapse(text);
    let (needle, _) = collapse(e.old);
    if needle.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let mut at = 0;
    while let Some(i) = flat[at..].find(&needle) {
        let s = at + i;
        let end_flat = s + needle.len();
        let (Some(&start), Some(&end)) = (map.get(s), map.get(end_flat)) else {
            break;
        };
        // Stop at the last non-space character of the match.
        let end = text[start..end]
            .trim_end()
            .len()
            .saturating_add(start)
            .min(text.len());
        hits.push(Hit {
            start,
            end,
            replacement: e.new.to_string(),
        });
        at = end_flat;
    }
    hits
}

fn line_of(text: &str, at: usize) -> String {
    (text[..at].bytes().filter(|b| *b == b'\n').count() + 1).to_string()
}

/// "not found", with the nearest thing in the file so the next try can work.
fn not_found(text: &str, old: &str) -> String {
    let (needle, _) = needle_lines(old);
    let spans = line_spans(text);
    let line = |i: usize| &text[spans[i].0..spans[i].1];
    let mut best: Option<(usize, usize)> = None;
    if needle.len() <= spans.len() {
        for i in 0..=(spans.len() - needle.len()) {
            let score = (0..needle.len())
                .filter(|&k| line(i + k).trim() == needle[k].trim())
                .count();
            if score > best.map_or(0, |(_, s)| s) {
                best = Some((i, score));
            }
        }
    }
    let mut msg = String::from("old_string not found in the file");
    if let Some((i, score)) = best
        && score * 2 >= needle.len()
    {
        let off = (0..needle.len())
            .find(|&k| line(i + k).trim() != needle[k].trim())
            .unwrap_or(0);
        msg.push_str(&format!(
            ". The closest text starts at line {}; it first differs at line {}, where the file has `{}` and old_string has `{}`",
            i + 1,
            i + off + 1,
            line(i + off).trim(),
            needle[off].trim()
        ));
    }
    msg.push_str(". Read the file again and copy the text you want to replace.");
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one<'a>(old: &'a str, new: &'a str) -> Vec<Edit<'a>> {
        vec![Edit {
            old,
            new,
            replace_all: false,
        }]
    }

    fn run(text: &str, old: &str, new: &str) -> Result<Applied, String> {
        apply(text, &one(old, new))
    }

    #[test]
    fn exact_first() {
        let a = run("one\ntwo\nthree\n", "two", "2").unwrap();
        assert_eq!(a.text, "one\n2\nthree\n");
        assert_eq!(a.replacements, 1);
        assert!(a.notes.is_empty());
    }

    #[test]
    fn trailing_whitespace_and_crlf_do_not_matter() {
        let a = run(
            "fn a() {\n    let x = 1;   \n}\n",
            "    let x = 1;\n",
            "    let x = 2;\n",
        )
        .unwrap();
        assert_eq!(a.text, "fn a() {\n    let x = 2;\n}\n");
        assert_eq!(a.notes.len(), 1);
        let a = run("a\r\nb\r\n", "a\nb\n", "c\n").unwrap();
        assert_eq!(a.text, "c\n");
    }

    #[test]
    fn a_block_copied_without_its_indentation_still_lands() {
        let text = "impl T {\n    fn go(&self) {\n        work();\n    }\n}\n";
        let a = run(
            text,
            "fn go(&self) {\n    work();\n}\n",
            "fn go(&self) {\n    work()?;\n}\n",
        )
        .unwrap();
        assert_eq!(
            a.text,
            "impl T {\n    fn go(&self) {\n        work()?;\n    }\n}\n"
        );
        assert!(a.notes[0].contains("indentation"), "{:?}", a.notes);
    }

    /// U+2000 and U+2001 are `E2 80 80` and `E2 80 81`: two whitespace
    /// prefixes that share their first two bytes without sharing a character.
    /// Both the text the model sent and the text in the file reach the
    /// indentation matcher, so both directions are tried here.
    #[test]
    fn indentation_in_different_multi_byte_spaces_does_not_panic() {
        let a = run(
            "    a();\n    b();\n",
            "\u{2000}a();\n\u{2001}b();\n",
            "c();\n",
        )
        .unwrap();
        assert_eq!(a.text, "    c();\n");
        let a = run(
            "\u{2000}a();\n\u{2001}b();\n",
            "    a();\n    b();\n",
            "c();\n",
        )
        .unwrap();
        assert_eq!(a.text, "c();\n");
    }

    #[test]
    fn spacing_between_words_can_differ() {
        let a = run("let x = foo(a,   b);\n", "foo(a, b)", "foo(b, a)").unwrap();
        assert_eq!(a.text, "let x = foo(b, a);\n");
        assert!(a.notes[0].contains("spacing"));
    }

    #[test]
    fn several_matches_are_refused_with_line_numbers() {
        let e = run("x\ny\nx\n", "x", "z").unwrap_err();
        assert!(e.contains("matches 2 times"), "{e}");
        assert!(e.contains("lines 1, 3"), "{e}");
        let a = apply(
            "x\ny\nx\n",
            &[Edit {
                old: "x",
                new: "z",
                replace_all: true,
            }],
        )
        .unwrap();
        assert_eq!(a.text, "z\ny\nz\n");
        assert_eq!(a.replacements, 2);
    }

    #[test]
    fn a_miss_points_at_the_closest_lines() {
        let text = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n";
        let e = run(
            text,
            "    let x = 1;\n    println!(\"{y}\");\n",
            "    let x = 2;\n",
        )
        .unwrap_err();
        assert!(e.contains("closest text starts at line 2"), "{e}");
        assert!(e.contains("first differs at line 3"), "{e}");
    }

    #[test]
    fn edits_apply_in_order_and_fail_together() {
        let text = "a\nb\nc\n";
        let a = apply(
            text,
            &[
                Edit {
                    old: "a",
                    new: "A",
                    replace_all: false,
                },
                Edit {
                    old: "c",
                    new: "C",
                    replace_all: false,
                },
            ],
        )
        .unwrap();
        assert_eq!(a.text, "A\nb\nC\n");
        assert_eq!(a.replacements, 2);
        let e = apply(
            text,
            &[
                Edit {
                    old: "a",
                    new: "A",
                    replace_all: false,
                },
                Edit {
                    old: "zzz",
                    new: "Z",
                    replace_all: false,
                },
            ],
        )
        .unwrap_err();
        assert!(e.starts_with("edit 2: "), "{e}");
    }

    #[test]
    fn empty_and_identical_edits_are_refused() {
        assert!(run("a", "", "b").is_err());
        assert!(run("a", "a", "a").is_err());
    }
}
