//! Colour themes for ah.
//!
//! `/theme <name>` switches the `[theme]` settings to one of the bundled
//! palettes and remembers the choice in the plugin key/value store, so it is
//! back on the next start. `[themes] name = "nord"` in the config file is the
//! default when nothing was picked yet, `[themes.palettes.<name>]` adds or
//! adjusts palettes, and `[themes] background = true` also paints the
//! background keys (`bg`, `code_bg`, `input_bg`), which are skipped by default
//! so the terminal's own background stays.
//!
//! The plugin keeps the `[theme]` table it saw at load time and merges every
//! palette over that copy, so `/theme off` returns exactly to the colours from
//! the config file.

use ah_plugin_sdk::prelude::*;
use serde_json::Map;

/// Keys only applied when `[themes] background = true`.
const BACKGROUND_KEYS: &[&str] = &["bg", "code_bg", "input_bg"];

/// Bundled palettes as `[theme]` key/value pairs.
const PALETTES: &[(&str, &[(&str, &str)])] = &[
    ("dracula", &[
        ("bg", "#282a36"), ("code_bg", "#21222c"), ("input_bg", "#282a36"),
        ("fg", "#f8f8f2"), ("accent", "#bd93f9"), ("user", "#50fa7b"), ("assistant", "#f8f8f2"),
        ("reasoning", "#6272a4"), ("tool", "#ffb86c"), ("tool_output", "#6272a4"), ("error", "#ff5555"),
        ("dim", "#6272a4"), ("border", "#44475a"), ("border_focus", "#bd93f9"),
        ("status_fg", "#282a36"), ("status_bg", "#bd93f9"), ("input_fg", "#f8f8f2"), ("selection", "#44475a"),
        ("heading", "#ff79c6"), ("link", "#8be9fd"), ("quote", "#6272a4"), ("code", "#f8f8f2"), ("rule", "#44475a"),
        ("syn_keyword", "#ff79c6"), ("syn_string", "#f1fa8c"), ("syn_comment", "#6272a4"), ("syn_number", "#bd93f9"),
        ("syn_type", "#8be9fd"), ("syn_function", "#50fa7b"), ("syn_builtin", "#8be9fd"), ("syn_attr", "#8be9fd"),
        ("diff_add", "#50fa7b"), ("diff_del", "#ff5555"),
    ]),
    ("nord", &[
        ("bg", "#2e3440"), ("code_bg", "#3b4252"), ("input_bg", "#2e3440"),
        ("fg", "#d8dee9"), ("accent", "#88c0d0"), ("user", "#a3be8c"), ("assistant", "#d8dee9"),
        ("reasoning", "#616e88"), ("tool", "#ebcb8b"), ("tool_output", "#616e88"), ("error", "#bf616a"),
        ("dim", "#616e88"), ("border", "#434c5e"), ("border_focus", "#88c0d0"),
        ("status_fg", "#2e3440"), ("status_bg", "#88c0d0"), ("input_fg", "#d8dee9"), ("selection", "#434c5e"),
        ("heading", "#eceff4"), ("link", "#81a1c1"), ("quote", "#616e88"), ("code", "#d8dee9"), ("rule", "#434c5e"),
        ("syn_keyword", "#81a1c1"), ("syn_string", "#a3be8c"), ("syn_comment", "#616e88"), ("syn_number", "#b48ead"),
        ("syn_type", "#8fbcbb"), ("syn_function", "#88c0d0"), ("syn_builtin", "#8fbcbb"), ("syn_attr", "#8fbcbb"),
        ("diff_add", "#a3be8c"), ("diff_del", "#bf616a"),
    ]),
    ("gruvbox", &[
        ("bg", "#282828"), ("code_bg", "#1d2021"), ("input_bg", "#282828"),
        ("fg", "#ebdbb2"), ("accent", "#fabd2f"), ("user", "#b8bb26"), ("assistant", "#ebdbb2"),
        ("reasoning", "#928374"), ("tool", "#fe8019"), ("tool_output", "#928374"), ("error", "#fb4934"),
        ("dim", "#928374"), ("border", "#504945"), ("border_focus", "#fabd2f"),
        ("status_fg", "#282828"), ("status_bg", "#fabd2f"), ("input_fg", "#ebdbb2"), ("selection", "#504945"),
        ("heading", "#fbf1c7"), ("link", "#83a598"), ("quote", "#928374"), ("code", "#ebdbb2"), ("rule", "#504945"),
        ("syn_keyword", "#fb4934"), ("syn_string", "#b8bb26"), ("syn_comment", "#928374"), ("syn_number", "#d3869b"),
        ("syn_type", "#fabd2f"), ("syn_function", "#8ec07c"), ("syn_builtin", "#fe8019"), ("syn_attr", "#8ec07c"),
        ("diff_add", "#b8bb26"), ("diff_del", "#fb4934"),
    ]),
    ("gruvbox-light", &[
        ("bg", "#fbf1c7"), ("code_bg", "#f9f5d7"), ("input_bg", "#fbf1c7"),
        ("fg", "#3c3836"), ("accent", "#b57614"), ("user", "#79740e"), ("assistant", "#3c3836"),
        ("reasoning", "#928374"), ("tool", "#af3a03"), ("tool_output", "#928374"), ("error", "#9d0006"),
        ("dim", "#928374"), ("border", "#d5c4a1"), ("border_focus", "#b57614"),
        ("status_fg", "#fbf1c7"), ("status_bg", "#b57614"), ("input_fg", "#3c3836"), ("selection", "#d5c4a1"),
        ("heading", "#282828"), ("link", "#076678"), ("quote", "#928374"), ("code", "#3c3836"), ("rule", "#d5c4a1"),
        ("syn_keyword", "#9d0006"), ("syn_string", "#79740e"), ("syn_comment", "#928374"), ("syn_number", "#8f3f71"),
        ("syn_type", "#b57614"), ("syn_function", "#427b58"), ("syn_builtin", "#af3a03"), ("syn_attr", "#427b58"),
        ("diff_add", "#79740e"), ("diff_del", "#9d0006"),
    ]),
    ("catppuccin-mocha", &[
        ("bg", "#1e1e2e"), ("code_bg", "#181825"), ("input_bg", "#1e1e2e"),
        ("fg", "#cdd6f4"), ("accent", "#cba6f7"), ("user", "#a6e3a1"), ("assistant", "#cdd6f4"),
        ("reasoning", "#7f849c"), ("tool", "#fab387"), ("tool_output", "#7f849c"), ("error", "#f38ba8"),
        ("dim", "#6c7086"), ("border", "#45475a"), ("border_focus", "#cba6f7"),
        ("status_fg", "#1e1e2e"), ("status_bg", "#cba6f7"), ("input_fg", "#cdd6f4"), ("selection", "#45475a"),
        ("heading", "#b4befe"), ("link", "#89b4fa"), ("quote", "#7f849c"), ("code", "#cdd6f4"), ("rule", "#45475a"),
        ("syn_keyword", "#cba6f7"), ("syn_string", "#a6e3a1"), ("syn_comment", "#6c7086"), ("syn_number", "#fab387"),
        ("syn_type", "#f9e2af"), ("syn_function", "#89b4fa"), ("syn_builtin", "#89dceb"), ("syn_attr", "#89b4fa"),
        ("diff_add", "#a6e3a1"), ("diff_del", "#f38ba8"),
    ]),
    ("catppuccin-latte", &[
        ("bg", "#eff1f5"), ("code_bg", "#e6e9ef"), ("input_bg", "#eff1f5"),
        ("fg", "#4c4f69"), ("accent", "#8839ef"), ("user", "#40a02b"), ("assistant", "#4c4f69"),
        ("reasoning", "#8c8fa1"), ("tool", "#fe640b"), ("tool_output", "#8c8fa1"), ("error", "#d20f39"),
        ("dim", "#9ca0b0"), ("border", "#bcc0cc"), ("border_focus", "#8839ef"),
        ("status_fg", "#eff1f5"), ("status_bg", "#8839ef"), ("input_fg", "#4c4f69"), ("selection", "#ccd0da"),
        ("heading", "#7287fd"), ("link", "#1e66f5"), ("quote", "#8c8fa1"), ("code", "#4c4f69"), ("rule", "#bcc0cc"),
        ("syn_keyword", "#8839ef"), ("syn_string", "#40a02b"), ("syn_comment", "#9ca0b0"), ("syn_number", "#fe640b"),
        ("syn_type", "#df8e1d"), ("syn_function", "#1e66f5"), ("syn_builtin", "#04a5e5"), ("syn_attr", "#1e66f5"),
        ("diff_add", "#40a02b"), ("diff_del", "#d20f39"),
    ]),
    ("tokyo-night", &[
        ("bg", "#1a1b26"), ("code_bg", "#16161e"), ("input_bg", "#1a1b26"),
        ("fg", "#c0caf5"), ("accent", "#7aa2f7"), ("user", "#9ece6a"), ("assistant", "#c0caf5"),
        ("reasoning", "#565f89"), ("tool", "#ff9e64"), ("tool_output", "#565f89"), ("error", "#f7768e"),
        ("dim", "#565f89"), ("border", "#414868"), ("border_focus", "#7aa2f7"),
        ("status_fg", "#1a1b26"), ("status_bg", "#7aa2f7"), ("input_fg", "#c0caf5"), ("selection", "#33467c"),
        ("heading", "#c0caf5"), ("link", "#7dcfff"), ("quote", "#565f89"), ("code", "#c0caf5"), ("rule", "#414868"),
        ("syn_keyword", "#bb9af7"), ("syn_string", "#9ece6a"), ("syn_comment", "#565f89"), ("syn_number", "#ff9e64"),
        ("syn_type", "#2ac3de"), ("syn_function", "#7aa2f7"), ("syn_builtin", "#7dcfff"), ("syn_attr", "#7aa2f7"),
        ("diff_add", "#9ece6a"), ("diff_del", "#f7768e"),
    ]),
    ("solarized-dark", &[
        ("bg", "#002b36"), ("code_bg", "#073642"), ("input_bg", "#002b36"),
        ("fg", "#839496"), ("accent", "#268bd2"), ("user", "#859900"), ("assistant", "#839496"),
        ("reasoning", "#586e75"), ("tool", "#b58900"), ("tool_output", "#586e75"), ("error", "#dc322f"),
        ("dim", "#586e75"), ("border", "#073642"), ("border_focus", "#268bd2"),
        ("status_fg", "#002b36"), ("status_bg", "#268bd2"), ("input_fg", "#839496"), ("selection", "#073642"),
        ("heading", "#93a1a1"), ("link", "#268bd2"), ("quote", "#586e75"), ("code", "#839496"), ("rule", "#073642"),
        ("syn_keyword", "#859900"), ("syn_string", "#2aa198"), ("syn_comment", "#586e75"), ("syn_number", "#d33682"),
        ("syn_type", "#b58900"), ("syn_function", "#268bd2"), ("syn_builtin", "#cb4b16"), ("syn_attr", "#268bd2"),
        ("diff_add", "#859900"), ("diff_del", "#dc322f"),
    ]),
    ("solarized-light", &[
        ("bg", "#fdf6e3"), ("code_bg", "#eee8d5"), ("input_bg", "#fdf6e3"),
        ("fg", "#657b83"), ("accent", "#268bd2"), ("user", "#859900"), ("assistant", "#657b83"),
        ("reasoning", "#93a1a1"), ("tool", "#b58900"), ("tool_output", "#93a1a1"), ("error", "#dc322f"),
        ("dim", "#93a1a1"), ("border", "#eee8d5"), ("border_focus", "#268bd2"),
        ("status_fg", "#fdf6e3"), ("status_bg", "#268bd2"), ("input_fg", "#657b83"), ("selection", "#eee8d5"),
        ("heading", "#586e75"), ("link", "#268bd2"), ("quote", "#93a1a1"), ("code", "#657b83"), ("rule", "#eee8d5"),
        ("syn_keyword", "#859900"), ("syn_string", "#2aa198"), ("syn_comment", "#93a1a1"), ("syn_number", "#d33682"),
        ("syn_type", "#b58900"), ("syn_function", "#268bd2"), ("syn_builtin", "#cb4b16"), ("syn_attr", "#268bd2"),
        ("diff_add", "#859900"), ("diff_del", "#dc322f"),
    ]),
    ("one-dark", &[
        ("bg", "#282c34"), ("code_bg", "#21252b"), ("input_bg", "#282c34"),
        ("fg", "#abb2bf"), ("accent", "#61afef"), ("user", "#98c379"), ("assistant", "#abb2bf"),
        ("reasoning", "#5c6370"), ("tool", "#d19a66"), ("tool_output", "#5c6370"), ("error", "#e06c75"),
        ("dim", "#5c6370"), ("border", "#3e4451"), ("border_focus", "#61afef"),
        ("status_fg", "#282c34"), ("status_bg", "#61afef"), ("input_fg", "#abb2bf"), ("selection", "#3e4451"),
        ("heading", "#dcdfe4"), ("link", "#61afef"), ("quote", "#5c6370"), ("code", "#abb2bf"), ("rule", "#3e4451"),
        ("syn_keyword", "#c678dd"), ("syn_string", "#98c379"), ("syn_comment", "#5c6370"), ("syn_number", "#d19a66"),
        ("syn_type", "#e5c07b"), ("syn_function", "#61afef"), ("syn_builtin", "#56b6c2"), ("syn_attr", "#e06c75"),
        ("diff_add", "#98c379"), ("diff_del", "#e06c75"),
    ]),
    ("monokai", &[
        ("bg", "#272822"), ("code_bg", "#1e1f1c"), ("input_bg", "#272822"),
        ("fg", "#f8f8f2"), ("accent", "#66d9ef"), ("user", "#a6e22e"), ("assistant", "#f8f8f2"),
        ("reasoning", "#75715e"), ("tool", "#fd971f"), ("tool_output", "#75715e"), ("error", "#f92672"),
        ("dim", "#75715e"), ("border", "#49483e"), ("border_focus", "#66d9ef"),
        ("status_fg", "#272822"), ("status_bg", "#66d9ef"), ("input_fg", "#f8f8f2"), ("selection", "#49483e"),
        ("heading", "#f8f8f2"), ("link", "#66d9ef"), ("quote", "#75715e"), ("code", "#f8f8f2"), ("rule", "#49483e"),
        ("syn_keyword", "#f92672"), ("syn_string", "#e6db74"), ("syn_comment", "#75715e"), ("syn_number", "#ae81ff"),
        ("syn_type", "#66d9ef"), ("syn_function", "#a6e22e"), ("syn_builtin", "#66d9ef"), ("syn_attr", "#a6e22e"),
        ("diff_add", "#a6e22e"), ("diff_del", "#f92672"),
    ]),
];

