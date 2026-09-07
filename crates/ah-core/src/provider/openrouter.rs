//! OpenRouter (OpenAI-compatible) chat completions over SSE using `ureq`.

use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ah_abi::{ChatRequest, Usage};
use serde::Deserialize;
use serde_json::Value;

use super::sse::{SseItem, SseParser};
use super::{OnEvent, Provider, StreamEvent};
use crate::{Error, Result};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouter {
    agent: ureq::Agent,
    base_url: String,
    api_key: String,
    app_title: String,
    referer: String,
}

impl OpenRouter {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(None)
            .timeout_connect(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .user_agent(concat!("ah/", env!("CARGO_PKG_VERSION")))
            .max_idle_connections(2)
            .build();
        Self {
            agent: config.new_agent(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            app_title: "ah".into(),
            referer: "https://github.com/ah-harness/ah".into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// GET a JSON endpoint (used by `ah models`, key info).
    pub fn get_json(&self, path: &str) -> Result<Value> {
        let mut req = self.agent.get(self.url(path));
        if !self.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.api_key));
        }
        let mut resp = req.call()?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(32 * 1024 * 1024)
            .read_to_string()?;
        if !(200..300).contains(&status) {
            return Err(Error::Api {
                status,
                message: excerpt(&body),
            });
        }
        Ok(serde_json::from_str(&body)?)
    }
}

fn excerpt(s: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(s)
        && let Some(m) = v.pointer("/error/message").and_then(Value::as_str)
    {
        return m.to_string();
    }
    let s = s.trim();
    if s.len() > 400 {
        format!("{}…", &s[..s.floor_char_boundary(400)])
    } else {
        s.to_string()
    }
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageWire>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Deserialize, Default)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct UsageWire {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    cache_discount: Option<f64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize, Default)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: Value,
}

/// Translate one SSE `data:` payload into stream events.
pub(crate) fn handle_chunk(payload: &str, on_event: OnEvent<'_>) -> Result<bool> {
    let chunk: Chunk = match serde_json::from_str(payload) {
        Ok(c) => c,
        Err(e) => {
            crate::warn!("unparseable chunk ({e}): {payload}");
            return Ok(true);
        }
    };
    if let Some(err) = chunk.error {
        return Err(Error::Api {
            status: 200,
            message: format!("{} ({})", err.message, err.code),
        });
    }
    for choice in chunk.choices {
        if let Some(err) = choice.error {
            return Err(Error::Api {
                status: 200,
                message: format!("{} ({})", err.message, err.code),
            });
        }
        if let Some(t) = choice.delta.reasoning.filter(|s| !s.is_empty())
            && !on_event(StreamEvent::Reasoning(t))
        {
            return Ok(false);
        }
        if let Some(t) = choice.delta.content.filter(|s| !s.is_empty())
            && !on_event(StreamEvent::Text(t))
        {
            return Ok(false);
        }
        for tc in choice.delta.tool_calls {
            let f = tc.function.unwrap_or_default();
            let ev = StreamEvent::ToolCallDelta {
                index: tc.index,
                id: tc.id,
                name: f.name,
                arguments: f.arguments.unwrap_or_default(),
            };
            if !on_event(ev) {
                return Ok(false);
            }
        }
        if let Some(r) = choice.finish_reason
            && !on_event(StreamEvent::Finish(r))
        {
            return Ok(false);
        }
    }
    if let Some(u) = chunk.usage {
        let details = u.prompt_tokens_details.unwrap_or_default();
        let usage = Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
            cost: u.cost.unwrap_or(0.0),
            cached_tokens: details.cached_tokens,
            cache_write_tokens: details.cache_write_tokens,
            cache_discount: u.cache_discount.unwrap_or(0.0).abs(),
        };
        if !on_event(StreamEvent::Usage(usage)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Drive an SSE byte stream to completion.
pub(crate) fn read_stream<R: Read>(
    reader: R,
    cancel: &AtomicBool,
    on_event: OnEvent<'_>,
) -> Result<()> {
    let mut br = BufReader::with_capacity(16 * 1024, reader);
    let mut parser = SseParser::new();
    let mut line = String::with_capacity(1024);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        line.clear();
        let n = br.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // EOF without [DONE]; treat as complete.
        }
        let trimmed = line.strip_suffix('\n').unwrap_or(&line);
        match parser.push_line(trimmed) {
            Some(SseItem::Done) => return Ok(()),
            Some(SseItem::Data(d)) if !handle_chunk(d, on_event)? => return Err(Error::Cancelled),
            Some(SseItem::Data(_)) | None => {}
        }
    }
}

