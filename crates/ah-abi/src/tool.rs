use alloc::string::String;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tool declaration in the shape OpenRouter/OpenAI expect under `tools[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    #[serde(rename = "type", default = "default_tool_type")]
    pub kind: String,
    pub function: ToolSpecFunction,
}

fn default_tool_type() -> String {
    String::from("function")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpecFunction {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// JSON schema for the arguments object.
    pub parameters: Value,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            kind: default_tool_type(),
            function: ToolSpecFunction {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
    pub fn name(&self) -> &str {
        &self.function.name
    }
}

/// Outcome of running a tool. Errors go back to the model as text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolResult {
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
        }
    }
    pub fn err(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: true,
        }
    }
}
