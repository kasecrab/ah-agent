# Skills

A skill is a saved prompt the user runs by name. Skills never get their own
slash commands; one `/skills` picker lists all of them, so the command popup
stays short however many exist.

## Files

| Location | Scope |
|---|---|
| `~/.config/ah/skills/<name>.md` or `~/.config/ah/skills/<name>/SKILL.md` | user |
| `.ah/skills/<name>.md` or `.ah/skills/<name>/SKILL.md` | project (wins on a name clash) |

The name is the file stem or the directory name. Directories are scanned when
the picker opens or a skill is run, so new files need no reload.

## Format

Optional front matter, then the prompt body:

```markdown
---
description: Review a file for bugs and style problems
---
Review `$ARGUMENTS` carefully. Report bugs first, then style. Suggest fixes
as diffs.
```

- `description:` is shown in the picker. Without front matter the first line
  of the body is used.
- `$ARGUMENTS` is replaced by whatever the user typed after the name. A skill
  that contains it prompts for arguments when run from the picker without any.
- A body without `$ARGUMENTS` gets the arguments appended after a blank line,
  or is sent as is when there are none.

## Running

| Input | Effect |
|---|---|
| `/skills` | picker over every skill (fuzzy filter, Enter runs, Esc closes) |
| `/skills review src/main.rs` | run `review` with `src/main.rs` as `$ARGUMENTS` |
| `/skills review` | run `review`; a skill that takes arguments asks for them first |
| `/skill ...` | alias of `/skills` |

The rendered prompt is sent as a normal user message and appears in the
transcript and the session file like anything typed by hand. Because the
model sees only the rendered text, a skill can use every tool and follows the
same permissions as the rest of the session.