impl Provider for OpenRouter {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn stream(&self, req: &ChatRequest, cancel: &AtomicBool, on_event: OnEvent<'_>) -> Result<()> {
        let mut body = serde_json::to_value(req)?;
        attach_images(&mut body);
        body["stream"] = Value::Bool(true);
        body["usage"] = serde_json::json!({ "include": true });
        crate::debug!(
            "request model={} messages={} tools={}",
            req.model,
            req.messages.len(),
            req.tools.len()
        );

        let mut resp = self
            .agent
            .post(self.url("/chat/completions"))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", &self.referer)
            .header("X-Title", &self.app_title)
            .header("Accept", "text/event-stream")
            .send_json(&body)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(Error::Api {
                status,
                message: excerpt(&text),
            });
        }
        let reader = resp.body_mut().with_config().limit(u64::MAX).reader();
        read_stream(reader, cancel, on_event)
    }
}

/// Turn `{"content": "...", "images": [...]}` into the OpenAI content-parts
/// shape; messages without images are left alone.
fn attach_images(body: &mut Value) {
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for m in msgs {
        let Some(images) = m.get("images").and_then(|i| i.as_array()).cloned() else {
            continue;
        };
        let text = m
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(serde_json::json!({"type": "text", "text": text}));
        }
        for url in images {
            parts.push(serde_json::json!({"type": "image_url", "image_url": {"url": url}}));
        }
        if let Some(o) = m.as_object_mut() {
            o.insert("content".into(), Value::Array(parts));
            o.remove("images");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Accumulator;

    #[test]
    fn images_become_content_parts() {
        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "user", "content": "look", "images": ["data:image/png;base64,AA=="]},
        ]});
        attach_images(&mut body);
        assert_eq!(body["messages"][0]["content"], "hi");
        let parts = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "look");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AA==");
        assert!(body["messages"][1].get("images").is_none());
    }

    const FIXTURE: &str = concat!(
        ": OPENROUTER PROCESSING\n\n",
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me \"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"look.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"command\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"id\":\"1\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15,\"cost\":0.0001}}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn parses_openrouter_fixture() {
        let cancel = AtomicBool::new(false);
        let mut acc = Accumulator::default();
        read_stream(FIXTURE.as_bytes(), &cancel, &mut |ev| {
            acc.apply(&ev);
            true
        })
        .unwrap();
        let acc = acc.finish();
        assert_eq!(acc.content, "Let me look.");
        assert_eq!(acc.tool_calls.len(), 1);
        assert_eq!(acc.tool_calls[0].id, "call_a");
        assert_eq!(acc.tool_calls[0].function.name, "bash");
        assert_eq!(acc.tool_calls[0].function.arguments, "{\"command\":\"ls\"}");
        assert_eq!(acc.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(acc.usage.total_tokens, 15);
        assert!((acc.usage.cost - 0.0001).abs() < 1e-9);
    }

    #[test]
    fn cache_usage_fields_are_read() {
        let payload = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,
            "total_tokens":105,"cost":0.002,"cache_discount":-0.0031,
            "prompt_tokens_details":{"cached_tokens":90,"cache_write_tokens":10}}}"#;
        let mut usage = Usage::default();
        handle_chunk(payload, &mut |ev| {
            if let StreamEvent::Usage(u) = ev {
                usage = u;
            }
            true
        })
        .unwrap();
        assert_eq!(usage.cached_tokens, 90);
        assert_eq!(usage.cache_write_tokens, 10);
        assert!((usage.cache_discount - 0.0031).abs() < 1e-9);
    }

    #[test]
    fn mid_stream_error_surfaces() {
        let s = "data: {\"error\":{\"code\":429,\"message\":\"rate limited\"}}\n\n";
        let cancel = AtomicBool::new(false);
        let err = read_stream(s.as_bytes(), &cancel, &mut |_| true).unwrap_err();
        assert!(matches!(err, Error::Api { .. }), "{err}");
        assert!(err.to_string().contains("rate limited"));
    }

    #[test]
    fn cancel_stops_stream() {
        let cancel = AtomicBool::new(false);
        let mut n = 0;
        let err = read_stream(FIXTURE.as_bytes(), &cancel, &mut |_| {
            n += 1;
            n < 2
        })
        .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
    }
}
