# themes

An `ah` plugin that bundles colour palettes and switches between them with
`/theme <name>`. It is also the reference example for writing and publishing
an ah plugin: one `Cargo.toml`, one `src/lib.rs`, installable straight from
git.

```
ah plugin install https://github.com/kasecrab/ah-agent plugins/themes
```

Then, inside ah:

```
/theme                 list palettes, mark the active one
/theme nord            switch (persists across sessions)
/theme off             back to ah's defaults
```

Palettes: `dracula`, `nord`, `gruvbox`, `gruvbox-light`, `catppuccin-mocha`,
`catppuccin-latte`, `tokyo-night`, `solarized-dark`, `solarized-light`,
`one-dark`, `monokai`.

Config (`~/.config/ah/config.toml`):

```toml
[themes]
name = "nord"        # default when nothing was picked with /theme
background = false   # also paint bg, code_bg, input_bg and status_bg

# Add your own palette or override part of a bundled one. Keys are the
# [theme] keys from `ah docs config`.
[themes.palettes.mine]
accent = "#ff79c6"
user = "#50fa7b"
```

What it demonstrates:

| Piece | Where |
|---|---|
| manifest with hooks and a slash command | `manifest()` |
| `on_load` returning a `settings_patch` | `Hook::OnLoad` arm |
| reading plugin config through `settings_get` | `config()` |
| persisting state with `kv_get` / `kv_set` | `active()` / `Hook::SlashCommand` |
| resetting settings with the ABI's `Theme::default()` | `reset_patch()` |
| logging with `log!` | `Hook::OnLoad` arm |
