//! OpenRouter (OpenAI-compatible) chat completions over SSE using `ureq`.

use std::io::{BufRead, BufReader};
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
            // One per agent that can be streaming at once, plus the catalogue
            // fetch: subagents share this pool, and a connection they cannot
            // keep is a TLS handshake they pay for again.
            .max_idle_connections(8)
            .max_idle_connections_per_host(8)
            // A model that stops sending without closing the socket would
            // otherwise park the reader thread for the life of the process.
            .timeout_recv_body(Some(Duration::from_secs(120)))
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

    /// Transcribe audio on `/audio/transcriptions`, which is where the
    /// speech-to-text models live. Nothing about it is a chat: the whole clip
    /// goes up as base64 and the whole transcript comes back at once, so
    /// there is no stream to follow and no prompt to pay for.
    ///
    /// Returns the text and what it cost, which the response reports itself.
    pub fn transcribe(
        &self,
        model: &str,
        audio: &[u8],
        format: &str,
        language: &str,
    ) -> Result<(String, f64)> {
        let mut body = serde_json::json!({
            "model": model,
            "input_audio": {
                "data": crate::clipboard::base64(audio),
                "format": format,
            },
        });
        if !language.trim().is_empty() {
            body["language"] = Value::String(language.trim().to_string());
        }
        crate::debug!(
            "transcribe model={model} format={format} audio={}B",
            audio.len()
        );
        let mut resp = self
            .agent
            .post(self.url("/audio/transcriptions"))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", self.referer.clone())
            .header("X-Title", self.app_title.clone())
            .send_json(&body)?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_to_string()?;
        if !(200..300).contains(&status) {
            return Err(Error::Api {
                status,
                message: excerpt(&text),
            });
        }
        let v: Value = serde_json::from_str(&text)?;
        let said = v["text"].as_str().unwrap_or_default().to_string();
        let cost = v["usage"]["cost"].as_f64().unwrap_or(0.0);
        Ok((said, cost))
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
    /// The whole message, which some providers repeat on the last chunk. Only
    /// its images are read: the text already arrived as deltas, and reading it
    /// again would say everything twice.
    #[serde(default)]
    message: Option<MessageWire>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize, Default)]
struct MessageWire {
    #[serde(default)]
    images: Vec<ImageWire>,
}

