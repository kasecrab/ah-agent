//! Plugin manifest and per-hook request/response payloads.
//!
//! Wire format: every payload is UTF-8 JSON. Plugin exports
//! `ah_alloc(len) -> ptr`, `ah_free(ptr, len)`, `ah_manifest() -> u64` and
//! `ah_call(hook_ptr, hook_len, in_ptr, in_len) -> u64` where a `u64` return
//! packs `(ptr << 32) | len` of a buffer the host reads then frees via `ah_free`.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ChatRequest, Message, Settings, ToolCall, ToolResult, ToolSpec, Usage};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub abi_version: u32,
    pub description: String,
    pub hooks: Vec<Hook>,
    pub tools: Vec<ToolSpec>,
    pub commands: Vec<SlashCommandSpec>,
    /// Static settings patch applied at load, before `on_load` runs.
    pub settings_patch: Option<Value>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: String::from("0.0.0"),
            abi_version: crate::ABI_VERSION,
            description: String::new(),
            hooks: Vec::new(),
            tools: Vec::new(),
            commands: Vec::new(),
            settings_patch: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hook {
    OnLoad,
    SystemPrompt,
    BeforeRequest,
    BeforeTool,
    AfterTool,
    ToolCall,
    Statusline,
    SlashCommand,
    OnTurnEnd,
    Keybinds,
}

impl Hook {
    pub const ALL: [Hook; 10] = [
        Hook::OnLoad,
        Hook::SystemPrompt,
        Hook::BeforeRequest,
        Hook::BeforeTool,
        Hook::AfterTool,
        Hook::ToolCall,
        Hook::Statusline,
        Hook::SlashCommand,
        Hook::OnTurnEnd,
        Hook::Keybinds,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Hook::OnLoad => "on_load",
            Hook::SystemPrompt => "system_prompt",
            Hook::BeforeRequest => "before_request",
            Hook::BeforeTool => "before_tool",
            Hook::AfterTool => "after_tool",
            Hook::ToolCall => "tool_call",
            Hook::Statusline => "statusline",
            Hook::SlashCommand => "slash_command",
            Hook::OnTurnEnd => "on_turn_end",
            Hook::Keybinds => "keybinds",
        }
    }

    pub fn parse(s: &str) -> Option<Hook> {
        Hook::ALL.iter().copied().find(|h| h.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SlashCommandSpec {
    pub name: String,
    pub description: String,
    pub usage: String,
}

// ---- hook payloads -------------------------------------------------------

/// `on_load`: input is the fully merged settings; output patch is merged on top.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnLoadIn {
    pub settings: Settings,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OnLoadOut {
    pub settings_patch: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemPromptIn {
    pub prompt: String,
    pub cwd: String,
    pub os: String,
    pub shell: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemPromptOut {
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeforeRequestIn {
    pub request: ChatRequest,
    pub turn: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeforeRequestOut {
    pub request: ChatRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeforeToolIn {
    pub call: ToolCall,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum ToolDecision {
    #[default]
    Allow,
    Deny {
        reason: String,
    },
    /// Run with different arguments (still the same tool).
    Replace {
        arguments: String,
    },
    /// Ask the user even in auto mode.
    Ask {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BeforeToolOut {
    #[serde(flatten)]
    pub decision: ToolDecision,
    pub settings_patch: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AfterToolIn {
    pub call: ToolCall,
    pub result: ToolResult,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AfterToolOut {
    pub result: Option<ToolResult>,
    pub settings_patch: Option<Value>,
}

/// `tool_call`: dispatched only to the plugin whose manifest declares the tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallIn {
    pub call: ToolCall,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallOut {
    pub result: ToolResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StatusContext {
    pub model: String,
    pub usage: Usage,
    pub cwd: String,
    pub git_branch: String,
    pub plugins: u32,
    /// `idle`, `thinking`, `streaming`, `tool:<name>`.
    pub state: String,
    pub session_id: String,
    pub width: u16,
    /// Name of the favorite matching the current model, if any.
    pub favorite: String,
    /// Reasoning effort in use (`model.reasoning.effort`), or empty.
    pub effort: String,
    /// Tokens in the conversation as of the last response, and the model's
    /// window (0 when unknown).
    #[serde(default)]
    pub context_tokens: u64,
    #[serde(default)]
    pub context_window: u64,
    /// Input modality icons for the model (`TI→T`), empty when unknown or hidden.
    #[serde(default)]
    pub modalities: String,
    /// Result of the built-in template, so a plugin can decorate instead of replace.
    pub rendered: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatuslineOut {
    pub text: String,
}

/// Why a `slash_command` call is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SlashStage {
    /// The user typed the command; `args` is what followed it.
    #[default]
    Run,
    /// The cursor in a picker this command opened moved to an item; `args`
    /// is that item's `value`. Return a `settings_patch` to preview it and
    /// change nothing else: the host undoes preview patches on cancel.
    Preview,
    /// The user pressed Enter on a picker item; `args` is its `value`.
    Pick,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlashCommandIn {
    pub name: String,
    pub args: String,
    pub cwd: String,
    #[serde(default)]
    pub stage: SlashStage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SlashCommandOut {
    /// Shown in the transcript as a system notice.
    pub message: Option<String>,
    pub settings_patch: Option<Value>,
    /// If set, submitted to the model as a user message.
    pub send_to_model: Option<String>,
    /// Open a list the user picks from; the choice comes back as another
    /// `slash_command` call with stage `pick`. TUI only.
    pub picker: Option<PickerSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PickerSpec {
    pub title: String,
    pub items: Vec<PickerItem>,
    /// Index of the item under the cursor when the list opens.
    pub selected: usize,
    /// Call the command with stage `preview` whenever the cursor moves.
    pub preview: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PickerItem {
    /// Sent back as `args`.
    pub value: String,
    /// Shown in the list; `value` when empty.
    pub label: String,
    /// Dim text at the right of the row.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnTurnEndIn {
    pub message: Message,
    pub usage: Usage,
    pub total_usage: Usage,
    pub tool_calls: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OnTurnEndOut {
    pub settings_patch: Option<Value>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KeybindsOut {
    /// `(key, action)` where action is a `Keys` field name or a slash command
    /// like `/reload`.
    pub binds: Vec<(String, String)>,
}

/// Log level for the `ah.log` host import.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum LogLevel {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl LogLevel {
    pub fn from_u32(v: u32) -> Self {
        match v {
            0 => LogLevel::Error,
            1 => LogLevel::Warn,
            2 => LogLevel::Info,
            _ => LogLevel::Debug,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
        }
    }
}
