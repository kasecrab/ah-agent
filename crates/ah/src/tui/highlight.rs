//! Small lexer-based code highlighter. Linear scan, no regex, one keyword
//! table per language.

use ratatui::style::Style;

use super::theme::Palette;

#[derive(Clone, Copy)]
pub struct Lang {
    keywords: &'static [&'static str],
    line_comment: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
    /// Identifiers starting uppercase are types.
    caps_are_types: bool,
}

const RUST: Lang = Lang {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while", "u8", "u16", "u32", "u64", "u128",
        "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64", "bool", "char", "str",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"'],
    caps_are_types: true,
};
const PYTHON: Lang = Lang {
    keywords: &[
        "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
        "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
        "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return",
        "try", "while", "with", "yield", "self", "print",
    ],
    line_comment: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    caps_are_types: true,
};
const JS: Lang = Lang {
    keywords: &[
        "abstract",
        "as",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "let",
        "new",
        "null",
        "of",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "type",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "with",
        "yield",
        "string",
        "number",
        "boolean",
        "any",
        "never",
        "unknown",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\'', '`'],
    caps_are_types: true,
};
const GO: Lang = Lang {
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
        "nil",
        "true",
        "false",
        "string",
        "int",
        "int64",
        "int32",
        "uint",
        "byte",
        "bool",
        "error",
        "float64",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '`', '\''],
    caps_are_types: true,
};
const C: Lang = Lang {
    keywords: &[
        "auto",
        "break",
        "case",
        "char",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extern",
        "float",
        "for",
        "goto",
        "if",
        "inline",
        "int",
        "long",
        "register",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "struct",
        "switch",
        "typedef",
        "union",
        "unsigned",
        "void",
        "volatile",
        "while",
        "class",
        "namespace",
        "template",
        "typename",
        "public",
        "private",
        "protected",
        "virtual",
        "override",
        "new",
        "delete",
        "this",
        "nullptr",
        "true",
        "false",
        "using",
        "bool",
        "constexpr",
        "auto",
        "include",
        "define",
        "ifdef",
        "ifndef",
        "endif",
    ],
    line_comment: &["//", "#"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
    caps_are_types: true,
};
const JAVA: Lang = Lang {
    keywords: &[
        "abstract",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "float",
        "for",
        "if",
        "implements",
        "import",
        "instanceof",
        "int",
        "interface",
        "long",
        "native",
        "new",
        "null",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "try",
        "void",
        "volatile",
        "while",
        "true",
        "false",
        "var",
        "val",
        "fun",
        "when",
        "object",
        "data",
        "sealed",
    ],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\''],
    caps_are_types: true,
};
const SHELL: Lang = Lang {
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until", "case",
        "esac", "function", "return", "exit", "export", "local", "readonly", "set", "unset",
        "echo", "cd", "ls", "cat", "grep", "sed", "awk", "cargo", "git", "sudo", "rm", "cp", "mv",
        "mkdir", "curl", "just", "let", "def", "source",
    ],
    line_comment: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    caps_are_types: false,
};
const CONFIG: Lang = Lang {
    keywords: &["true", "false", "null", "yes", "no", "on", "off"],
    line_comment: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    caps_are_types: false,
};
const JSON: Lang = Lang {
    keywords: &["true", "false", "null"],
    line_comment: &[],
    block_comment: None,
    quotes: &['"'],
    caps_are_types: false,
};
const SQL: Lang = Lang {
    keywords: &[
        "select",
        "from",
        "where",
        "insert",
        "into",
        "values",
        "update",
        "set",
        "delete",
        "create",
        "table",
        "drop",
        "alter",
        "index",
        "join",
        "left",
        "right",
        "inner",
        "outer",
        "on",
        "group",
        "by",
        "order",
        "having",
        "limit",
        "offset",
        "as",
        "and",
        "or",
        "not",
        "null",
        "is",
        "in",
        "exists",
        "distinct",
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "primary",
        "key",
        "references",
        "default",
        "begin",
        "commit",
        "rollback",
        "with",
        "union",
        "case",
        "when",
        "then",
        "else",
        "end",
        "SELECT",
        "FROM",
        "WHERE",
        "INSERT",
        "INTO",
        "VALUES",
        "UPDATE",
        "SET",
        "DELETE",
        "CREATE",
        "TABLE",
        "DROP",
        "ALTER",
        "INDEX",
        "JOIN",
        "LEFT",
        "RIGHT",
        "INNER",
        "OUTER",
        "ON",
        "GROUP",
        "BY",
        "ORDER",
        "HAVING",
        "LIMIT",
        "OFFSET",
        "AS",
        "AND",
        "OR",
        "NOT",
        "NULL",
        "IS",
        "IN",
        "EXISTS",
        "DISTINCT",
        "PRIMARY",
        "KEY",
        "DEFAULT",
        "WITH",
        "UNION",
        "CASE",
        "WHEN",
        "THEN",
        "ELSE",
        "END",
    ],
    line_comment: &["--"],
    block_comment: Some(("/*", "*/")),
    quotes: &['\'', '"'],
    caps_are_types: false,
};
const RUBY: Lang = Lang {
    keywords: &[
        "alias",
        "and",
        "begin",
        "break",
        "case",
        "class",
        "def",
        "defined?",
        "do",
        "else",
        "elsif",
        "end",
        "ensure",
        "false",
        "for",
        "if",
        "in",
        "module",
        "next",
        "nil",
        "not",
        "or",
        "redo",
        "rescue",
        "retry",
        "return",
        "self",
        "super",
        "then",
        "true",
        "undef",
        "unless",
        "until",
        "when",
        "while",
        "yield",
        "require",
        "attr_accessor",
        "puts",
    ],
    line_comment: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    caps_are_types: true,
};
const LUA: Lang = Lang {
    keywords: &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if",
        "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ],
    line_comment: &["--"],
    block_comment: Some(("--[[", "]]")),
    quotes: &['"', '\''],
    caps_are_types: false,
};
const ZIG: Lang = Lang {
    keywords: &[
        "const",
        "var",
        "fn",
        "pub",
        "return",
        "if",
        "else",
        "while",
        "for",
        "switch",
        "break",
        "continue",
        "defer",
        "errdefer",
        "try",
        "catch",
        "struct",
        "enum",
        "union",
        "error",
        "unreachable",
        "comptime",
        "inline",
        "test",
        "and",
        "or",
        "orelse",
        "null",
        "undefined",
        "true",
        "false",
        "usize",
        "u8",
        "u32",
        "u64",
        "i32",
        "i64",
        "f32",
        "f64",
        "bool",
        "void",
        "anytype",
    ],
    line_comment: &["//"],
    block_comment: None,
    quotes: &['"', '\''],
    caps_are_types: true,
};

