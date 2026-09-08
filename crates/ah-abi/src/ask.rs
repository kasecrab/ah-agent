//! Putting a question to the person at the keyboard: what the model asks with
//! the `ask_user` tool, and what comes back.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// One option offered for a question.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    /// What picking it means, shown under the label.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// One question. `options` may be empty, which asks for text and nothing else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Two or three words naming the decision, for the title of the box.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub header: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<Choice>,
    /// More than one option may be picked.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub multi: bool,
}

/// Everything one `ask_user` call wants to know. The questions are put one
/// after another and answered in the same round trip.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub questions: Vec<Question>,
}

/// What the user said to one question. Both halves can be filled: an option
/// picked and a note about it. Never both empty — a question the user did not
/// answer dismisses the whole ask instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// Labels picked, in the order they were offered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub picked: Vec<String>,
    /// What the user typed: an answer of its own when nothing was picked,
    /// otherwise what they wanted to add to the choice.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl Answer {
    pub fn is_empty(&self) -> bool {
        self.picked.is_empty() && self.note.is_empty()
    }
}

/// What came back from the question. `Unavailable` means there was no user to
/// ask at all — a piped or scripted run — which the model must handle by
/// deciding for itself rather than by asking again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reply")]
pub enum Reply {
    Answered { answers: Vec<Answer> },
    Dismissed,
    Unavailable,
}
