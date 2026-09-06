//! The configuration tree. Plugins change it via JSON merge patches.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub model: ModelSettings,
    pub prompt: PromptSettings,
    pub theme: Theme,
    pub layout: Layout,
    pub keys: Keys,
    pub tools: ToolSettings,
    pub plugins: PluginSettings,
    pub statusline: StatusLine,
    pub permissions: Permissions,
    pub context: ContextSettings,
    /// Plugin-private or forward-compatible keys. Preserved through merges.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSettings {
    pub id: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub reasoning: Option<Value>,
    pub provider: Option<Value>,
    /// Base URL of the OpenAI-compatible endpoint.
    pub base_url: String,
    /// Optional API key override. Prefer `OPENROUTER_API_KEY` or `ah login`.
    pub api_key: Option<String>,
    /// Named favorites, cycled with `keys.cycle_model` and managed by `/favorite`:
    /// `fast = "deepseek/deepseek-v4-flash-0731"` or
    /// `smart = { id = "anthropic/claude-sonnet-4.5", effort = "high" }`.
    pub favorites: BTreeMap<String, Favorite>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Favorite {
    Id(String),
    Full {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
}

impl Favorite {
    pub fn id(&self) -> &str {
        match self {
            Favorite::Id(id) | Favorite::Full { id, .. } => id,
        }
    }

    /// Reasoning effort, if the favorite pins one. `"off"` disables reasoning.
    pub fn effort(&self) -> Option<&str> {
        match self {
            Favorite::Id(_) => None,
            Favorite::Full { effort, .. } => effort.as_deref(),
        }
    }
}

impl ModelSettings {
    /// Current reasoning effort (`model.reasoning.effort`), if set.
    pub fn effort(&self) -> Option<&str> {
        self.reasoning
            .as_ref()
            .and_then(|r| r.get("effort"))
            .and_then(|e| e.as_str())
    }
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            id: String::from("deepseek/deepseek-v4-flash-0731"),
            max_tokens: Some(8192),
            temperature: None,
            top_p: None,
            reasoning: None,
            provider: None,
            base_url: String::from("https://openrouter.ai/api/v1"),
            api_key: None,
            favorites: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptSettings {
    /// Full system prompt. `{cwd}`, `{os}`, `{shell}`, `{date}` are substituted.
    pub system: String,
    /// Appended after `system`, meant for per-project additions.
    pub append: String,
    /// Instruction files folded into the system prompt: the first that exists
    /// in the user config dir, in each directory from the repository root to
    /// the working directory, and in `.ah/`.
    pub instructions: Vec<String>,
}

impl Default for PromptSettings {
    fn default() -> Self {
        Self {
            system: String::from(
                "You are ah, a fast coding agent running in a terminal. \
                 Working directory: {cwd}. OS: {os}. Shell: {shell}. Date: {date}.\n\
                 Use the provided tools to inspect and change files and run commands. \
                 Prefer reading before editing. Keep replies short; the user sees your \
                 text in a terminal. When a task is done, summarise what changed.",
            ),
            append: String::new(),
            instructions: vec![String::from("AGENTS.md"), String::from("CLAUDE.md")],
        }
    }
}

/// Colors: names (`red`, `bright_blue`), `#rrggbb`, or ANSI index (`123`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub fg: String,
    pub bg: String,
    pub accent: String,
    pub user: String,
    pub assistant: String,
    pub reasoning: String,
    pub tool: String,
    pub tool_output: String,
    pub error: String,
    pub dim: String,
    pub border: String,
    pub border_focus: String,
    pub status_fg: String,
    pub status_bg: String,
    pub input_fg: String,
    pub input_bg: String,
    pub selection: String,
    /// Markdown styles.
    pub heading: String,
    pub link: String,
    pub quote: String,
    pub code: String,
    pub code_bg: String,
    pub rule: String,
    /// Code highlighting.
    pub syn_keyword: String,
    pub syn_string: String,
    pub syn_comment: String,
    pub syn_number: String,
    pub syn_type: String,
    pub syn_function: String,
    /// Added and removed lines in file diffs.
    pub diff_add: String,
    pub diff_del: String,
    pub border_style: BorderStyle,
    pub user_prefix: String,
    pub assistant_prefix: String,
    pub tool_prefix: String,
    /// Prompt glyph at the left of the input line.
    pub input_prefix: String,
    pub spinner: Vec<String>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: "reset".into(),
            bg: "reset".into(),
            accent: "cyan".into(),
            user: "green".into(),
            assistant: "reset".into(),
            reasoning: "dark_gray".into(),
            tool: "yellow".into(),
            tool_output: "dark_gray".into(),
            error: "red".into(),
            dim: "dark_gray".into(),
            border: "gray".into(),
            border_focus: "gray".into(),
            status_fg: "gray".into(),
            status_bg: "reset".into(),
            input_fg: "reset".into(),
            input_bg: "reset".into(),
            selection: "blue".into(),
            heading: "white".into(),
            link: "blue".into(),
            quote: "dark_gray".into(),
            code: "white".into(),
            code_bg: "235".into(),
            rule: "dark_gray".into(),
            syn_keyword: "magenta".into(),
            syn_string: "green".into(),
            syn_comment: "dark_gray".into(),
            syn_number: "yellow".into(),
            syn_type: "cyan".into(),
            syn_function: "blue".into(),
            diff_add: "green".into(),
            diff_del: "red".into(),
            border_style: BorderStyle::Lines,
            user_prefix: "> ".into(),
            assistant_prefix: "".into(),
            tool_prefix: "⚙ ".into(),
            input_prefix: "› ".into(),
            spinner: ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BorderStyle {
    None,
    /// Horizontal rules above and below the input, no sides.
    #[default]
    Lines,
    Plain,
    Rounded,
    Double,
    Thick,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    /// Input box height in rows (grows up to `input_max_height` with content).
    pub input_height: u16,
    pub input_max_height: u16,
    /// 0 = full width.
    pub transcript_max_width: u16,
    pub show_status: bool,
    /// Show tool output blocks expanded by default.
    pub show_tool_output: bool,
    /// Max lines of tool output shown when expanded.
    pub tool_output_lines: u16,
    pub show_reasoning: bool,
    pub wrap: bool,
    /// Redraw throttle while streaming, in milliseconds. 0 = redraw on every delta.
    pub stream_redraw_ms: u64,
    pub spinner_ms: u64,
    /// Rows scrolled per wheel/arrow step.
    pub scroll_step: u16,
    /// Capture the mouse: wheel scrolls, drag selects and copies (OSC 52).
    /// Off, the terminal keeps the mouse and wheel ticks arrive as Up/Down.
    pub mouse: bool,
    /// Push kitty keyboard-protocol flags (needed for Shift-Enter).
    pub kitty_keyboard: bool,
    /// Pastes with more lines than this collapse to a `[Pasted N lines]` chip.
    pub paste_collapse_lines: usize,
    /// Render assistant messages as markdown.
    pub markdown: bool,
    /// Highlight fenced code blocks.
    pub code_highlight: bool,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            input_height: 1,
            input_max_height: 10,
            transcript_max_width: 0,
            show_status: true,
            show_tool_output: false,
            tool_output_lines: 20,
            show_reasoning: true,
            wrap: true,
            stream_redraw_ms: 33,
            spinner_ms: 100,
            scroll_step: 3,
            mouse: true,
            kitty_keyboard: true,
            paste_collapse_lines: 3,
            markdown: true,
            code_highlight: true,
        }
    }
}