/// Key under which the `/theme` choice is persisted.
const KV_ACTIVE: &str = "active";
/// Key under which the `[theme]` table seen at load time is persisted.
const KV_BASE: &str = "base";

fn manifest() -> Manifest {
    Manifest {
        name: "themes".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        description: "Colour palettes: /theme <name>".into(),
        hooks: vec![Hook::OnLoad, Hook::SlashCommand],
        commands: vec![SlashCommandSpec {
            name: "theme".into(),
            description: "List or switch colour themes".into(),
            usage: "/theme [name|off]".into(),
        }],
        ..Default::default()
    }
}

/// `[themes]` from the merged config.
struct Config {
    name: Option<String>,
    background: bool,
    palettes: Map<String, Value>,
}

fn config() -> Config {
    let table = settings_get("/themes");
    Config {
        name: table.get("name").and_then(Value::as_str).map(str::to_owned),
        background: table.get("background").and_then(Value::as_bool).unwrap_or(false),
        palettes: table.get("palettes").and_then(Value::as_object).cloned().unwrap_or_default(),
    }
}

/// Palette names, bundled first, then the user's, without duplicates.
fn names(cfg: &Config) -> Vec<String> {
    let mut out: Vec<String> = PALETTES.iter().map(|(n, _)| n.to_string()).collect();
    for k in cfg.palettes.keys() {
        if !out.contains(k) {
            out.push(k.clone());
        }
    }
    out
}