pub fn lang_for(info: &str) -> Option<Lang> {
    let name = info
        .split([' ', ',', '{'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match name.as_str() {
        "rust" | "rs" => RUST,
        "python" | "py" => PYTHON,
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" | "mjs" => JS,
        "go" | "golang" => GO,
        "c" | "cpp" | "c++" | "h" | "hpp" | "cc" | "objc" => C,
        "java" | "kotlin" | "kt" | "scala" | "cs" | "csharp" | "swift" => JAVA,
        "sh" | "bash" | "zsh" | "shell" | "fish" | "nu" | "nushell" | "console" | "makefile"
        | "make" | "dockerfile" => SHELL,
        "toml" | "yaml" | "yml" | "ini" | "conf" | "env" => CONFIG,
        "json" | "jsonc" | "json5" => JSON,
        "sql" | "psql" | "mysql" | "sqlite" => SQL,
        "ruby" | "rb" => RUBY,
        "lua" => LUA,
        "zig" => ZIG,
        _ => return None,
    })
}

/// Per-line highlighted spans. `state` carries an open block comment across lines.
#[derive(Default, Clone, Copy)]
pub struct State {
    in_block_comment: bool,
}

pub fn highlight_line(
    lang: &Lang,
    line: &str,
    pal: &Palette,
    base: Style,
    state: &mut State,
) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    let push = |out: &mut Vec<(String, Style)>, s: &str, st: Style| {
        if s.is_empty() {
            return;
        }
        if let Some((last, ls)) = out.last_mut()
            && *ls == st
        {
            last.push_str(s);
        } else {
            out.push((s.to_string(), st));
        }
    };
    let kw = base.fg(pal.syn_keyword);
    let string = base.fg(pal.syn_string);
    let comment = base.fg(pal.syn_comment);
    let number = base.fg(pal.syn_number);
    let ty = base.fg(pal.syn_type);
    let func = base.fg(pal.syn_function);

    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if state.in_block_comment {
            let end = lang.block_comment.map(|(_, e)| e).unwrap_or("*/");
            match line[i..].find(end) {
                Some(p) => {
                    push(&mut out, &line[i..i + p + end.len()], comment);
                    i += p + end.len();
                    state.in_block_comment = false;
                }
                None => {
                    push(&mut out, &line[i..], comment);
                    return out;
                }
            }
            continue;
        }
        let rest = &line[i..];
        if let Some((s, _)) = lang.block_comment
            && rest.starts_with(s)
        {
            state.in_block_comment = true;
            push(&mut out, s, comment);
            i += s.len();
            continue;
        }
        if lang.line_comment.iter().any(|c| rest.starts_with(c)) {
            push(&mut out, rest, comment);
            return out;
        }
        let c = b[i] as char;
        if lang.quotes.contains(&c) {
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] as char == c {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let j = j.min(b.len());
            push(&mut out, &line[i..j], string);
            i = j;
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'.' || b[j] == b'_') {
                j += 1;
            }
            push(&mut out, &line[i..j], number);
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let mut j = i + 1;
            while j < b.len()
                && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'?' || b[j] == b'!')
            {
                j += 1;
            }
            let word = &line[i..j];
            let next = b.get(j).copied();
            let style = if lang.keywords.contains(&word) {
                kw
            } else if next == Some(b'(') {
                func
            } else if lang.caps_are_types && c.is_ascii_uppercase() {
                ty
            } else {
                base
            };
            push(&mut out, word, style);
            i = j;
            continue;
        }
        // Everything else (punctuation, whitespace, multibyte) one char at a time.
        let ch_len = rest.chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
        push(&mut out, &line[i..i + ch_len], base);
        i += ch_len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ah_core::abi::Theme;

    #[test]
    fn rust_tokens() {
        let pal = Palette::from_theme(&Theme::default());
        let lang = lang_for("rust").unwrap();
        let mut st = State::default();
        let spans = highlight_line(
            &lang,
            "let x = foo(42); // hi \"s\"",
            &pal,
            Style::default(),
            &mut st,
        );
        let texts: Vec<&str> = spans.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(
            texts,
            vec!["let", " x = ", "foo", "(", "42", "); ", "// hi \"s\""]
        );
        assert_eq!(spans[0].1.fg, Some(pal.syn_keyword));
        assert_eq!(spans[2].1.fg, Some(pal.syn_function));
        assert_eq!(spans[4].1.fg, Some(pal.syn_number));
        assert_eq!(spans[6].1.fg, Some(pal.syn_comment));
    }

    #[test]
    fn block_comment_carries_state() {
        let pal = Palette::from_theme(&Theme::default());
        let lang = lang_for("c").unwrap();
        let mut st = State::default();
        highlight_line(&lang, "int a; /* open", &pal, Style::default(), &mut st);
        assert!(st.in_block_comment);
        let spans = highlight_line(&lang, "still */ int b;", &pal, Style::default(), &mut st);
        assert!(!st.in_block_comment);
        assert_eq!(spans[0].0, "still */");
        assert_eq!(spans[0].1.fg, Some(pal.syn_comment));
    }

    #[test]
    fn unknown_language_is_none() {
        assert!(lang_for("brainfuck").is_none());
        assert!(lang_for("").is_none());
        assert!(lang_for("rust,ignore").is_some());
    }
}
