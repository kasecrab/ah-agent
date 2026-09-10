//! The configuration tree. Plugins change it via JSON merge patches.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
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
    pub agents: AgentSettings,
    pub voice: VoiceSettings,
    pub images: ImageSettings,
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
    /// Tell the model that `ah docs <topic>` exists, so it can look up ah's
    /// own settings, keys and plugin API when asked instead of guessing.
    pub docs_hint: bool,
}

impl Default for PromptSettings {
    fn default() -> Self {
        Self {
            system: String::from(
                "You are ah, a fast coding agent running in a terminal. \
                 Working directory: {cwd}. OS: {os}. Shell: {shell}. Date: {date}.\n\
                 Use the provided tools to inspect and change files and run commands. \
                 Prefer reading before editing. Keep replies short; the user sees your \
                 text in a terminal. When a task is done, summarise what changed.\n\
                 Work that takes more than a couple of steps goes in the plan tool first: \
                 set the tasks, mark one started before you work on it and done as soon as \
                 it is finished, and give a task `needs` when it cannot start until another \
                 one is done. Skip it for a single edit or question.\n\
                 When the request leaves a choice open that would send the work one way \
                 or the other, ask with the ask_user tool before doing it, once, with the \
                 options you would otherwise pick between. Anything you can settle from \
                 the code or a sensible default, settle yourself and say what you assumed.",
            ),
            append: String::new(),
            instructions: vec![String::from("AGENTS.md"), String::from("CLAUDE.md")],
            docs_hint: true,
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
    /// Code highlighting. A colour may be followed by `dim`, `bold`, `italic`
    /// or `underline`: `"cyan dim"`.
    pub syn_keyword: String,
    pub syn_string: String,
    pub syn_comment: String,
    pub syn_number: String,
    pub syn_type: String,
    pub syn_function: String,
    pub syn_builtin: String,
    /// Keys in JSON, TOML and YAML.
    pub syn_attr: String,
    /// Background of the running-jobs chip above the input.
    pub job: String,
    /// Background of the running-agents chip beside it.
    pub agent: String,
    /// Added and removed lines in file diffs.
    pub diff_add: String,
    pub diff_del: String,
    pub border_style: BorderStyle,
    pub user_prefix: String,
    pub assistant_prefix: String,
    pub tool_prefix: String,
    /// Prompt glyph at the left of the input line.
    pub input_prefix: String,
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
            code: "reset".into(),
            code_bg: "reset".into(),
            rule: "dark_gray".into(),
            syn_keyword: "blue".into(),
            syn_string: "red".into(),
            syn_comment: "green".into(),
            syn_number: "green".into(),
            syn_type: "cyan dim".into(),
            syn_function: "yellow".into(),
            syn_builtin: "cyan".into(),
            syn_attr: "cyan".into(),
            job: "green".into(),
            agent: "magenta".into(),
            diff_add: "green".into(),
            diff_del: "red".into(),
            border_style: BorderStyle::Lines,
            user_prefix: "> ".into(),
            assistant_prefix: "".into(),
            tool_prefix: "⚙ ".into(),
            input_prefix: "› ".into(),
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
    /// Redraw period for the working line while a turn runs, in
    /// milliseconds. 0 leaves the word still, for terminals or people that
    /// would rather have no animation.
    pub animation_ms: u64,
    /// Rows scrolled per wheel/arrow step.
    pub scroll_step: u16,
    /// Capture the mouse: wheel scrolls, drag selects and copies (OSC 52).
    /// Off, the terminal keeps the mouse and wheel ticks arrive as Up/Down.
    pub mouse: bool,
    /// Push kitty keyboard-protocol flags (needed for Shift-Enter).
    pub kitty_keyboard: bool,
    /// Pastes with more lines than this collapse to a `[Pasted N lines]` chip.
    pub paste_collapse_lines: usize,
    /// Messages that can wait while a turn runs; 0 disables queueing.
    pub queue_max: usize,
    /// Show the model's input modalities (`TIF→T`) in pickers and the status bar.
    pub show_modalities: bool,
    /// Shell command that prints the clipboard image as PNG; empty detects
    /// `wl-paste`, `xclip` or `pngpaste`.
    pub image_paste_cmd: String,
    /// Render assistant messages as markdown.
    pub markdown: bool,
    /// Highlight fenced code blocks.
    pub code_highlight: bool,
    /// Show the plan summary above the input while tasks are open.
    pub show_plan: bool,
    /// Terminal window title, so several ah windows can be told apart.
    /// `{task}` is the session name, or the first message of the session, or
    /// the working directory. `{cwd}`, `{model}` and `{session}` also work.
    /// Empty leaves the title alone.
    pub window_title: String,
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
            animation_ms: 32,
            scroll_step: 3,
            mouse: true,
            kitty_keyboard: true,
            paste_collapse_lines: 3,
            queue_max: 5,
            show_modalities: true,
            image_paste_cmd: String::new(),
            markdown: true,
            code_highlight: true,
            show_plan: true,
            window_title: String::from("{task} · ah"),
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
    /// Clear the input; `yank` brings it back.
    pub delete_line: Vec<String>,
    /// Put back what the last delete took.
    pub yank: Vec<String>,
    pub line_start: Vec<String>,
    pub line_end: Vec<String>,
    /// Attach the image on the clipboard to the next message.
    pub paste_image: Vec<String>,
    /// Open a picture the model drew in whatever the desktop uses.
    pub open_image: Vec<String>,
    /// Show or hide the plan line above the input.
    pub toggle_plan: Vec<String>,
    /// Arm and disarm dictation.
    pub voice: Vec<String>,
    /// Held to listen while dictation is armed.
    pub talk: Vec<String>,
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
            delete_word: v(&["ctrl-w", "ctrl-backspace", "ctrl-h", "alt-backspace"]),
            delete_line: v(&["ctrl-u"]),
            yank: v(&["ctrl-y"]),
            line_start: v(&["ctrl-a", "home"]),
            line_end: v(&["ctrl-e", "end"]),
            paste_image: v(&["ctrl-v"]),
            open_image: v(&["ctrl-o"]),
            toggle_plan: v(&["alt-p"]),
            voice: v(&["alt-v"]),
            talk: v(&["space"]),
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
    /// Run consecutive read-only tool calls from one model message at the same
    /// time. Calls that change anything keep their order.
    pub parallel: bool,
    /// Most tool calls in flight at once.
    pub max_parallel: u32,
    /// Shell commands treated as read-only, and so safe to run beside another
    /// call. Matched like `permissions.deny`: a rule matches a command segment
    /// that equals it or starts with it followed by a space, `*` matches any
    /// continuation. A command that redirects or substitutes is never parallel.
    pub parallel_bash: Vec<String>,
    /// A foreground command that outruns its timeout keeps running as a
    /// background job instead of being killed.
    pub background_on_timeout: bool,
    /// Output kept per job: the first third of it, then the most recent lines.
    pub job_buffer_bytes: usize,
    /// Time a job gets to stop politely before it is killed outright.
    pub job_kill_grace_ms: u64,
    /// Default limit for a `jobs` wait, when the model does not give one.
    pub job_wait_ms: u64,
    /// When a background job ends while the model is idle, start a turn so it
    /// can read the output and say what happened.
    pub job_wake: bool,
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self {
            enabled: [
                "ask_user",
                "bash",
                "read_file",
                "write_file",
                "edit_file",
                "jobs",
                "plan",
                "agent",
                "agents",
            ]
            .iter()
            .map(|s| String::from(*s))
            .collect(),
            disabled: Vec::new(),
            max_output_bytes: 32 * 1024,
            bash_timeout_ms: 120_000,
            shell: String::new(),
            read_default_limit: 2000,
            parallel: true,
            max_parallel: 8,
            parallel_bash: [
                "ls",
                "cat",
                "head",
                "tail",
                "wc",
                "stat",
                "file",
                "find",
                "fd",
                "rg",
                "grep",
                "tree",
                "du",
                "df",
                "pwd",
                "which",
                "echo",
                "date",
                "git log",
                "git status",
                "git diff",
                "git show",
                "git branch",
                "git ls-files",
                "git blame",
                "cargo metadata",
                "cargo tree",
            ]
            .iter()
            .map(|s| String::from(*s))
            .collect(),
            background_on_timeout: true,
            job_buffer_bytes: 256 * 1024,
            job_kill_grace_ms: 2000,
            job_wait_ms: 60_000,
            job_wake: true,
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
    /// What the row shows, left to right, separated by dots. `/statusline`
    /// edits this. Names: `favorite`, `model`, `effort`, `modalities`,
    /// `context`, `tokens`, `cost`, `plan`, `cwd`, `git`, `plugins`, `state`,
    /// `session`.
    pub items: Vec<String>,
    /// A template that replaces `items` when set, drawn in one colour.
    /// Placeholders: `{model} {favorite} {effort} {modalities} {tokens_in}
    /// {tokens_out} {cost} {context} {plan} {cwd} {git} {plugins} {state}
    /// {session}`.
    pub format: String,
    /// Give each item a colour of its own instead of one flat row.
    pub colors: bool,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self {
            items: [
                "favorite",
                "model",
                "effort",
                "modalities",
                "context",
                "tokens",
                "cost",
                "cwd",
                "git",
            ]
            .iter()
            .map(|s| String::from(*s))
            .collect(),
            format: String::new(),
            colors: true,
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
    /// Ask the provider to cache the prompt prefix on models that need an
    /// explicit cache breakpoint. Models that cache on their own are untouched.
    pub cache: bool,
    /// Model id prefixes that need an explicit breakpoint.
    pub cache_models: Vec<String>,
    /// How long the provider keeps the cache: `5m` or `1h`. `1h` doubles the
    /// price of a cache write, so it only pays off across long pauses.
    pub cache_ttl: String,
    /// Skip caching when the estimated prompt is smaller than this. Below the
    /// provider minimum nothing is cached anyway, and a write that is never
    /// read costs more than no cache at all.
    pub cache_min_tokens: u64,
    /// Remind the model of an unfinished plan it has stopped updating. The
    /// reminder is one line at the end of the prompt, so it never disturbs a
    /// cached prefix.
    pub plan_reminder: bool,
    /// Requests without a plan change before the reminder is sent again.
    pub plan_reminder_every: u32,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            auto_compact: true,
            compact_at: 90,
            window: 0,
            summary_max_tokens: 4096,
            cache: true,
            cache_models: ["anthropic/", "qwen/"]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
            cache_ttl: String::from("5m"),
            cache_min_tokens: 2048,
            plan_reminder: true,
            plan_reminder_every: 4,
        }
    }
}

/// Subagents: child agent loops the model starts with the `agent` tool. Each
/// runs on its own thread with its own conversation, model and tools, and
/// hands back one report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Offer the `agent` and `agents` tools at all.
    pub enabled: bool,
    /// Children running at the same time. The rest queue.
    pub max_concurrent: u32,
    /// Children a session may start.
    pub max_total: u32,
    /// 1 lets the model start children; a child at the limit has no `agent`
    /// tool, so it cannot start any of its own.
    pub max_depth: u16,
    /// Tasks accepted in one `agent` call.
    pub max_spawn: usize,
    /// Requests a child may make before it has to stop and report.
    pub max_requests: u32,
    /// Conversation size a child is allowed, in bytes. Children never compact;
    /// one that fills this stops and reports what it has.
    pub max_context_bytes: u64,
    /// How long `agent` waits before leaving the children in the background.
    pub timeout_ms: u64,
    /// Time a child gets to stop politely before it is left to its own devices.
    pub kill_grace_ms: u64,
    /// Tools a child is offered when its definition names none. `plan` and
    /// `ask_user` are always removed: the plan belongs to the session, and a
    /// child has nobody to ask.
    pub tools: Vec<String>,
    /// Model for children whose definition names none; empty inherits.
    pub model: String,
    /// Reasoning effort for those children; empty inherits.
    pub effort: String,
    /// Longest report a child can hand back. The middle is dropped.
    pub report_bytes: usize,
    /// Tool lines kept per child for the agent list.
    pub log_lines: usize,
    /// What a child keeps of its own work, in bytes, so its conversation can be
    /// read the way the main one is. The oldest goes first.
    pub view_bytes: usize,
    /// Finished children kept, with their conversation, for follow-ups.
    pub keep: usize,
    /// When a background child ends while the model is idle, start a turn so
    /// it can read the report and say what happened.
    pub wake: bool,
    /// Let the model name a model per task instead of taking the definition's.
    pub allow_model_arg: bool,
    /// Thread stack for a child; 0 uses the system default.
    pub stack_bytes: usize,
    /// Agent types the model can choose between, by name.
    pub defs: BTreeMap<String, AgentDef>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent: 4,
            max_total: 32,
            max_depth: 1,
            max_spawn: 8,
            max_requests: 40,
            max_context_bytes: 256 * 1024,
            timeout_ms: 600_000,
            kill_grace_ms: 2000,
            tools: ["read_file", "bash", "jobs"]
                .iter()
                .map(|s| String::from(*s))
                .collect(),
            model: String::new(),
            effort: String::new(),
            report_bytes: 8192,
            log_lines: 200,
            view_bytes: 256 * 1024,
            keep: 16,
            wake: true,
            allow_model_arg: false,
            stack_bytes: 0,
            defs: BTreeMap::new(),
        }
    }
}