/// `{"type":"image_url","image_url":{"url":"data:…"}}`. Some providers put a
/// bare string on `image_url`, and a few put the url one level up.
#[derive(Deserialize)]
struct ImageWire {
    #[serde(default)]
    image_url: Option<ImageUrlWire>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImageUrlWire {
    /// First, so a bare string is not tried against the object form.
    Url(String),
    Object {
        #[serde(default)]
        url: Option<String>,
    },
}

impl ImageWire {
    /// The inline image, if there is one. A remote url is dropped: ah has no
    /// fetcher, and adding one would mean a dependency and a fresh set of
    /// questions about what a model can make the client request.
    fn url(self) -> Option<String> {
        match self.image_url {
            Some(ImageUrlWire::Url(u)) => Some(u),
            Some(ImageUrlWire::Object { url }) => url,
            None => self.url,
        }
        .filter(|u| u.starts_with("data:"))
    }
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
    #[serde(default)]
    images: Vec<ImageWire>,
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
        for img in choice
            .delta
            .images
            .into_iter()
            .chain(choice.message.unwrap_or_default().images)
        {
            if let Some(url) = img.url()
                && !on_event(StreamEvent::Image(url))
            {
                return Ok(false);
            }
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

/// What the line source has for us right now.
enum Piece {
    Line(String),
    /// Nothing yet; a chance to notice a cancel.
    Idle,
    End,
}

/// Feed SSE lines to `on_event` until the stream ends, `next` fails, or the
/// turn is cancelled. `next` must return `Idle` rather than block for long,
/// so a cancel is acted on within one tick instead of at the next byte.
fn drive(
    cancel: &AtomicBool,
    on_event: OnEvent<'_>,
    mut next: impl FnMut() -> Result<Piece>,
) -> Result<()> {
    let mut parser = SseParser::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let line = match next()? {
            Piece::Idle => continue,
            Piece::End => return Ok(()), // EOF without [DONE]; treat as complete.
            Piece::Line(l) => l,
        };
        let trimmed = line.strip_suffix('\n').unwrap_or(&line);
        match parser.push_line(trimmed) {
            Some(SseItem::Done) => return Ok(()),
            Some(SseItem::Data(d)) if !handle_chunk(d, on_event)? => return Err(Error::Cancelled),
            Some(SseItem::Data(_)) | None => {}
        }
    }
}

/// Drive an SSE byte stream to completion. Reads block, so this is for
/// sources that cannot stall: fixtures and tests.
#[cfg(test)]
fn read_stream<R: std::io::Read>(
    reader: R,
    cancel: &AtomicBool,
    on_event: OnEvent<'_>,
) -> Result<()> {
    let mut br = BufReader::with_capacity(16 * 1024, reader);
    let mut line = String::with_capacity(1024);
    drive(cancel, on_event, move || {
        line.clear();
        Ok(match br.read_line(&mut line)? {
            0 => Piece::End,
            _ => Piece::Line(line.clone()),
        })
    })
}

/// How long a read waits before the cancel flag is looked at again.
const TICK: Duration = Duration::from_millis(40);

impl Provider for OpenRouter {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn stream(&self, req: &ChatRequest, cancel: &AtomicBool, on_event: OnEvent<'_>) -> Result<()> {
        let mut body = serde_json::to_value(req)?;
        attach_images(&mut body);
        let audio_bytes = attach_audio(&mut body);
        body["stream"] = Value::Bool(true);
        body["usage"] = serde_json::json!({ "include": true });
        // Never the body: one dictated phrase is a few hundred KB of the
        // user's voice in base64, and the log is a file on disk.
        crate::debug!(
            "request model={} messages={} tools={} audio={}B",
            req.model,
            req.messages.len(),
            req.tools.len(),
            audio_bytes
        );

        // The request and every read happen on their own thread: a socket read
        // blocks until bytes arrive, and Esc must not wait for a slow model.
        // This side only ever waits `TICK` for the next line.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Piece>>(64);
        let agent = self.agent.clone();
        let url = self.url("/chat/completions");
        let key = self.api_key.clone();
        let referer = self.referer.clone();
        let title = self.app_title.clone();
        let worker = std::thread::Builder::new()
            .name("ah-stream".into())
            .spawn(move || {
                let send = |v| tx.send(v).is_ok();
                let mut resp = match agent
                    .post(url)
                    .header("Authorization", format!("Bearer {key}"))
                    .header("HTTP-Referer", referer)
                    .header("X-Title", title)
                    .header("Accept", "text/event-stream")
                    .send_json(&body)
                {
                    Ok(r) => r,
                    Err(e) => {
                        send(Err(e.into()));
                        return;
                    }
                };
                let status = resp.status().as_u16();
                if !(200..300).contains(&status) {
                    let text = resp.body_mut().read_to_string().unwrap_or_default();
                    send(Err(Error::Api {
                        status,
                        message: excerpt(&text),
                    }));
                    return;
                }
                let reader = resp.body_mut().with_config().limit(u64::MAX).reader();
                let mut br = BufReader::with_capacity(16 * 1024, reader);
                let mut line = String::with_capacity(1024);
                loop {
                    line.clear();
                    let chunk = match br.read_line(&mut line) {
                        Ok(0) => Ok(Piece::End),
                        Ok(_) => Ok(Piece::Line(line.clone())),
                        Err(e) => Err(e.into()),
                    };
                    let end = !matches!(chunk, Ok(Piece::Line(_)));
                    // A receiver that has gone away means the turn was
                    // cancelled; dropping the response closes the socket.
                    if !send(chunk) || end {
                        return;
                    }
                }
            })?;
        let out = drive(cancel, on_event, || {
            match rx.recv_timeout(TICK) {
                Ok(chunk) => chunk,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(Piece::Idle),
                // the worker is done and said all it had to say
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(Piece::End),
            }
        });
        // Cancelled turns leave the worker to unwind on its own; it exits as
        // soon as its next send fails.
        if out.is_ok() {
            let _ = worker.join();
        }
        out
    }
}

/// Put `images` on the wire. A user message becomes OpenAI content parts; an
/// assistant message keeps its text and hands its images back in the shape
/// they arrived in, which is what a provider expects to see for a picture it
/// generated — the OpenAI schema has no image content part for an assistant.
/// Messages without images are left alone.
fn attach_images(body: &mut Value) {
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    for m in msgs {
        let Some(images) = m.get("images").and_then(|i| i.as_array()).cloned() else {
            continue;
        };
        if images.is_empty() {
            continue;
        }
        let assistant = m.get("role").and_then(Value::as_str) == Some("assistant");
        let Some(o) = m.as_object_mut() else { continue };
        if assistant {
            let wire: Vec<Value> = images
                .into_iter()
                .map(|url| serde_json::json!({"type": "image_url", "image_url": {"url": url}}))
                .collect();
            o.insert("images".into(), Value::Array(wire));
            continue;
        }
        let text = o
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
        o.insert("content".into(), Value::Array(parts));
        o.remove("images");
    }
}

/// Turn `{"content": "...", "audio": ["data:audio/wav;base64,..."]}` into
/// OpenAI `input_audio` parts. Returns how many base64 bytes were attached,
/// which is all anything is allowed to say about them.
fn attach_audio(body: &mut Value) -> usize {
    let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return 0;
    };
    let mut bytes = 0;
    for m in msgs {
        let Some(audio) = m.get("audio").and_then(|a| a.as_array()).cloned() else {
            continue;
        };
        let text = m
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let mut parts = match m.get("content").and_then(|c| c.as_array()) {
            Some(existing) => existing.clone(),
            None => {
                let mut v = Vec::new();
                if !text.is_empty() {
                    v.push(serde_json::json!({"type": "text", "text": text}));
                }
                v
            }
        };
        for url in audio {
            let Some(url) = url.as_str() else { continue };
            let (format, data) = split_audio_url(url);
            bytes += data.len();
            parts.push(serde_json::json!({
                "type": "input_audio",
                "input_audio": {"data": data, "format": format}
            }));
        }
        if let Some(o) = m.as_object_mut() {
            o.insert("content".into(), Value::Array(parts));
            o.remove("audio");
        }
    }
    bytes
}

/// `data:audio/wav;base64,AAAA` splits into `("wav", "AAAA")`. Anything that
/// is not a data URL is passed through as bare wav payload, which is what a
/// caller that already has base64 would hand over.
fn split_audio_url(url: &str) -> (&str, &str) {
    let Some(rest) = url.strip_prefix("data:") else {
        return ("wav", url);
    };
    let Some((meta, data)) = rest.split_once(',') else {
        return ("wav", url);
    };
    let mime = meta.split(';').next().unwrap_or("");
    let format = match mime.rsplit_once('/') {
        Some((_, "mpeg")) => "mp3",
        Some((_, "x-wav" | "wave")) => "wav",
        Some((_, sub)) if !sub.is_empty() => sub,
        _ => "wav",
    };
    (format, data)
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

    #[test]
    fn an_assistant_keeps_its_images_as_an_array() {
        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": "draw a cat"},
            {"role": "assistant", "content": "here", "images": ["data:image/png;base64,AA=="]},
            {"role": "assistant", "content": "", "images": ["data:image/png;base64,BB=="]},
            {"role": "user", "content": "empty", "images": []},
        ]});
        attach_images(&mut body);
        // Text stays a string and the images ride beside it, the way the
        // provider sent them back.
        assert_eq!(body["messages"][1]["content"], "here");
        let imgs = body["messages"][1]["images"].as_array().unwrap();
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0]["type"], "image_url");
        assert_eq!(imgs[0]["image_url"]["url"], "data:image/png;base64,AA==");
        // An image-only answer keeps its empty content.
        assert_eq!(body["messages"][2]["content"], "");
        // A user message with no images is not rewritten into empty parts.
        assert_eq!(body["messages"][3]["content"], "empty");
    }

    #[test]
    fn generated_images_arrive_as_events() {
        const IMAGES: &str = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Here you go.\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"images\":[{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/png;base64,AAAA\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"images\":[{\"type\":\"image_url\",\"image_url\":\"data:image/png;base64,BBBB\"}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"images\":[{\"url\":\"https://example.com/a.png\"},{}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"message\":{\"images\":[{\"image_url\":{\"url\":\"data:image/png;base64,AAAA\"}}]},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let mut acc = Accumulator::default();
        let cancel = AtomicBool::new(false);
        read_stream(IMAGES.as_bytes(), &cancel, &mut |e| {
            acc.apply(&e);
            true
        })
        .unwrap();
        assert_eq!(acc.content, "Here you go.");
        // Both shapes of url are read, the remote one is dropped, and the
        // repeat on `message` does not become a second copy.
        assert_eq!(
            acc.images,
            vec![
                "data:image/png;base64,AAAA".to_string(),
                "data:image/png;base64,BBBB".to_string()
            ]
        );
        assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
        assert_eq!(acc.into_message().images.len(), 2);
    }

    #[test]
    fn audio_becomes_input_audio_parts() {
        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": "transcribe", "audio": ["data:audio/wav;base64,QUJD"]},
        ]});
        let n = attach_audio(&mut body);
        assert_eq!(n, 4);
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "transcribe");
        assert_eq!(parts[1]["type"], "input_audio");
        assert_eq!(parts[1]["input_audio"]["data"], "QUJD");
        assert_eq!(parts[1]["input_audio"]["format"], "wav");
        assert!(body["messages"][0].get("audio").is_none());
    }

    #[test]
    fn audio_rides_beside_images_on_one_message() {
        let mut body = serde_json::json!({"messages": [
            {"role": "user", "content": "both",
             "images": ["data:image/png;base64,AA=="],
             "audio": ["data:audio/wav;base64,QQ=="]},
        ]});
        attach_images(&mut body);
        attach_audio(&mut body);
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0]["text"], "both");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[2]["type"], "input_audio");
    }

    #[test]
    fn audio_formats_come_from_the_mime_type() {
        assert_eq!(split_audio_url("data:audio/wav;base64,AA"), ("wav", "AA"));
        assert_eq!(split_audio_url("data:audio/mpeg;base64,AA"), ("mp3", "AA"));
        assert_eq!(split_audio_url("data:audio/flac;base64,AA"), ("flac", "AA"));
        assert_eq!(split_audio_url("QUJD"), ("wav", "QUJD"));
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
    fn a_stalled_stream_still_notices_a_cancel() {
        let cancel = AtomicBool::new(false);
        let start = std::time::Instant::now();
        let err = drive(&cancel, &mut |_| true, || {
            // A model that has sent nothing yet: every read comes back empty.
            if start.elapsed() > Duration::from_millis(60) {
                cancel.store(true, Ordering::Relaxed);
            }
            Ok(Piece::Idle)
        })
        .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
        assert!(start.elapsed() < Duration::from_secs(1), "cancel was slow");
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
