# Project instructions

ah folds instruction files into the system prompt so the model knows a
project's conventions without being told each session.

## Which files

`prompt.instructions` lists candidate file names in priority order. The
default is:

```toml
[prompt]
instructions = ["AGENTS.md", "CLAUDE.md"]
```

For each directory below, the first name in the list that exists is used and
the rest are ignored:

1. the user config dir (`~/.config/ah/AGENTS.md`) for rules that apply everywhere
2. every directory from the repository root (the nearest ancestor containing
   `.git`) down to the working directory, root first
3. `.ah/` inside the working directory

Files are read when a turn starts, so edits take effect on the next message.
Nothing is cached.

## How they reach the model

Each file is appended to the system prompt as its own section:

```
# Instructions from /home/me/proj/AGENTS.md

<file contents>
```

The prompt itself comes from `prompt.system` (with `{cwd}`, `{os}`, `{shell}`
and `{date}` substituted) followed by `prompt.append`. Plugins with the
`system_prompt` hook see the assembled text last and may rewrite it.

## /init

`/init` in the TUI asks the model to study the project (build commands, tests,
layout, conventions) and write `AGENTS.md` in the working directory, or update
it when one exists. It is a normal turn: the model uses its tools and the file
lands on disk like any other edit.

## Writing a good AGENTS.md

Keep it short and factual: how to build and test, where code lives, naming
rules, anything the model gets wrong without being told. Avoid restating what
the code makes obvious. Instructions for a subdirectory belong in an
`AGENTS.md` inside it; those are only loaded when the working directory is at
or below that directory.