/// One agent type. The model picks between these by name, so `description` is
/// what it reads when it decides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentDef {
    /// What this type is for, shown to the model in the tool description.
    pub description: String,
    /// System prompt for the child; empty keeps the session's.
    pub prompt: String,
    /// Model id or favorite name; empty falls back to `agents.model`.
    pub model: String,
    /// Reasoning effort; empty falls back to `agents.effort`.
    pub effort: String,
    /// Tools this type is offered; empty falls back to `agents.tools`.
    pub tools: Vec<String>,
    /// Requests this type may make; 0 falls back to `agents.max_requests`.
    pub max_requests: u32,
    /// Wait before backgrounding; 0 falls back to `agents.timeout_ms`.
    pub timeout_ms: u64,
    /// Merge patch applied over the child's settings, after everything above.
    pub settings: Value,
}

/// Dictation. Speech is cut into phrases locally and each one is transcribed
/// by an OpenRouter model that takes audio input, so there is no second key
/// and no local model. Nothing is ever sent on its own: the words land in the
/// input box and wait for Enter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceSettings {
    /// Offer `/voice` at all. Off, the microphone is never opened.
    pub enabled: bool,
    /// Whether dictation was left switched on. `/voice` writes this, so the
    /// switch stays where it was put rather than being thrown again in every
    /// new window.
    pub armed: bool,
    /// Who transcribes: `openrouter` uses the key `ah` already has and any
    /// model that takes audio; `deepgram` opens a socket to Deepgram and
    /// needs a key of its own, which is the only way to get words back while
    /// the sentence is still being said. Empty asks on the first `/voice`.
    pub provider: String,
    /// Model that does the transcribing; empty asks on first use. What counts
    /// as a model depends on `provider`.
    pub model: String,
    /// Seconds an unused Deepgram socket is held open before it is dropped
    /// and reopened on demand. An open connection sending no audio is not
    /// billed, and the handshake costs over a second, so the default holds it
    /// for as long as dictation is armed.
    pub idle_secs: u64,
    /// `fast`, `balanced` or `cheap`: one choice for `phrase_ms`,
    /// `max_chunk_ms` and `max_inflight`. Anything set by hand wins over it.
    pub mode: String,
    /// Words the model would otherwise get wrong: project names, identifiers,
    /// people. Sent with every phrase, so keep it short.
    pub prompt_append: String,
    /// Spoken language; empty lets the model decide.
    pub language: String,
    /// `auto` watches what the terminal reports and picks the best it can:
    /// hold to talk where key releases arrive, otherwise a toggle.
    pub hotkey_mode: String,
    /// How long the talk key has to stay down before it counts as being held
    /// rather than typed. Below this it is an ordinary keystroke, so the
    /// space bar still types spaces while dictation is armed.
    pub dwell_ms: u64,
    /// Silence that counts as letting the talk key go, on terminals that
    /// report no release event. It has to outlast the delay a keyboard waits
    /// before it starts repeating, so it starts wide; once the repeat rate
    /// has actually been seen, a much shorter gap is used instead.
    pub release_grace_ms: u64,
    /// Longest a toggled microphone stays on. Holding a key needs no limit.
    pub max_listen_secs: u64,
    /// Input device name; empty takes the system default.
    pub device: String,
    /// Command that prints raw signed 16-bit little-endian mono PCM on
    /// stdout, replacing the built-in capture. Read from the user config or
    /// the environment only, never from a project file.
    pub capture_cmd: String,
    /// Hold the microphone open for as long as dictation is armed, rather
    /// than opening it for each hold. Opening costs a few tens of
    /// milliseconds, and a microphone that is open is a microphone that is
    /// on, so this is off.
    pub keep_open: bool,
    /// Capture rate; 0 asks for 16000 and resamples whatever the device gives.
    pub sample_rate: u32,
    /// Audio kept before the phrase starts, so nothing is clipped.
    pub ring_ms: u64,
    /// Silence that ends a phrase.
    pub phrase_ms: u64,
    /// Longest phrase before it is cut anyway, at the quietest moment near
    /// the end.
    pub max_chunk_ms: u64,
    /// Phrases transcribed at the same time.
    pub max_inflight: usize,
    /// Audio kept from before speech was detected.
    pub preroll_ms: u64,
    /// How far above the noise floor counts as speech.
    pub speech_ratio: f32,
    /// Drop what models invent over silence: stock phrases and repeat loops.
    pub filter: bool,
    /// Stop a dictation once it has cost this much. 0 does not watch.
    pub budget_usd: f64,
    /// Show what the dictation has cost so far next to the timer.
    pub show_cost: bool,
    /// Draw the input level. It is the only part that redraws on a clock.
    pub meter: bool,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            armed: false,
            provider: String::new(),
            model: String::new(),
            idle_secs: 0,
            mode: String::from("balanced"),
            prompt_append: String::new(),
            language: String::new(),
            hotkey_mode: String::from("auto"),
            dwell_ms: 180,
            release_grace_ms: 700,
            max_listen_secs: 300,
            device: String::new(),
            capture_cmd: String::new(),
            keep_open: false,
            sample_rate: 0,
            ring_ms: 2000,
            phrase_ms: 400,
            max_chunk_ms: 3500,
            max_inflight: 2,
            preroll_ms: 300,
            speech_ratio: 3.0,
            filter: true,
            budget_usd: 0.0,
            show_cost: false,
            meter: false,
        }
    }
}