/// The `[theme]` values a palette sets. A user palette may name a bundled one
/// in `base` and override part of it.
fn palette(cfg: &Config, name: &str) -> Option<Map<String, Value>> {
    let mut out = Map::new();
    if let Some(custom) = cfg.palettes.get(name).and_then(Value::as_object) {
        if let Some(base) = custom.get("base").and_then(Value::as_str) {
            out = palette(cfg, base)?;
        }
        for (k, v) in custom {
            if k != "base" {
                out.insert(k.clone(), v.clone());
            }
        }
        return Some(out);
    }
    let (_, pairs) = PALETTES.iter().find(|(n, _)| *n == name)?;
    for (k, v) in pairs.iter() {
        out.insert(k.to_string(), Value::String(v.to_string()));
    }
    Some(out)
}

/// The `[theme]` table from before this plugin touched it.
fn base_theme() -> Map<String, Value> {
    kv_get(KV_BASE)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Settings patch that applies `name` over the base theme, or just the base
/// theme when `name` is `None`.
fn patch_for(cfg: &Config, name: Option<&str>) -> Result<Value, String> {
    let mut theme = base_theme();
    if let Some(n) = name {
        let pal = palette(cfg, n).ok_or_else(|| format!("unknown theme `{n}`"))?;
        for (k, v) in pal {
            if cfg.background || !BACKGROUND_KEYS.contains(&k.as_str()) {
                theme.insert(k, v);
            }
        }
    }
    Ok(json!({ "theme": theme }))
}

fn active() -> Option<String> {
    kv_get(KV_ACTIVE).filter(|s| !s.is_empty())
}

fn handle(hook: Hook, input: Value) -> Result<Value, String> {
    match hook {
        Hook::OnLoad => {
            let inp: OnLoadIn = serde_json::from_value(input).map_err(|e| e.to_string())?;
            let base = serde_json::to_string(&inp.settings.theme).map_err(|e| e.to_string())?;
            kv_set(KV_BASE, Some(&base));
            let cfg = config();
            let Some(name) = active().or_else(|| cfg.name.clone()) else {
                return Ok(Value::Null);
            };
            match patch_for(&cfg, Some(&name)) {
                Ok(patch) => {
                    ah_plugin_sdk::log!(LogLevel::Info, "theme {name}");
                    Ok(json!({ "settings_patch": patch }))
                }
                Err(e) => {
                    ah_plugin_sdk::log!(LogLevel::Warn, "{e}");
                    Ok(Value::Null)
                }
            }
        }
        Hook::SlashCommand => {
            let inp: SlashCommandIn = serde_json::from_value(input).map_err(|e| e.to_string())?;
            let cfg = config();
            let arg = inp.args.trim();
            if arg.is_empty() || arg == "list" {
                let current = active().or_else(|| cfg.name.clone());
                let list = names(&cfg)
                    .iter()
                    .map(|n| {
                        let mark = if current.as_deref() == Some(n) { "*" } else { " " };
                        format!("{mark} {n}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return Ok(json!({ "message": format!("themes (/theme <name>, /theme off):\n{list}") }));
            }
            if arg == "off" || arg == "default" {
                kv_set(KV_ACTIVE, None);
                return Ok(json!({
                    "message": "theme: off",
                    "settings_patch": patch_for(&cfg, None)?,
                }));
            }
            let patch = match patch_for(&cfg, Some(arg)) {
                Ok(p) => p,
                Err(e) => return Ok(json!({ "message": format!("{e}; /theme lists the names") })),
            };
            kv_set(KV_ACTIVE, Some(arg));
            Ok(json!({ "message": format!("theme: {arg}"), "settings_patch": patch }))
        }
        _ => Ok(Value::Null),
    }
}

ah_plugin_sdk::plugin!(manifest, handle);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_palette_sets_the_same_keys() {
        let keys = |name: &str| {
            let mut k: Vec<&str> = PALETTES
                .iter()
                .find(|(n, _)| *n == name)
                .unwrap()
                .1
                .iter()
                .map(|(k, _)| *k)
                .collect();
            k.sort_unstable();
            k
        };
        let first = keys(PALETTES[0].0);
        for (name, pairs) in PALETTES {
            assert_eq!(keys(name), first, "{name} differs");
            for (_, v) in pairs.iter() {
                assert!(v.len() == 7 && v.starts_with('#'), "{name}: bad colour {v}");
            }
        }
    }

    #[test]
    fn palette_keys_exist_in_theme() {
        let theme = serde_json::to_value(Theme::default()).unwrap();
        for (name, pairs) in PALETTES {
            for (k, _) in pairs.iter() {
                assert!(theme.get(*k).is_some(), "{name}: `{k}` is not a [theme] key");
            }
        }
    }

    #[test]
    fn custom_palette_extends_base() {
        let cfg = Config {
            name: None,
            background: false,
            palettes: json!({"mine": {"base": "nord", "accent": "#ff0000"}})
                .as_object()
                .cloned()
                .unwrap(),
        };
        let p = palette(&cfg, "mine").unwrap();
        assert_eq!(p["accent"], "#ff0000");
        assert_eq!(p["user"], "#a3be8c");
        assert!(p.get("base").is_none());
        assert!(palette(&cfg, "nope").is_none());
        assert_eq!(names(&cfg).last().map(String::as_str), Some("mine"));
    }

    #[test]
    fn background_keys_are_opt_in() {
        let cfg = Config { name: None, background: false, palettes: Map::new() };
        let p = patch_for(&cfg, Some("dracula")).unwrap();
        assert!(p["theme"].get("bg").is_none());
        assert_eq!(p["theme"]["accent"], "#bd93f9");
        let cfg = Config { name: None, background: true, palettes: Map::new() };
        let p = patch_for(&cfg, Some("dracula")).unwrap();
        assert_eq!(p["theme"]["bg"], "#282a36");
        assert!(patch_for(&cfg, Some("nope")).is_err());
    }
}