/// Key bindings, as crossterm-ish strings: `enter`, `ctrl-c`, `alt-enter`,
/// `shift-tab`, `pageup`, `f2`, `ctrl-shift-x`. Multiple bindings: `["ctrl-c", "esc"]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Keys {
    pub submit: Vec<String>,
    pub newline: Vec<String>,
    pub cancel: Vec<String>,
    pub quit: Vec<String>,
    pub scroll_up: Vec<String>,
    pub scroll_down: Vec<String>,
    pub page_up: Vec<String>,
    pub page_down: Vec<String>,
    pub scroll_top: Vec<String>,
    pub scroll_bottom: Vec<String>,
    pub clear: Vec<String>,
    pub toggle_tools: Vec<String>,
    pub toggle_reasoning: Vec<String>,
    /// Switch to the next favorite model.
    pub cycle_model: Vec<String>,
    pub history_prev: Vec<String>,
    pub history_next: Vec<String>,
    pub delete_word: Vec<String>,
    pub delete_line: Vec<String>,
    pub line_start: Vec<String>,
    pub line_end: Vec<String>,
}

impl Default for Keys {
    fn default() -> Self {
        let v = |xs: &[&str]| xs.iter().map(|s| String::from(*s)).collect::<Vec<_>>();
        Self {
            submit: v(&["enter"]),
            newline: v(&["shift-enter", "alt-enter", "ctrl-j"]),
            cancel: v(&["esc"]),
            quit: v(&["ctrl-c", "ctrl-d"]),
            scroll_up: v(&["ctrl-up", "alt-k"]),
            scroll_down: v(&["ctrl-down", "alt-j"]),
            page_up: v(&["pageup"]),
            page_down: v(&["pagedown"]),
            scroll_top: v(&["ctrl-home"]),
            scroll_bottom: v(&["ctrl-end"]),
            clear: v(&["ctrl-l"]),
            toggle_tools: v(&["ctrl-t"]),
            toggle_reasoning: v(&["ctrl-r"]),
            cycle_model: v(&["shift-tab"]),
            history_prev: v(&["up", "ctrl-p"]),
            history_next: v(&["down", "ctrl-n"]),
            delete_word: v(&["ctrl-w"]),
            delete_line: v(&["ctrl-u"]),
            line_start: v(&["ctrl-a", "home"]),
            line_end: v(&["ctrl-e", "end"]),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolSettings {
    /// Built-in tools to expose. Plugin tools are always added unless disabled.
    pub enabled: Vec<String>,
    pub disabled: Vec<String>,
    /// Tool output larger than this is truncated head+tail before reaching the model.
    pub max_output_bytes: usize,
    pub bash_timeout_ms: u64,
    /// Shell used by the `bash` tool. Empty = `$SHELL` or `sh`.
    pub shell: String,
    pub read_default_limit: usize,
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            enabled: ["bash", "read_file", "write_file", "edit_file"]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
            disabled: Vec::new(),
            max_output_bytes: 32 * 1024,
            bash_timeout_ms: 120_000,
            shell: String::new(),
            read_default_limit: 2000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginSettings {
    /// Extra plugin files or directories, in addition to the standard locations.
    pub paths: Vec<String>,
    pub disabled: Vec<String>,
    /// Interpreter fuel budget per hook call. Roughly one unit per wasm instruction.
    pub fuel_per_call: u64,
    /// Max linear memory per plugin in bytes.
    pub max_memory_bytes: u64,
    pub enabled: bool,
}

impl Default for PluginSettings {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            disabled: Vec::new(),
            fuel_per_call: 50_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusLine {
    /// Template. Placeholders: `{model} {favorite} {effort} {tokens_in} {tokens_out}
    /// {cost} {context} {cwd} {git} {plugins} {state} {session}`.
    pub format: String,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self {
            format: String::from(
                " {favorite} {model} {effort} │ ↑{tokens_in} ↓{tokens_out} ${cost} │ {context} │ {cwd} {git}",
            ),
        }
    }
}

/// Context window accounting and compaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextSettings {
    /// Summarise the conversation automatically once it fills `compact_at`
    /// percent of the model's context window.
    pub auto_compact: bool,
    /// Percent of the window that triggers auto compaction.
    pub compact_at: u8,
    /// Window size in tokens; 0 means "from the model catalogue".
    pub window: u64,
    /// Max tokens the summary may use.
    pub summary_max_tokens: u32,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            auto_compact: true,
            compact_at: 90,
            window: 0,
            summary_max_tokens: 4096,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    /// `auto` runs every tool without asking. `ask` prompts for tools in `ask_for`.
    pub mode: PermissionMode,
    pub ask_for: Vec<String>,
    /// Shell commands refused in every mode. A rule matches a command segment
    /// that equals it or starts with it followed by a space; a trailing `*`
    /// matches any continuation. Setting this replaces the built-in list.
    pub deny: Vec<String>,
}

/// Commands no mode runs unless the user edits `permissions.deny`.
pub const DEFAULT_DENY: &[&str] = &[
    "rm -rf /",
    "rm -rf ~",
    "rm -rf ~/",
    "rm -rf .",
    "rm -rf ..",
    "rm -fr /",
    "rm -fr ~",
    "rm -rf --no-preserve-root*",
    "rm -rf $HOME",
    "rm -rf $HOME/",
    "git reset --hard",
    "git push --force",
    "git push -f",
    "git clean -f*",
    "git clean -x*",
    "git clean -d*",
    "git checkout -- .",
    "git checkout .",
    "git restore .",
    "git branch -D",
    "git stash drop",
    "git stash clear",
    "git filter-branch*",
    "chmod -R 777 /",
    "mkfs*",
    "dd if=*",
    "shutdown*",
    "reboot",
    "poweroff",
    ":(){ :|:& };:",
];

impl Default for Permissions {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Auto,
            ask_for: ["bash", "write_file", "edit_file"]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
            deny: DEFAULT_DENY.iter().map(|s| String::from(*s)).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Auto,
    Ask,
}

/// RFC 7386 JSON merge patch. `null` removes a key; objects merge recursively;
/// anything else replaces.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(pm) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let tm = target.as_object_mut().expect("object");
            for (k, pv) in pm {
                if pv.is_null() {
                    tm.remove(k);
                } else {
                    let slot = tm.entry(k.clone()).or_insert(Value::Null);
                    merge_patch(slot, pv);
                }
            }
        }
        other => *target = other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_patch_semantics() {
        let mut t = json!({"a": {"b": 1, "c": 2}, "d": [1,2]});
        merge_patch(
            &mut t,
            &json!({"a": {"b": null, "e": 3}, "d": [9], "f": "x"}),
        );
        assert_eq!(t, json!({"a": {"c": 2, "e": 3}, "d": [9], "f": "x"}));
    }

    #[test]
    fn settings_roundtrip_and_extra() {
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        merge_patch(
            &mut v,
            &json!({"theme": {"accent": "magenta"}, "guard": {"deny": ["rm -rf"]}}),
        );
        let s: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(s.theme.accent, "magenta");
        assert_eq!(s.extra["guard"]["deny"][0], "rm -rf");
        assert_eq!(s.layout.input_height, 1);
    }

    #[test]
    fn favorite_accepts_string_or_table() {
        let v = json!({"model": {"favorites": {
            "fast": "deepseek/deepseek-v4-flash-0731",
            "smart": {"id": "anthropic/claude-sonnet-4.5", "effort": "high"}
        }}});
        let s: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(
            s.model.favorites["fast"].id(),
            "deepseek/deepseek-v4-flash-0731"
        );
        assert_eq!(s.model.favorites["fast"].effort(), None);
        assert_eq!(s.model.favorites["smart"].effort(), Some("high"));
        let m = ModelSettings {
            reasoning: Some(json!({"effort": "low"})),
            ..Default::default()
        };
        assert_eq!(m.effort(), Some("low"));
    }
}