impl VoiceSettings {
    /// True when phrases go to Deepgram's socket rather than to OpenRouter.
    pub fn live(&self) -> bool {
        self.provider == "deepgram"
    }

    /// The model to use, with the provider's own default filled in. A model
    /// left over from the other provider is not one, so it is ignored.
    pub fn model_for(&self) -> String {
        let m = self.model.trim();
        if self.live() {
            // OpenRouter ids are `author/name`; Deepgram's are not.
            if m.is_empty() || m.contains('/') {
                return String::from("nova-3");
            }
        }
        m.into()
    }

    /// `mode` as the three numbers it stands for, or `None` when the name is
    /// not one of the presets.
    pub fn preset(&self) -> Option<(u64, u64, usize)> {
        match self.mode.as_str() {
            "fast" => Some((300, 2000, 3)),
            "balanced" => Some((400, 3500, 2)),
            "cheap" => Some((700, 8000, 1)),
            _ => None,
        }
    }

    /// `phrase_ms`, `max_chunk_ms` and `max_inflight` after the preset has had
    /// its say. A dial still sitting at its default follows `mode`; one that
    /// has been moved keeps the value it was given.
    pub fn dials(&self) -> (u64, u64, usize) {
        let d = Self::default();
        let Some((phrase, chunk, inflight)) = self.preset() else {
            return (self.phrase_ms, self.max_chunk_ms, self.max_inflight);
        };
        (
            if self.phrase_ms == d.phrase_ms {
                phrase
            } else {
                self.phrase_ms
            },
            if self.max_chunk_ms == d.max_chunk_ms {
                chunk
            } else {
                self.max_chunk_ms
            },
            if self.max_inflight == d.max_inflight {
                inflight
            } else {
                self.max_inflight
            },
        )
    }
}

