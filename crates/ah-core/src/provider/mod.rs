//! Model provider abstraction. One synchronous streaming call; events are
//! delivered through a callback on the calling thread.

pub mod openrouter;
pub mod sse;

use std::sync::atomic::AtomicBool;

use ah_abi::{ChatRequest, ToolCall, ToolFunction, Usage};

use crate::Result;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// A complete generated image as a `data:` URL. Image models send the
    /// whole picture in one chunk; there is no partial form to accumulate.
    Image(String),
    Usage(Usage),
    Finish(String),
}

/// Return `false` from the callback to stop reading the stream.
pub type OnEvent<'a> = &'a mut dyn FnMut(StreamEvent) -> bool;

pub trait Provider: Send + Sync {
    fn stream(&self, req: &ChatRequest, cancel: &AtomicBool, on_event: OnEvent<'_>) -> Result<()>;
    fn name(&self) -> &str;
}

/// Accumulates stream deltas into a complete assistant message.
#[derive(Debug, Default, Clone)]
pub struct Accumulator {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    /// Generated images as `data:` URLs, in arrival order.
    pub images: Vec<String>,
    pub usage: Usage,
    pub finish_reason: Option<String>,
}

impl Accumulator {
    pub fn apply(&mut self, ev: &StreamEvent) {
        match ev {
            StreamEvent::Text(t) => self.content.push_str(t),
            StreamEvent::Reasoning(t) => self.reasoning.push_str(t),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                while self.tool_calls.len() <= *index {
                    self.tool_calls.push(ToolCall {
                        id: String::new(),
                        kind: "function".into(),
                        function: ToolFunction::default(),
                    });
                }
                let tc = &mut self.tool_calls[*index];
                if let Some(id) = id
                    && !id.is_empty()
                {
                    tc.id = id.clone();
                }
                if let Some(n) = name
                    && !n.is_empty()
                {
                    tc.function.name.push_str(n);
                }
                tc.function.arguments.push_str(arguments);
            }
            // A provider that repeats the image on the final `message` as
            // well as in a delta would otherwise store it twice.
            StreamEvent::Image(url) => {
                if !self.images.iter().any(|u| u == url) {
                    self.images.push(url.clone());
                }
            }
            StreamEvent::Usage(u) => self.usage = *u,
            StreamEvent::Finish(r) => self.finish_reason = Some(r.clone()),
        }
    }

    /// Fill in missing ids / empty argument strings.
    pub fn finish(mut self) -> Self {
        for (i, tc) in self.tool_calls.iter_mut().enumerate() {
            if tc.id.is_empty() {
                tc.id = format!("call_{i}");
            }
            if tc.function.arguments.trim().is_empty() {
                tc.function.arguments = "{}".into();
            }
        }
        self
    }

    pub fn into_message(self) -> ah_abi::Message {
        let mut m = ah_abi::Message::assistant(self.content);
        m.tool_calls = self.tool_calls;
        if !self.reasoning.is_empty() {
            m.reasoning = Some(self.reasoning);
        }
        m.images = self.images;
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_tool_call_deltas_by_index() {
        let mut a = Accumulator::default();
        a.apply(&StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("bash".into()),
            arguments: "{\"cmd".into(),
        });
        a.apply(&StreamEvent::ToolCallDelta {
            index: 1,
            id: Some("c2".into()),
            name: Some("read_file".into()),
            arguments: "".into(),
        });
        a.apply(&StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: "\":\"ls\"}".into(),
        });
        a.apply(&StreamEvent::Text("hi".into()));
        let a = a.finish();
        assert_eq!(a.tool_calls.len(), 2);
        assert_eq!(a.tool_calls[0].function.arguments, "{\"cmd\":\"ls\"}");
        assert_eq!(a.tool_calls[0].function.name, "bash");
        assert_eq!(a.tool_calls[1].function.arguments, "{}");
        assert_eq!(a.content, "hi");
    }
}