/// Images the model generates: whether to ask for them, where they land, and
/// how many go back with the next request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageSettings {
    /// When to ask a *chat* model for pictures. `auto` asks only models the
    /// catalogue says draw, `always` asks regardless — for a catalogue that
    /// has not caught up, or a proxy of your own — and `off` never asks. A
    /// model that only draws has no other mode, so it is always called on the
    /// images endpoint whatever this says.
    pub output: ImageOutput,
    /// Where generated images are written. Empty means one directory per
    /// session under the data directory.
    pub dir: String,
    /// How many of the most recent generated images go back to the model with
    /// the next request. Editing a picture needs at least one; 0 is the only
    /// value that never disturbs the provider's prompt cache.
    pub history: u32,
    /// Shape of a resent image. `assistant` hands it back on the message it
    /// came from, which is what OpenRouter expects; `user` re-attaches it to a
    /// message after it, for a provider that refuses assistant images.
    pub echo: ImageEcho,
    /// How pictures are drawn. `auto` uses the kitty or iTerm2 protocol when
    /// the terminal speaks one; `off`, and any terminal that speaks neither,
    /// gets a one-line chip instead.
    pub inline: ImageInline,
    /// Tallest an inline picture gets, in rows. Also capped at two thirds of
    /// the window, so a picture always leaves room for the conversation.
    pub max_rows: u16,
    /// Widest an inline picture gets, in columns; 0 means the transcript width.
    pub max_cols: u16,
    /// Terminal cell size in pixels as `"9x18"`, for terminals that will not
    /// report one. Empty asks the terminal.
    pub cell_px: String,
    /// Command that opens a saved image. `{path}` is substituted, or the path
    /// is appended. Empty tries `xdg-open`, `open`, then `start`.
    pub open_cmd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImageOutput {
    #[default]
    Auto,
    Always,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImageEcho {
    #[default]
    Assistant,
    User,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImageInline {
    #[default]
    Auto,
    Kitty,
    Iterm2,
    Off,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self {
            output: ImageOutput::Auto,
            dir: String::new(),
            // Image models edit: "now make it night" is unusable without the
            // picture it refers to.
            history: 1,
            echo: ImageEcho::Assistant,
            inline: ImageInline::Auto,
            max_rows: 20,
            max_cols: 0,
            cell_px: String::new(),
            open_cmd: String::new(),
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
