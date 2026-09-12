//! The agent loop: one user turn = N model calls interleaved with tool runs.
//! Synchronous; the caller runs it on a worker thread and receives progress
//! through the [`AgentIo`] callbacks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ah_abi::*;
use serde_json::Value;

use crate::provider::{Accumulator, Provider, StreamEvent};
use crate::tools::{Registry, ToolCtx};
use crate::{Error, Result};

/// Progress events for a UI (TUI, CLI printer, JSONL emitter).
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    RequestStart {
        turn: u32,
    },
    Text(String),
    Reasoning(String),
    /// Streaming finished for this request; the full message is attached.
    AssistantMessage(Message),
    Usage(Usage),
    ToolStart(ToolCall),
    ToolEnd {
        call: ToolCall,
        result: ToolResult,
        duration_ms: u64,
    },
    ToolDenied {
        call: ToolCall,
        reason: String,
    },
    ToolMessage(Message),
    /// The model drew something and it is on disk. `width` and `height` are 0
    /// for a format ah cannot measure. The bytes are deliberately not here: a
    /// UI on the far side of a channel reads the file.
    Image {
        path: PathBuf,
        mime: String,
        width: u32,
        height: u32,
        bytes: usize,
    },
    Notice(String),
    SettingsPatch(Value),
    Retry {
        attempt: u32,
        wait_ms: u64,
        error: String,
    },
    Error(String),
    /// A summary request went out; the conversation is being compacted. `auto`
    /// is true when the window filled up, false for `/compact`.
    Compacting {
        auto: bool,
    },
    /// How far the summary has got: `done` tokens written of the `budget` it is
    /// allowed. Only what the model streams back can be measured, so the count
    /// stands still while the request is still being read.
    CompactProgress {
        done: u64,
        budget: u64,
    },
    /// The conversation was replaced by a summary. `before` is the token
    /// count that triggered it, `after` a rough size of the summary.
    Compacted {
        before: u64,
        after: u64,
        summary: String,
    },
    TurnEnd(TurnSummary),
}

/// What the model is asked when the conversation is compacted.
pub const SUMMARY_PROMPT: &str = "Summarise this conversation so it can continue in a fresh \
context with none of the messages above. Include: the user's requests in order and the \
exact wording of constraints they set; decisions made and why; files, functions and \
commands touched, with paths; the current state of the work, what is done and what is \
left; open questions. Be dense and specific; headings and lists are fine. No preamble.";

/// The two halves of the wrapper [`summary_message`] puts around a summary.
/// How often the summary's size goes out while it streams.
const PROGRESS_EVERY: Duration = Duration::from_millis(120);

const SUMMARY_OPEN: &str = "[The conversation so far was compacted. Summary:]";
const SUMMARY_CLOSE: &str = "[End of summary. Continue from here.]";

/// Wrap a summary as the single user message a compacted conversation starts with.
pub fn summary_message(summary: &str) -> Message {
    Message::user(format!(
        "{SUMMARY_OPEN}\n\n{}\n\n{SUMMARY_CLOSE}",
        summary.trim()
    ))
}

/// The summary inside such a message, or `None` when `content` is an ordinary
/// message. Lets a UI replaying a session file tell the two apart.
pub fn summary_text(content: &str) -> Option<&str> {
    let rest = content.trim().strip_prefix(SUMMARY_OPEN)?;
    Some(
        rest.trim_end()
            .strip_suffix(SUMMARY_CLOSE)
            .unwrap_or(rest)
            .trim(),
    )
}

/// Times one turn pays to ask again after a reply ran out of room mid-call.
const MAX_CUT_OFF: u32 = 2;
/// Room to assume when the settings name none, and the ceiling to raise it to.
const DEFAULT_ROOM: u32 = 8192;
const MAX_ROOM: u32 = 65_536;

/// The name of the first tool call whose arguments are not whole JSON, if any.
fn unfinished_call(acc: &Accumulator) -> Option<String> {
    acc.tool_calls
        .iter()
        .find(|c| serde_json::from_str::<Value>(&c.function.arguments).is_err())
        .map(|c| c.function.name.clone())
}

/// Rough token count for text of `bytes` bytes.
pub fn estimate_tokens(bytes: usize) -> u64 {
    (bytes / 4) as u64
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnSummary {
    pub usage: Usage,
    pub requests: u32,
    pub tool_calls: u32,
    pub cancelled: bool,
}

/// One event as a flat JSON object, for `--json` and for anything watching from
/// off the machine. Written out by hand rather than derived: ten of the
/// variants are newtypes over values that are not maps, which an internally
/// tagged enum cannot carry, and these shapes are published in
/// `docs/commands.md`.
pub fn event_json(ev: &AgentEvent) -> Value {
    use serde_json::json;
    match ev {
        AgentEvent::RequestStart { turn } => json!({"type": "request_start", "turn": turn}),
        AgentEvent::Text(t) => json!({"type": "text", "text": t}),
        AgentEvent::Reasoning(t) => json!({"type": "reasoning", "text": t}),
        AgentEvent::AssistantMessage(m) => json!({"type": "assistant", "message": m}),
        AgentEvent::Usage(u) => json!({"type": "usage", "usage": u}),
        AgentEvent::ToolStart(c) => json!({"type": "tool_start", "call": c}),
        AgentEvent::ToolEnd {
            call,
            result,
            duration_ms,
        } => {
            json!({"type": "tool_end", "call": call, "result": result, "duration_ms": duration_ms})
        }
        AgentEvent::ToolDenied { call, reason } => {
            json!({"type": "tool_denied", "call": call, "reason": reason})
        }
        AgentEvent::ToolMessage(m) => json!({"type": "tool_message", "message": m}),
        AgentEvent::Image {
            path,
            mime,
            width,
            height,
            bytes,
        } => json!({
            "type": "image", "path": path, "mime": mime,
            "width": width, "height": height, "bytes": bytes
        }),
        AgentEvent::Notice(n) => json!({"type": "notice", "text": n}),
        AgentEvent::SettingsPatch(p) => json!({"type": "settings_patch", "patch": p}),
        AgentEvent::Retry {
            attempt,
            wait_ms,
            error,
        } => json!({"type": "retry", "attempt": attempt, "wait_ms": wait_ms, "error": error}),
        AgentEvent::Error(e) => json!({"type": "error", "error": e}),
        AgentEvent::Compacting { auto } => json!({"type": "compacting", "auto": auto}),
        AgentEvent::CompactProgress { done, budget } => {
            json!({"type": "compact_progress", "done": done, "budget": budget})
        }
        AgentEvent::Compacted {
            before,
            after,
            summary,
        } => json!({"type": "compacted", "before": before, "after": after, "summary": summary}),
        AgentEvent::TurnEnd(s) => {
            json!({"type": "turn_end", "requests": s.requests, "tool_calls": s.tool_calls, "usage": s.usage, "cancelled": s.cancelled})
        }
    }
}

/// The way back, for a reader on the other end of a pipe or a socket. A shape
/// this build does not know comes back as `None` rather than as a guess:
/// skipping an event is better than mistaking it for one it is not.
pub fn event_from_json(v: &Value) -> Option<AgentEvent> {
    fn text(v: &Value, key: &str) -> Option<String> {
        Some(v.get(key)?.as_str()?.to_string())
    }
    fn num(v: &Value, key: &str) -> Option<u64> {
        v.get(key)?.as_u64()
    }
    fn of<T: serde::de::DeserializeOwned>(v: &Value, key: &str) -> Option<T> {
        serde_json::from_value(v.get(key)?.clone()).ok()
    }

    Some(match v.get("type")?.as_str()? {
        "request_start" => AgentEvent::RequestStart {
            turn: num(v, "turn")? as u32,
        },
        "text" => AgentEvent::Text(text(v, "text")?),
        "reasoning" => AgentEvent::Reasoning(text(v, "text")?),
        "assistant" => AgentEvent::AssistantMessage(of(v, "message")?),
        "usage" => AgentEvent::Usage(of(v, "usage")?),
        "tool_start" => AgentEvent::ToolStart(of(v, "call")?),
        "tool_end" => AgentEvent::ToolEnd {
            call: of(v, "call")?,
            result: of(v, "result")?,
            duration_ms: num(v, "duration_ms")?,
        },
        "tool_denied" => AgentEvent::ToolDenied {
            call: of(v, "call")?,
            reason: text(v, "reason")?,
        },
        "tool_message" => AgentEvent::ToolMessage(of(v, "message")?),
        "image" => AgentEvent::Image {
            path: PathBuf::from(text(v, "path")?),
            mime: text(v, "mime")?,
            width: num(v, "width")? as u32,
            height: num(v, "height")? as u32,
            bytes: num(v, "bytes")? as usize,
        },
        "notice" => AgentEvent::Notice(text(v, "text")?),
        "settings_patch" => AgentEvent::SettingsPatch(v.get("patch")?.clone()),
        "retry" => AgentEvent::Retry {
            attempt: num(v, "attempt")? as u32,
            wait_ms: num(v, "wait_ms")?,
            error: text(v, "error")?,
        },
        "error" => AgentEvent::Error(text(v, "error")?),
        "compacting" => AgentEvent::Compacting {
            auto: v.get("auto")?.as_bool()?,
        },
        "compact_progress" => AgentEvent::CompactProgress {
            done: num(v, "done")?,
            budget: num(v, "budget")?,
        },
        "compacted" => AgentEvent::Compacted {
            before: num(v, "before")?,
            after: num(v, "after")?,
            summary: text(v, "summary")?,
        },
        "turn_end" => AgentEvent::TurnEnd(TurnSummary {
            usage: of(v, "usage")?,
            requests: num(v, "requests")? as u32,
            tool_calls: num(v, "tool_calls")? as u32,
            cancelled: v.get("cancelled")?.as_bool()?,
        }),
        _ => return None,
    })
}

/// Rough byte size of a request prompt: what the provider bills as input.
fn prompt_bytes(system: &str, messages: &[Message], tools_bytes: usize) -> usize {
    let msgs: usize = messages
        .iter()
        .map(|m| {
            m.content.len()
                + m.reasoning.as_deref().map_or(0, str::len)
                + m.tool_calls
                    .iter()
                    .map(|c| c.function.name.len() + c.function.arguments.len())
                    .sum::<usize>()
                // Not the bytes of the image: a provider bills a flat-ish
                // number of tokens per picture whatever the file weighs, and
                // counting the base64 would compact the conversation after
                // every one.
                + m.images.len() * crate::image::IMAGE_TOKENS * 4
        })
        .sum();
    system.len() + msgs + tools_bytes
}

/// Rough size of a conversation on its own, for a session resumed from disk:
/// there is no usage figure until the model answers, but the gauge and
/// `/compact` both need to know how full the window is.
pub fn messages_tokens(messages: &[Message]) -> u64 {
    estimate_tokens(prompt_bytes("", messages, 0))
}

/// Byte size of the tool declarations, which sit in the cached prefix too.
fn tools_bytes(tools: &[ToolSpec]) -> usize {
    tools
        .iter()
        .map(|t| {
            t.function.name.len()
                + t.function.description.len()
                + t.function.parameters.to_string().len()
        })
        .sum()
}

/// Callbacks the loop uses to talk to whoever is driving it. Read-only tool
/// calls run side by side, so the driver is shared between threads.
pub trait AgentIo: Sync {
    fn emit(&self, ev: AgentEvent);
    /// Blocking permission prompt. Return `false` to deny.
    fn ask_permission(&self, call: &ToolCall, reason: &str) -> bool;
    /// Put the `ask_user` tool's questions to the user and block until they
    /// answer. A driver with nobody at a keyboard leaves this alone.
    fn ask_user(&self, _ask: &Ask) -> Reply {
        Reply::Unavailable
    }
}

/// [`AgentIo::ask_user`] as the asker a tool sees. The loop hands the driver to
/// tools through this rather than as itself: one tool asks questions, and the
/// rest have no business with the rest of the driver.
struct IoAsker<'a>(&'a dyn AgentIo);

impl crate::tools::AskUser for IoAsker<'_> {
    fn ask(&self, ask: &Ask) -> Reply {
        self.0.ask_user(ask)
    }
}

/// Somewhere the loop looks for messages that arrived while it was working:
/// what a subagent is told by the agent above it, or by the user watching it.
/// Read between requests, so a message never lands mid-tool.
pub trait Mailbox: Send + Sync {
    fn take(&self) -> Vec<String>;
}

/// Plugin hook surface used by the loop. `NoHooks` is the empty impl.
pub trait Hooks {
    fn system_prompt(&mut self, input: SystemPromptIn) -> String {
        input.prompt
    }
    fn before_request(&mut self, req: ChatRequest, _turn: u32) -> ChatRequest {
        req
    }
    fn before_tool(&mut self, _call: &ToolCall, _cwd: &str) -> (ToolDecision, Vec<Value>) {
        (ToolDecision::Allow, Vec::new())
    }
    fn after_tool(
        &mut self,
        _call: &ToolCall,
        result: ToolResult,
        _duration_ms: u64,
    ) -> (ToolResult, Vec<Value>) {
        (result, Vec::new())
    }
    /// `Some` if a plugin owns this tool name.
    fn plugin_tool(&mut self, _call: &ToolCall, _cwd: &str) -> Option<ToolResult> {
        None
    }
    fn plugin_tool_specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn on_turn_end(&mut self, _input: OnTurnEndIn) -> (Vec<Value>, Vec<String>) {
        (Vec::new(), Vec::new())
    }
}

pub struct NoHooks;

/// Outcome of the checks a call passes before it runs.
enum Gate {
    Run(ToolCall),
    Refused(ToolResult),
}
impl Hooks for NoHooks {}

pub struct Agent<'a> {
    pub provider: &'a dyn Provider,
    pub registry: &'a Registry,
    pub hooks: &'a mut dyn Hooks,
    pub settings: &'a Settings,
    pub cwd: PathBuf,
    pub cancel: &'a AtomicBool,
    /// Hard stop on runaway tool loops.
    pub max_requests: u32,
    /// The id this loop's jobs and subagents are filed under. 0 for the agent
    /// the user talks to in a window; a subagent uses its own id, and a daemon
    /// gives each of its sessions one of its own so that two sessions in one
    /// process cannot read, kill or answer each other's work.
    pub agent_id: u32,
    /// Whether this loop is a subagent, as opposed to one somebody is talking
    /// to. It used to be `agent_id != 0`, which stopped being the same
    /// question once a main loop could have an id of its own.
    pub child: bool,
    /// Id of the session this turn belongs to, empty when there is none. It
    /// rides along as the provider's sticky-routing key.
    pub session_id: String,
    /// Model context window in tokens; 0 disables auto compaction.
    pub context_window: u64,
    /// Conversation size as of the last response.
    pub context_tokens: u64,
    /// Compactions performed by this agent (the caller re-persists messages).
    pub compactions: u32,
    /// Tell the model about background jobs that ended and plans it has left
    /// alone. Off in tests that assert on exact message lists.
    pub notices: bool,
    /// Where messages for this loop arrive while it works. Subagents have one;
    /// the agent the user talks to hears from the user directly.
    pub mailbox: Option<std::sync::Arc<dyn Mailbox>>,
    /// What the `agent` tool starts subagents with. `None` leaves the loop
    /// unable to start any, whatever its tool list says.
    pub spawner: Option<std::sync::Arc<dyn crate::agents::Spawner + Sync>>,
    /// Output modalities of the current model as the catalogue reports them,
    /// empty when it does not list the model. The agent never reads the
    /// catalogue itself; the caller, which already has it for the context
    /// window, passes it in.
    pub output_modalities: crate::models::Modalities,
    /// Where generated images are written. `None` turns the feature off, which
    /// is what a caller with nowhere to put a file wants.
    pub image_dir: Option<PathBuf>,
    /// Plan version behind the last reminder, and how many requests have gone
    /// by without the plan changing.
    plan_seen: u64,
    plan_quiet: u32,
    /// Last reference read back off disk and the data URL it produced, so a
    /// turn of ten tool calls re-encodes the picture once instead of ten times.
    image_cache: Option<(String, String)>,
}

impl<'a> Agent<'a> {
    pub fn new(
        provider: &'a dyn Provider,
        registry: &'a Registry,
        hooks: &'a mut dyn Hooks,
        settings: &'a Settings,
        cwd: PathBuf,
        cancel: &'a AtomicBool,
    ) -> Self {
        Self {
            provider,
            registry,
            hooks,
            settings,
            cwd,
            cancel,
            max_requests: 200,
            agent_id: 0,
            child: false,
            session_id: String::new(),
            context_window: 0,
            context_tokens: 0,
            compactions: 0,
            notices: true,
            mailbox: None,
            spawner: None,
            output_modalities: crate::models::Modalities::EMPTY,
            image_dir: None,
            plan_seen: 0,
            plan_quiet: 0,
            image_cache: None,
        }
    }

    /// Modalities to ask this model for; empty leaves the key off the request.
    fn modalities(&self) -> Vec<String> {
        if self.image_dir.is_none() {
            return Vec::new();
        }
        // A model that only draws has no other mode to fall back on, so this
        // is not a request for image output — it is what the model is. The
        // setting decides whether a *chat* model is asked for pictures.
        if self.draws_only() {
            return vec!["image".into()];
        }
        let wanted = match self.settings.images.output {
            ImageOutput::Off => false,
            ImageOutput::Always => true,
            // Unknown means no. Asking a model that cannot draw for images is
            // an error on some providers, and a catalogue that has not been
            // fetched yet is the normal state for the first second of a run.
            ImageOutput::Auto => self.output_modalities.has("image"),
        };
        if !wanted {
            return Vec::new();
        }
        let text = self.settings.images.output == ImageOutput::Always
            || self.output_modalities.is_empty()
            || self.output_modalities.has("text");
        if text {
            vec!["image".into(), "text".into()]
        } else {
            vec!["image".into()]
        }
    }

    /// True when the catalogue says this model answers with a picture and
    /// nothing else. Such a model lives on the images endpoint: it has no
    /// conversation, no tools, and nothing to summarise.
    pub fn draws_only(&self) -> bool {
        self.output_modalities.has("image") && !self.output_modalities.has("text")
    }

    /// Swap stored references for what actually goes on the wire. The newest
    /// `budget` generated images become `data:` URLs; older ones leave a line
    /// of text naming their file and their entry goes. Images a user attached
    /// are already data URLs and are never dropped: that would change what the
    /// user asked.
    ///
    /// This runs on the per-request copy, so the conversation the caller holds
    /// — and the session file — keep every reference.
    fn hydrate_images(&mut self, all: &mut Vec<Message>, budget: u32) {
        if all.iter().all(|m| m.images.is_empty()) {
            return;
        }
        let mut left = budget;
        for m in all.iter_mut().rev() {
            if m.images.is_empty() {
                continue;
            }
            let mut kept = Vec::with_capacity(m.images.len());
            for entry in core::mem::take(&mut m.images) {
                if crate::image::is_data_url(&entry) {
                    kept.push(entry);
                    continue;
                }
                if left > 0
                    && let Some(url) = self.image_data_url(&entry)
                {
                    left -= 1;
                    kept.push(url);
                    continue;
                }
                let note = crate::image::omitted_note(&entry);
                if !m.content.contains(note.trim()) {
                    m.content.push_str(&note);
                }
            }
            m.images = kept;
        }
        if self.settings.images.echo == ImageEcho::User {
            move_assistant_images_to_user(all);
        }
    }

    /// A stored reference read back as a data URL, `None` when the file has
    /// gone.
    fn image_data_url(&mut self, entry: &str) -> Option<String> {
        if let Some((k, v)) = &self.image_cache
            && k == entry
        {
            return Some(v.clone());
        }
        let path = crate::image::resolve(entry)?;
        let bytes = std::fs::read(&path).ok()?;
        let url = crate::clipboard::data_url(&bytes);
        self.image_cache = Some((entry.to_string(), url.clone()));
        Some(url)
    }

    /// Write one generated image and return what stands for it in a message.
    fn save_image(&self, url: &str, seq: u32) -> Result<crate::image::Saved> {
        let dir = self
            .image_dir
            .clone()
            .ok_or_else(|| Error::Config("nowhere to put a generated image".into()))?;
        crate::image::save(&dir, url, seq, crate::image::MAX_IMAGE_BYTES)
    }

    /// True once the conversation fills `context.compact_at` percent of the window.
    pub fn over_threshold(&self) -> bool {
        let c = &self.settings.context;
        // Nothing to compact and nobody to write the summary: a model that
        // only draws would answer the summary request with a picture.
        !self.draws_only()
            && c.auto_compact
            && self.context_window > 0
            && self.context_tokens.saturating_mul(100)
                >= self.context_window * c.compact_at.min(100) as u64
    }

    /// Replace `messages` with a model-written summary. `focus` is extra
    /// guidance for the summary; empty for none.
    pub fn compact(
        &mut self,
        messages: &mut Vec<Message>,
        focus: &str,
        io: &dyn AgentIo,
    ) -> Result<()> {
        self.compact_inner(messages, focus, false, io)
    }

    /// [`Agent::compact`], announced as automatic: the window filled up rather
    /// than someone asking for it.
    pub fn auto_compact(&mut self, messages: &mut Vec<Message>, io: &dyn AgentIo) -> Result<()> {
        self.compact_inner(messages, "", true, io)
    }

    fn compact_inner(
        &mut self,
        messages: &mut Vec<Message>,
        focus: &str,
        auto: bool,
        io: &dyn AgentIo,
    ) -> Result<()> {
        if self.draws_only() {
            return Err(Error::Config(format!(
                "{} only draws; it cannot write a summary",
                self.settings.model.id
            )));
        }
        if messages.is_empty() {
            return Ok(());
        }
        io.emit(AgentEvent::Compacting { auto });
        let system = self.system_prompt();
        let mut all = Vec::with_capacity(messages.len() + 2);
        all.push(Message::system(system));
        all.extend(messages.iter().cloned());
        let mut ask = SUMMARY_PROMPT.to_string();
        if !focus.trim().is_empty() {
            ask.push_str("\nPay particular attention to: ");
            ask.push_str(focus.trim());
        }
        all.push(Message::user(ask));
        // No image bytes in a summary request: the notes name the files, which
        // is all the summary can usefully say about them, and some summarisers
        // will not take an image at all.
        self.hydrate_images(&mut all, 0);
        let cache_control = self.cache_control(prompt_bytes(&all[0].content, &all[1..], 0));
        let req = ChatRequest {
            model: self.settings.model.id.clone(),
            messages: all,
            tools: Vec::new(),
            max_tokens: Some(self.settings.context.summary_max_tokens),
            temperature: None,
            top_p: None,
            // A summariser is never asked to draw.
            modalities: Vec::new(),
            reasoning: None,
            provider: self.settings.model.provider.clone(),
            session_id: self.session_key(),
            cache_control,
        };
        let budget = self.settings.context.summary_max_tokens as u64;
        let mut acc = Accumulator::default();
        // Every delta would flood the UI channel, so the count goes out on a
        // tick instead.
        let mut ticked = Instant::now();
        io.emit(AgentEvent::CompactProgress { done: 0, budget });
        self.provider.stream(&req, self.cancel, &mut |ev| {
            acc.apply(&ev);
            if ticked.elapsed() >= PROGRESS_EVERY {
                ticked = Instant::now();
                io.emit(AgentEvent::CompactProgress {
                    done: estimate_tokens(acc.content.len()),
                    budget,
                });
            }
            !self.cancel.load(Ordering::Relaxed)
        })?;
        let acc = acc.finish();
        if acc.content.trim().is_empty() {
            return Err(Error::Http("empty summary".into()));
        }
        // A conversation resumed from disk has no usage figure behind it until
        // the model answers, so fall back to its size rather than report zero.
        let before = match self.context_tokens {
            0 => messages_tokens(messages),
            n => n,
        };
        let mut msg = summary_message(&acc.content);
        let after = estimate_tokens(msg.content.len());
        // A summary can describe a picture but cannot be edited into a new
        // one, so the newest generated image comes across the boundary with it.
        msg.images = last_generated_images(messages, 1);
        *messages = vec![msg];
        self.context_tokens = after;
        self.compactions += 1;
        io.emit(AgentEvent::Compacted {
            before,
            after,
            summary: acc.content,
        });
        Ok(())
    }

    /// Top-level `cache_control` for a request of this size, or `None` when the
    /// model caches on its own, caching is off, or the prompt is too small for
    /// the write to pay for itself. The provider keeps one breakpoint at the end
    /// of the prompt and advances it as the conversation grows.
    fn cache_control(&self, prompt_bytes: usize) -> Option<Value> {
        let c = &self.settings.context;
        if !c.cache {
            return None;
        }
        let id = self.settings.model.id.to_ascii_lowercase();
        if !c
            .cache_models
            .iter()
            .any(|p| !p.is_empty() && id.starts_with(&p.to_ascii_lowercase()))
        {
            return None;
        }
        if estimate_tokens(prompt_bytes) < c.cache_min_tokens {
            return None;
        }
        Some(match c.cache_ttl.trim() {
            "" | "5m" => serde_json::json!({"type": "ephemeral"}),
            ttl => serde_json::json!({"type": "ephemeral", "ttl": ttl}),
        })
    }

    /// Sticky-routing key for this conversation, or `None` when the session
    /// has no id. OpenRouter caps it at 256 characters.
    fn session_key(&self) -> Option<String> {
        let id = self.session_id.trim();
        if id.is_empty() {
            return None;
        }
        Some(id.chars().take(256).collect())
    }

    pub fn system_prompt(&mut self) -> String {
        let cwd = self.cwd.display().to_string();
        let os = std::env::consts::OS;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
        let date = date_string();
        let mut prompt = self.settings.prompt.system.clone();
        if !self.settings.prompt.append.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&self.settings.prompt.append);
        }
        let mut prompt = prompt
            .replace("{cwd}", &cwd)
            .replace("{os}", os)
            .replace("{shell}", &shell)
            .replace("{date}", &date);
        let found = crate::instructions::load(&self.cwd, &self.settings.prompt.instructions);
        prompt.push_str(&crate::instructions::render(&found));
        if self.settings.prompt.docs_hint {
            prompt.push_str("\n\n");
            prompt.push_str(&crate::docs::hint());
        }
        self.hooks.system_prompt(SystemPromptIn {
            prompt,
            cwd,
            os: os.into(),
            shell,
        })
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.registry.specs();
        for s in self.hooks.plugin_tool_specs() {
            if !self.settings.tools.disabled.contains(&s.function.name) {
                specs.retain(|x| x.function.name != s.function.name);
                specs.push(s);
            }
        }
        specs
    }

    /// Run one user turn. `messages` holds the conversation without a system
    /// message; assistant and tool messages are appended as produced.
    pub fn run_turn(
        &mut self,
        messages: &mut Vec<Message>,
        io: &dyn AgentIo,
    ) -> Result<TurnSummary> {
        let mut summary = TurnSummary::default();
        let system = self.system_prompt();
        let specs = self.tool_specs();
        let specs_bytes = tools_bytes(&specs);
        let cwd_str = self.cwd.display().to_string();
        // Room for a request that ran out of it while writing a tool call, and
        // how many times that has been paid for this turn.
        let mut room: Option<u32> = None;
        let mut cut_off = 0u32;

        loop {
            if self.cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break;
            }
            if summary.requests >= self.max_requests {
                io.emit(AgentEvent::Error(format!(
                    "stopped after {} requests",
                    summary.requests
                )));
                break;
            }
            summary.requests += 1;
            let turn = summary.requests;

            if self.over_threshold() {
                match self.auto_compact(messages, io) {
                    Ok(()) => {}
                    Err(Error::Cancelled) => {
                        summary.cancelled = true;
                        break;
                    }
                    Err(e) => io.emit(AgentEvent::Notice(format!("compaction failed: {e}"))),
                }
            }

            // Jobs that ended since the last request; the model hears about
            // them here instead of having to poll.
            if self.notices {
                for note in
                    crate::jobs::table().notices(crate::jobs::Audience::Model(self.agent_id))
                {
                    // A subagent's commands are its own business: the screen
                    // shows them in that agent's view, not in the conversation
                    // the user is having. The main loop's news goes out the
                    // other way, from the table straight to the transcript.
                    if self.child {
                        io.emit(AgentEvent::Notice(note.clone()));
                    }
                    messages.push(Message::user(format!("[background] {note}")));
                }
                for note in
                    crate::agents::table().notices(crate::agents::Audience::Model(self.agent_id))
                {
                    if self.child {
                        io.emit(AgentEvent::Notice(note.clone()));
                    }
                    messages.push(Message::user(format!("[agent] {note}")));
                }
            }
            // Anything said to this loop while it was working: an agent above
            // it, or the user watching it.
            if let Some(mailbox) = self.mailbox.as_ref() {
                for said in mailbox.take() {
                    messages.push(Message::user(said));
                }
            }

            let mut all = Vec::with_capacity(messages.len() + 2);
            all.push(Message::system(system.clone()));
            all.extend(messages.iter().cloned());
            // The nudge belongs to this request and no other: a reminder kept
            // in the conversation is paid for on every turn after it, and by
            // then it is quoting a plan that has moved on. It still goes last,
            // where it cannot disturb a cached prefix.
            if let Some(line) = self.plan_reminder().filter(|_| self.notices) {
                all.push(Message::user(line));
            }
            self.hydrate_images(&mut all, self.settings.images.history);
            let modalities = self.modalities();
            // A model asked for pictures only has no text channel to call a
            // tool on; declaring tools it cannot use invites an error.
            let text_out = modalities.is_empty() || modalities.iter().any(|m| m == "text");
            let bytes = prompt_bytes(&all[0].content, &all[1..], specs_bytes);
            let req = ChatRequest {
                model: self.settings.model.id.clone(),
                messages: all,
                tools: if text_out { specs.clone() } else { Vec::new() },
                max_tokens: room.or(self.settings.model.max_tokens),
                temperature: self.settings.model.temperature,
                top_p: self.settings.model.top_p,
                modalities: modalities.clone(),
                reasoning: self.settings.model.reasoning.clone(),
                provider: self.settings.model.provider.clone(),
                session_id: self.session_key(),
                cache_control: self.cache_control(bytes),
            };
            let mut req = self.hooks.before_request(req, turn);
            // A plugin built against an older ABI deserialises the request into
            // a struct with no `modalities` and drops it on the way back. Every
            // other field only costs money when that happens; this one turns
            // image output off without a word.
            if req.modalities.is_empty()
                && !modalities.is_empty()
                && req.model == self.settings.model.id
            {
                crate::debug!("a plugin dropped `modalities`; restoring it");
                req.modalities = modalities;
            }

            io.emit(AgentEvent::RequestStart { turn });
            let (acc, saved, failure) = self.stream_with_retry(&req, io);
            let mut acc = match failure {
                None => acc,
                Some(e) => {
                    // Some models report usage before the stream ends; count it
                    // rather than lose the spend along with the answer.
                    if acc.usage.total_tokens > 0 {
                        summary.usage.add(&acc.usage);
                        self.context_tokens = acc.usage.prompt_tokens + acc.usage.completion_tokens;
                        io.emit(AgentEvent::Usage(acc.usage));
                    }
                    if let Some(m) = partial_message(acc, saved) {
                        io.emit(AgentEvent::AssistantMessage(m.clone()));
                        messages.push(m);
                    }
                    if matches!(e, Error::Cancelled) {
                        summary.cancelled = true;
                        break;
                    }
                    io.emit(AgentEvent::Error(e.to_string()));
                    return Err(e);
                }
            };
            summary.usage.add(&acc.usage);
            self.context_tokens = acc.usage.prompt_tokens + acc.usage.completion_tokens;
            io.emit(AgentEvent::Usage(acc.usage));

            // A tool call that ran out of tokens halfway arrives as half a JSON
            // object. The model cannot see that it was cut off, so handing it
            // the parse error costs a turn and teaches it nothing: ask again
            // with room to finish, and leave nothing of the attempt behind.
            if cut_off < MAX_CUT_OFF
                && acc.finish_reason.as_deref() == Some("length")
                && let Some(call) = unfinished_call(&acc)
            {
                cut_off += 1;
                let had = room
                    .or(self.settings.model.max_tokens)
                    .unwrap_or(DEFAULT_ROOM);
                room = Some(had.saturating_mul(2).min(MAX_ROOM));
                crate::debug!(
                    "{call} ran out of room mid-call; asking again with {}",
                    room.unwrap_or(0)
                );
                io.emit(AgentEvent::Notice(format!(
                    "the reply ran out of room while writing a {call} call; asking again"
                )));
                continue;
            }
            // Every image is a file by now. Dropping the base64 here keeps it
            // out of the message, out of the session line and out of `--json`.
            acc.images.clear();
            let mut assistant = acc.clone().into_message();
            assistant.images = saved;
            io.emit(AgentEvent::AssistantMessage(assistant.clone()));
            messages.push(assistant.clone());

            if acc.tool_calls.is_empty() {
                break;
            }

            let calls = &acc.tool_calls;
            let mut i = 0;
            while i < calls.len() {
                if self.cancel.load(Ordering::Relaxed) {
                    summary.cancelled = true;
                    // The API requires a tool message for every call; record the cancel.
                    for call in &calls[i..] {
                        let m = Message::tool_result(&call.id, "[cancelled by user]");
                        io.emit(AgentEvent::ToolMessage(m.clone()));
                        messages.push(m);
                    }
                    break;
                }
                let n = self.batch_len(calls, i);
                crate::debug!("running {n} tool call(s) at once");
                summary.tool_calls += n as u32;
                for (call, result, dur) in self.run_batch(&calls[i..i + n], &cwd_str, io) {
                    let m = Message::tool_result(
                        &call.id,
                        if result.is_error {
                            format!("ERROR: {}", result.output)
                        } else {
                            result.output.clone()
                        },
                    );
                    io.emit(AgentEvent::ToolEnd {
                        call,
                        result,
                        duration_ms: dur,
                    });
                    io.emit(AgentEvent::ToolMessage(m.clone()));
                    messages.push(m);
                }
                i += n;
            }
            if summary.cancelled {
                break;
            }
        }

        if let Some(last) = messages.last().filter(|m| m.role == Role::Assistant) {
            let (patches, notices) = self.hooks.on_turn_end(OnTurnEndIn {
                message: last.clone(),
                usage: summary.usage,
                total_usage: summary.usage,
                tool_calls: summary.tool_calls,
            });
            for p in patches {
                io.emit(AgentEvent::SettingsPatch(p));
            }
            for n in notices {
                io.emit(AgentEvent::Notice(n));
            }
        }
        io.emit(AgentEvent::TurnEnd(summary.clone()));
        Ok(summary)
    }

    /// One line about a plan the model has left alone for a while. A plan it
    /// just changed needs no reminder: the tool reply already showed it.
    fn plan_reminder(&mut self) -> Option<String> {
        if !self.settings.context.plan_reminder {
            return None;
        }
        let store = crate::plan::store();
        let version = store.version();
        if version != self.plan_seen {
            self.plan_seen = version;
            self.plan_quiet = 0;
            return None;
        }
        self.plan_quiet += 1;
        if self.plan_quiet < self.settings.context.plan_reminder_every.max(1) {
            return None;
        }
        self.plan_quiet = 0;
        store.with(|plan| {
            let (done, total) = plan.counts();
            if total == 0 || done == total {
                return None;
            }
            Some(format!(
                "[plan] {}\nUpdate it with the plan tool as you go.",
                plan.summary()
            ))
        })
    }

    /// Hooks, deny rules and the permission prompt, before anything runs.
    fn gate_tool(&mut self, call: &ToolCall, cwd: &str, io: &dyn AgentIo) -> Gate {
        let mut call = call.clone();
        let (decision, patches) = self.hooks.before_tool(&call, cwd);
        for p in patches {
            io.emit(AgentEvent::SettingsPatch(p));
        }
        let mut must_ask = self.settings.permissions.mode == PermissionMode::Ask
            && self
                .settings
                .permissions
                .ask_for
                .contains(&call.function.name);
        let mut ask_reason = String::new();
        match decision {
            ToolDecision::Allow => {}
            ToolDecision::Deny { reason } => {
                io.emit(AgentEvent::ToolDenied {
                    call,
                    reason: reason.clone(),
                });
                return Gate::Refused(ToolResult::err(format!("denied by policy: {reason}")));
            }
            ToolDecision::Replace { arguments } => call.function.arguments = arguments,
            ToolDecision::Ask { reason } => {
                must_ask = true;
                ask_reason = reason;
            }
        }
        if call.function.name == "bash"
            && let Ok(args) = serde_json::from_str::<Value>(&call.function.arguments)
            && let Some(cmd) = args.get("command").and_then(|c| c.as_str())
        {
            if let Some(rule) = crate::policy::denied(cmd, &self.settings.permissions.deny) {
                let reason = format!("matches deny rule `{rule}`");
                io.emit(AgentEvent::ToolDenied {
                    call,
                    reason: reason.clone(),
                });
                return Gate::Refused(ToolResult::err(format!(
                    "denied by policy: {reason}. Ask the user to run it themselves if it is really needed."
                )));
            }
            // Refusing costs a turn. Waiting on a password prompt that nobody
            // will ever see costs the whole tool timeout and shows nothing at
            // the end of it, so this is the kinder of the two.
            if !self.settings.permissions.allow_sudo
                && let Some(name) = crate::policy::escalates(cmd)
            {
                let reason = format!("`{name}` is not allowed in this session");
                io.emit(AgentEvent::ToolDenied {
                    call,
                    reason: reason.clone(),
                });
                return Gate::Refused(ToolResult::err(format!(
                    "denied by policy: {reason}, because its password prompt would go to a \
                     terminal nobody is reading (permissions.allow_sudo is off). Do it without \
                     {name} if you can; `ah remote serve --sudo` allows it for sessions driven \
                     from a phone."
                )));
            }
        }
        // Some files decide what this program does next: whether anybody is
        // asked, which host the key is sent to, what runs at startup. Writing
        // one is asked about in every mode, and the prompt says what the file
        // is for rather than only naming it — approving `Write(config.toml)`
        // without being told it is the file that governs the asking is not
        // really approving anything.
        if let Some(what) = writes_its_own_rules(&call, cwd) {
            must_ask = true;
            ask_reason = what;
        }
        if must_ask && !io.ask_permission(&call, &ask_reason) {
            io.emit(AgentEvent::ToolDenied {
                call,
                reason: "user declined".into(),
            });
            return Gate::Refused(ToolResult::err("user declined to run this tool"));
        }
        Gate::Run(call)
    }

    /// How many calls starting at `at` may run at the same time. Always at
    /// least one; more only for read-only calls, so order still holds for
    /// anything that writes.
    fn batch_len(&self, calls: &[ToolCall], at: usize) -> usize {
        let t = &self.settings.tools;
        if !t.parallel {
            return 1;
        }
        let max = (t.max_parallel.max(1) as usize).min(calls.len() - at);
        let mut n = 0;
        while n < max && self.registry.is_parallel(&calls[at + n], t) {
            n += 1;
        }
        n.max(1)
    }

    /// Run one batch and return `(call, result, duration)` in call order. A
    /// batch of one runs on this thread; a longer batch is read-only calls, so
    /// they run at once on scoped threads and are stitched back into order.
    fn run_batch(
        &mut self,
        calls: &[ToolCall],
        cwd: &str,
        io: &dyn AgentIo,
    ) -> Vec<(ToolCall, ToolResult, u64)> {
        let mut gates = Vec::with_capacity(calls.len());
        for c in calls {
            gates.push(self.gate_tool(c, cwd, io));
        }
        let mut results: Vec<Option<(ToolResult, u64)>> = Vec::with_capacity(gates.len());
        let mut pending = Vec::new();
        for (i, g) in gates.iter().enumerate() {
            match g {
                Gate::Refused(r) => results.push(Some((r.clone(), 0))),
                Gate::Run(c) => {
                    io.emit(AgentEvent::ToolStart(c.clone()));
                    pending.push(i);
                    results.push(None);
                }
            }
        }
        if pending.len() == 1 {
            let i = pending[0];
            let Gate::Run(call) = &gates[i] else {
                unreachable!()
            };
            let start = Instant::now();
            // Plugin tools share one interpreter, so they never join a batch.
            let r = match self.hooks.plugin_tool(call, cwd) {
                Some(r) => r,
                None => self.registry.run(call, &self.tool_ctx(&IoAsker(io))),
            };
            results[i] = Some((r, start.elapsed().as_millis() as u64));
        } else if !pending.is_empty() {
            let registry = self.registry;
            let asker = IoAsker(io);
            let ctx = self.tool_ctx(&asker);
            let done: Vec<(usize, ToolResult, u64)> = std::thread::scope(|scope| {
                let handles: Vec<_> = pending
                    .iter()
                    .map(|&i| {
                        let Gate::Run(call) = &gates[i] else {
                            unreachable!()
                        };
                        let ctx = &ctx;
                        scope.spawn(move || {
                            let start = Instant::now();
                            let r = registry.run(call, ctx);
                            (i, r, start.elapsed().as_millis() as u64)
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            for (i, r, d) in done {
                results[i] = Some((r, d));
            }
        }
        let mut out = Vec::with_capacity(calls.len());
        for (i, gate) in gates.into_iter().enumerate() {
            let (result, dur) = results[i]
                .take()
                .unwrap_or_else(|| (ToolResult::err("tool did not run"), 0));
            match gate {
                Gate::Run(call) => {
                    let (result, patches) = self.hooks.after_tool(&call, result, dur);
                    for p in patches {
                        io.emit(AgentEvent::SettingsPatch(p));
                    }
                    out.push((call, result, dur));
                }
                Gate::Refused(_) => out.push((calls[i].clone(), result, dur)),
            }
        }
        out
    }

    fn tool_ctx<'c>(&'c self, ask: &'c IoAsker<'c>) -> ToolCtx<'c> {
        ToolCtx {
            cwd: &self.cwd,
            settings: &self.settings.tools,
            agent: self.agent_id,
            cancel: self.cancel,
            ask,
            spawn: self.spawner.as_deref(),
        }
    }

    /// Stream one request. A transient failure that produced nothing is
    /// retried; one that arrives after the model has started talking is not,
    /// because the retry would pay for those tokens a second time. Either way
    /// what did arrive comes back beside the error, for the caller to keep.
    fn stream_with_retry(
        &self,
        req: &ChatRequest,
        io: &dyn AgentIo,
    ) -> (Accumulator, Vec<String>, Option<Error>) {
        let mut attempt = 0u32;
        loop {
            let mut acc = Accumulator::default();
            let mut got_any = false;
            // References to the images this request produced. A retry only
            // happens when nothing arrived, and an image sets `got_any`, so
            // this is always empty on the way round again.
            let mut saved: Vec<String> = Vec::new();
            let res = self.provider.stream(req, self.cancel, &mut |ev| {
                acc.apply(&ev);
                match ev {
                    StreamEvent::Text(t) => {
                        got_any = true;
                        io.emit(AgentEvent::Text(t))
                    }
                    StreamEvent::Reasoning(t) => {
                        got_any = true;
                        io.emit(AgentEvent::Reasoning(t))
                    }
                    StreamEvent::Image(url) => {
                        // Paid for the moment it arrived: never retried.
                        got_any = true;
                        match self.save_image(&url, saved.len() as u32) {
                            Ok(s) => {
                                io.emit(AgentEvent::Image {
                                    path: s.path,
                                    mime: s.mime,
                                    width: s.width,
                                    height: s.height,
                                    bytes: s.bytes,
                                });
                                saved.push(s.reference);
                            }
                            Err(e) => io.emit(AgentEvent::Notice(format!(
                                "could not save the generated image: {e}"
                            ))),
                        }
                    }
                    StreamEvent::ToolCallDelta { .. } => got_any = true,
                    StreamEvent::Usage(_) | StreamEvent::Finish(_) => {}
                }
                !self.cancel.load(Ordering::Relaxed)
            });
            match res {
                Ok(()) => return (acc.finish(), saved, None),
                Err(Error::Cancelled) => return (acc.finish(), saved, Some(Error::Cancelled)),
                Err(e) if attempt < 3 && !got_any && is_transient(&e) => {
                    attempt += 1;
                    let wait = Duration::from_millis(500 * (1u64 << attempt));
                    io.emit(AgentEvent::Retry {
                        attempt,
                        wait_ms: wait.as_millis() as u64,
                        error: e.to_string(),
                    });
                    let deadline = Instant::now() + wait;
                    while Instant::now() < deadline {
                        if self.cancel.load(Ordering::Relaxed) {
                            return (acc.finish(), saved, Some(Error::Cancelled));
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(e) => return (acc.finish(), saved, Some(e)),
            }
        }
    }
}

/// What is worth keeping from a request that ended part way: the text the
/// model had already written, said to be cut short. Tool calls are dropped —
/// one cut off mid-stream has truncated arguments and no result to pair with,
/// and every call shown to the API needs one. Reasoning alone is not enough to
/// keep: an assistant message with no content and no picture is not one the
/// next request can carry.
fn partial_message(acc: Accumulator, images: Vec<String>) -> Option<Message> {
    let text = acc.content.trim_end();
    if text.is_empty() && images.is_empty() {
        return None;
    }
    // A picture that arrived before the Esc was paid for and is on disk, so it
    // is worth keeping even with nothing said around it.
    let mut m = Message::assistant(if text.is_empty() {
        "[cut short]".to_string()
    } else {
        format!("{text}\n\n[cut short]")
    });
    if !acc.reasoning.is_empty() {
        m.reasoning = Some(acc.reasoning);
    }
    m.images = images;
    Some(m)
}

/// Move an assistant's generated images onto a user message just after it, for
/// a provider that refuses images on an assistant message. An assistant
/// message with tool calls is left alone: its `tool` results have to follow it
/// immediately, and a message wedged in between is a hard error.
fn move_assistant_images_to_user(all: &mut Vec<Message>) {
    let mut i = 0;
    while i < all.len() {
        if all[i].role != Role::Assistant
            || all[i].images.is_empty()
            || !all[i].tool_calls.is_empty()
        {
            i += 1;
            continue;
        }
        let images = core::mem::take(&mut all[i].images);
        match all.get_mut(i + 1) {
            Some(next) if next.role == Role::User => {
                let mut moved = images;
                moved.append(&mut next.images);
                next.images = moved;
                i += 2;
            }
            _ => {
                all.insert(
                    i + 1,
                    Message::user_with_images("[the image you generated]", images),
                );
                i += 2;
            }
        }
    }
}

/// The `n` most recent generated images in a conversation, newest last. Only
/// references count: an image a user attached belongs to the message they
/// wrote, which the summary has replaced.
fn last_generated_images(messages: &[Message], n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in messages.iter().rev() {
        if m.role != Role::Assistant {
            continue;
        }
        for entry in m.images.iter().rev() {
            if crate::image::is_data_url(entry) {
                continue;
            }
            out.push(entry.clone());
            if out.len() == n {
                out.reverse();
                return out;
            }
        }
    }
    out.reverse();
    out
}

fn is_transient(e: &Error) -> bool {
    match e {
        Error::Api { status, .. } => matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504),
        Error::Io(_) | Error::Http(_) => true,
        _ => false,
    }
}

/// `YYYY-MM-DD` from the system clock.
pub fn date_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // civil_from_days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Scripted provider: each call pops the next response script.
    pub struct MockProvider {
        pub scripts: Mutex<Vec<Vec<StreamEvent>>>,
        pub requests: Mutex<Vec<ChatRequest>>,
    }

    impl MockProvider {
        pub fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                scripts: Mutex::new(scripts),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn stream(
            &self,
            req: &ChatRequest,
            _cancel: &AtomicBool,
            on_event: crate::provider::OnEvent<'_>,
        ) -> Result<()> {
            self.requests.lock().unwrap().push(req.clone());
            let mut s = self.scripts.lock().unwrap();
            if s.is_empty() {
                return Err(Error::Api {
                    status: 500,
                    message: "script exhausted".into(),
                });
            }
            for ev in s.remove(0) {
                if !on_event(ev) {
                    return Err(Error::Cancelled);
                }
            }
            Ok(())
        }
    }

    #[derive(Default)]
    pub struct RecordingIo {
        pub events: Mutex<Vec<AgentEvent>>,
        pub allow: bool,
        /// What a question is answered with; nothing means nobody to ask.
        pub answer: Option<Reply>,
        /// The questions that were put.
        pub asked: Mutex<Vec<Ask>>,
    }

    impl AgentIo for RecordingIo {
        fn emit(&self, ev: AgentEvent) {
            self.events.lock().unwrap().push(ev);
        }
        fn ask_permission(&self, _call: &ToolCall, _reason: &str) -> bool {
            self.allow
        }
        fn ask_user(&self, ask: &Ask) -> Reply {
            self.asked.lock().unwrap().push(ask.clone());
            self.answer.clone().unwrap_or(Reply::Unavailable)
        }
    }
}

/// Whether a tool call writes one of the files that decide how this program
/// behaves, and what to say about it if so.
///
/// The model reaches these the same way it reaches any other path, and a write
/// to one of them is not an edit, it is a change to the rules the next turn
/// runs under. `~/.config/ah/config.toml` holds `permissions.mode`;
/// `credentials.toml` holds the key; the plugin directories hold programs that
/// run before the first request.
fn writes_its_own_rules(call: &ToolCall, cwd: &str) -> Option<String> {
    if !matches!(
        call.function.name.as_str(),
        "write_file" | "edit_file" | "multi_edit"
    ) {
        return None;
    }
    let args: Value = serde_json::from_str(&call.function.arguments).ok()?;
    let raw = args.get("path").and_then(|p| p.as_str())?;
    let path = crate::tools::resolve_path(std::path::Path::new(cwd), raw);
    // Compared as written rather than canonicalised: the file may not exist
    // yet, and a symlink into one of these directories is answered by the
    // directory check below rather than by resolving it.
    let config = crate::paths::config_dir();
    let data = crate::paths::data_dir();
    if path.starts_with(&config) || path.starts_with(&data) {
        let what = if path == crate::paths::credentials_file() {
            "this is the file your API key and pairing code are kept in"
        } else if path.parent().is_some_and(|p| p.ends_with("plugins")) {
            "a program in here runs before the first request of every session"
        } else {
            "this is the config that decides which tools are asked about, which host the key is sent to, and what runs at startup"
        };
        return Some(what.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::tools::Tool;
    use std::path::Path;

    fn tool_call_script(name: &str, args: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name: Some(name.into()),
                arguments: args.into(),
            },
            StreamEvent::Finish("tool_calls".into()),
            StreamEvent::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                cost: 0.001,
                ..Usage::default()
            }),
        ]
    }

    /// The smallest real PNG, as a data URL, for a scripted image reply.
    fn png_url() -> String {
        let mut v = Vec::new();
        v.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        v.extend_from_slice(&[0, 0, 0, 13]);
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0, 0x90, 0x77, 0x53, 0xDE]);
        format!("data:image/png;base64,{}", crate::clipboard::base64(&v))
    }

    fn image_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ah-agent-image-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// An agent wired to a scripted provider, with somewhere to put pictures.
    fn image_agent<'a>(
        provider: &'a MockProvider,
        registry: &'a Registry,
        hooks: &'a mut NoHooks,
        settings: &'a Settings,
        cancel: &'a AtomicBool,
        dir: &Path,
        modalities: &[&str],
    ) -> Agent<'a> {
        let mut a = Agent::new(
            provider,
            registry,
            hooks,
            settings,
            std::env::current_dir().unwrap(),
            cancel,
        );
        a.notices = false;
        a.image_dir = Some(dir.to_path_buf());
        a.output_modalities = crate::models::Modalities::from_names(modalities.iter().copied());
        a
    }

    #[test]
    fn a_generated_image_is_written_and_the_message_only_names_it() {
        let dir = image_dir("saved");
        let provider = MockProvider::new(vec![vec![
            StreamEvent::Text("here".into()),
            StreamEvent::Image(png_url()),
            StreamEvent::Finish("stop".into()),
        ]]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = image_agent(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            &cancel,
            &dir,
            &["image", "text"],
        );
        let mut messages = vec![Message::user("draw a cat")];
        let io = RecordingIo::default();
        agent.run_turn(&mut messages, &io).unwrap();

        let events = io.events.lock().unwrap();
        let img = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Image {
                    path,
                    width,
                    height,
                    ..
                } => Some((path.clone(), *width, *height)),
                _ => None,
            })
            .expect("an image event");
        assert_eq!((img.1, img.2), (1, 1));
        assert!(img.0.is_file());
        // The conversation names the file and carries none of its bytes.
        let stored = &messages[1].images;
        assert_eq!(stored.len(), 1);
        assert!(stored[0].starts_with(crate::image::SCHEME) || stored[0].starts_with("file://"));
        assert!(!messages[1].images[0].contains("base64,"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn images_are_asked_for_only_when_the_model_draws() {
        let dir = image_dir("modalities");
        let cases: [(&[&str], ImageOutput, &[&str]); 5] = [
            (&["image", "text"], ImageOutput::Auto, &["image", "text"]),
            (&["text"], ImageOutput::Auto, &[]),
            (&["image"], ImageOutput::Auto, &["image"]),
            // A catalogue that has not landed yet says nothing, so `auto`
            // asks for nothing and `always` asks anyway.
            (&[], ImageOutput::Auto, &[]),
            (&[], ImageOutput::Always, &["image", "text"]),
        ];
        for (out_mods, output, want) in cases {
            let provider = MockProvider::new(vec![vec![
                StreamEvent::Text("ok".into()),
                StreamEvent::Finish("stop".into()),
            ]]);
            let mut settings = Settings::default();
            settings.images.output = output;
            let registry = Registry::builtins(&settings.tools);
            let mut hooks = NoHooks;
            let cancel = AtomicBool::new(false);
            let mut agent = image_agent(
                &provider, &registry, &mut hooks, &settings, &cancel, &dir, out_mods,
            );
            let mut messages = vec![Message::user("hello")];
            agent
                .run_turn(&mut messages, &RecordingIo::default())
                .unwrap();
            let reqs = provider.requests.lock().unwrap();
            assert_eq!(
                reqs[0].modalities, want,
                "modalities {out_mods:?} {output:?}"
            );
            // A model with no text channel has nothing to call a tool with.
            if want == ["image"] {
                assert!(reqs[0].tools.is_empty());
            }
        }
    }

    #[test]
    fn only_the_newest_image_goes_back_and_the_rest_stay_put() {
        let dir = image_dir("history");
        let url = png_url();
        let provider = MockProvider::new(vec![
            vec![
                StreamEvent::Image(url.clone()),
                StreamEvent::Finish("stop".into()),
            ],
            vec![
                StreamEvent::Image(url.clone()),
                StreamEvent::Finish("stop".into()),
            ],
            vec![
                StreamEvent::Text("done".into()),
                StreamEvent::Finish("stop".into()),
            ],
        ]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = image_agent(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            &cancel,
            &dir,
            &["image", "text"],
        );
        let mut messages = vec![Message::user("draw a cat")];
        agent
            .run_turn(&mut messages, &RecordingIo::default())
            .unwrap();
        messages.push(Message::user("now a dog"));
        agent
            .run_turn(&mut messages, &RecordingIo::default())
            .unwrap();
        messages.push(Message::user("describe them"));
        agent
            .run_turn(&mut messages, &RecordingIo::default())
            .unwrap();

        let reqs = provider.requests.lock().unwrap();
        let third = &reqs[2].messages;
        let sent: Vec<&Message> = third.iter().filter(|m| !m.images.is_empty()).collect();
        // One picture on the wire: the newest.
        assert_eq!(sent.len(), 1);
        assert!(sent[0].images[0].starts_with("data:image/png;base64,"));
        // The older one left a note naming its file, and the note is the same
        // on every later request, so the cached prefix still matches.
        let older = third
            .iter()
            .find(|m| m.role == Role::Assistant && m.images.is_empty())
            .unwrap();
        assert!(older.content.contains("not resent"));
        // It names the file it left behind, which is what the model can ask
        // about later.
        let first_ref = messages
            .iter()
            .filter(|m| m.role == Role::Assistant && !m.images.is_empty())
            .map(|m| m.images[0].clone())
            .next()
            .unwrap();
        assert!(older.content.contains(&first_ref));
        // The conversation the caller holds still has both references.
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.role == Role::Assistant && !m.images.is_empty())
                .count(),
            2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_that_cannot_be_saved_says_so_and_keeps_nothing() {
        let dir = image_dir("nowhere");
        let provider = MockProvider::new(vec![vec![
            StreamEvent::Text("here".into()),
            StreamEvent::Image("data:image/png;base64,!!!!".into()),
            StreamEvent::Finish("stop".into()),
        ]]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = image_agent(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            &cancel,
            &dir,
            &["image", "text"],
        );
        let mut messages = vec![Message::user("draw a cat")];
        let io = RecordingIo::default();
        agent.run_turn(&mut messages, &io).unwrap();
        assert!(messages[1].images.is_empty());
        assert!(io.events.lock().unwrap().iter().any(|e| matches!(
            e,
            AgentEvent::Notice(n) if n.contains("could not save")
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_user_echo_moves_a_picture_off_the_assistant_message() {
        let url = "data:image/png;base64,AA==".to_string();
        let mut plain = Message::assistant("here it is");
        plain.images = vec![url.clone()];
        // An assistant message with tool calls is left alone: its tool results
        // have to follow it, and a message wedged in between is an error.
        let mut with_call = Message::assistant("");
        with_call.images = vec![url.clone()];
        with_call.tool_calls = vec![ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: ToolFunction {
                name: "bash".into(),
                arguments: "{}".into(),
            },
        }];
        let mut all = vec![
            Message::user("draw"),
            plain,
            with_call,
            Message::tool_result("c1", "ok"),
        ];
        move_assistant_images_to_user(&mut all);
        assert!(all[1].images.is_empty());
        assert_eq!(all[2].role, Role::User);
        assert_eq!(all[2].images, vec![url.clone()]);
        assert_eq!(all[3].images, vec![url]);
        assert_eq!(all[4].role, Role::Tool);
    }

    #[test]
    fn a_prompt_counts_an_image_as_tokens_not_as_base64() {
        let mut with = Message::assistant("hi");
        with.images = vec!["ah-image:s/1-0.png".into()];
        let plain = Message::assistant("hi");
        let grew = prompt_bytes("", std::slice::from_ref(&with), 0)
            - prompt_bytes("", std::slice::from_ref(&plain), 0);
        assert_eq!(grew, crate::image::IMAGE_TOKENS * 4);
    }

    #[test]
    fn a_summary_message_says_it_is_one() {
        let m = summary_message("  did things  ");
        assert_eq!(summary_text(&m.content), Some("did things"));
        // An ordinary message is not mistaken for a summary.
        assert_eq!(summary_text("hello"), None);
    }

    #[test]
    fn compacts_mid_turn_when_over_threshold() {
        let big = Usage {
            prompt_tokens: 95,
            completion_tokens: 2,
            total_tokens: 97,
            cost: 0.0,
            ..Usage::default()
        };
        let provider = MockProvider::new(vec![
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("c1".into()),
                    name: Some("bash".into()),
                    arguments: "{\"command\":\"echo hello\"}".into(),
                },
                StreamEvent::Finish("tool_calls".into()),
                StreamEvent::Usage(big),
            ],
            vec![
                StreamEvent::Text("SUMMARY".into()),
                StreamEvent::Finish("stop".into()),
            ],
            vec![
                StreamEvent::Text("done".into()),
                StreamEvent::Finish("stop".into()),
            ],
        ]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        agent.session_id = "sess-1".into();
        agent.context_window = 100;
        let mut messages = vec![Message::user("say hi via bash")];
        let io = RecordingIo::default();
        agent.run_turn(&mut messages, &io).unwrap();
        assert_eq!(agent.compactions, 1);
        // summary user message, then the final assistant reply
        assert_eq!(messages.len(), 2);
        assert!(messages[0].content.contains("SUMMARY"));
        assert_eq!(messages[1].content, "done");
        let reqs = provider.requests.lock().unwrap();
        assert!(reqs[1].tools.is_empty());
        // Including the summary request, which is part of the same session.
        assert!(
            reqs.iter()
                .all(|r| r.session_id.as_deref() == Some("sess-1"))
        );
        assert!(
            reqs[1]
                .messages
                .last()
                .unwrap()
                .content
                .starts_with("Summarise")
        );
        assert!(
            io.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::Compacted { before: 97, .. }))
        );
    }

    /// A session resumed from disk has messages but no usage figure yet, so
    /// the compaction has to size the conversation itself.
    #[test]
    fn a_resumed_conversation_is_sized_before_it_is_compacted() {
        let provider = MockProvider::new(vec![vec![
            StreamEvent::Text("SUMMARY".into()),
            StreamEvent::Finish("stop".into()),
        ]]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let mut messages = vec![
            Message::user("x".repeat(400)),
            Message::assistant("y".repeat(400)),
        ];
        let io = RecordingIo::default();
        assert_eq!(agent.context_tokens, 0);
        agent.compact(&mut messages, "", &io).unwrap();
        let before = io
            .events
            .lock()
            .unwrap()
            .iter()
            .find_map(|e| match e {
                AgentEvent::Compacted { before, .. } => Some(*before),
                _ => None,
            })
            .expect("no compacted event");
        assert_eq!(before, 200);
    }

    #[test]
    fn writing_the_rules_is_asked_about_even_in_auto_mode() {
        let call = |path: &str| ToolCall {
            id: "1".into(),
            kind: "function".into(),
            function: ah_abi::ToolFunction {
                name: "write_file".into(),
                arguments: serde_json::json!({"path": path, "content": "x"}).to_string(),
            },
        };
        let cwd = "/tmp";
        let config = crate::paths::user_config_file();
        let creds = crate::paths::credentials_file();
        assert!(writes_its_own_rules(&call(config.to_str().unwrap()), cwd).is_some());
        let about_keys = writes_its_own_rules(&call(creds.to_str().unwrap()), cwd).unwrap();
        assert!(about_keys.contains("API key"), "{about_keys}");
        // Ordinary work is left alone.
        assert!(writes_its_own_rules(&call("src/main.rs"), cwd).is_none());
        assert!(writes_its_own_rules(&call("/tmp/notes.md"), cwd).is_none());
        // And reading is not writing.
        let mut read = call(config.to_str().unwrap());
        read.function.name = "read_file".into();
        assert!(writes_its_own_rules(&read, cwd).is_none());
    }

    #[test]
    fn deny_rules_apply_in_auto_mode() {
        let provider = MockProvider::new(vec![
            tool_call_script("bash", "{\"command\":\"git reset --hard HEAD~3\"}"),
            vec![StreamEvent::Text("ok".into())],
        ]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let mut messages = vec![Message::user("x")];
        let io = RecordingIo::default();
        agent.run_turn(&mut messages, &io).unwrap();
        assert!(messages[2].content.contains("deny rule `git reset --hard`"));
    }

    #[test]
    fn a_session_with_no_keyboard_refuses_sudo_instead_of_waiting_for_it() {
        let provider = MockProvider::new(vec![
            tool_call_script("bash", "{\"command\":\"sudo systemctl restart nginx\"}"),
            vec![StreamEvent::Text("ok".into())],
        ]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let mut messages = vec![Message::user("x")];
        let io = RecordingIo::default();
        agent.run_turn(&mut messages, &io).unwrap();
        assert!(
            messages[2].content.contains("`sudo` is not allowed"),
            "{}",
            messages[2].content
        );
        // And it says what to do about it, since the model is the one reading.
        assert!(messages[2].content.contains("remote serve --sudo"));
    }

    #[test]
    fn nothing_runs_unasked_by_default() {
        let p = Settings::default().permissions;
        assert_eq!(p.mode, PermissionMode::Ask);
        assert!(!p.allow_sudo);
        for tool in ["bash", "write_file", "edit_file"] {
            assert!(p.ask_for.iter().any(|t| t == tool), "{tool} runs unasked");
        }
    }

    #[test]
    fn two_round_tool_turn() {
        let provider = MockProvider::new(vec![
            tool_call_script("bash", "{\"command\":\"echo hello\"}"),
            vec![
                StreamEvent::Text("done".into()),
                StreamEvent::Finish("stop".into()),
            ],
        ]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let mut messages = vec![Message::user("say hi via bash")];
        let io = RecordingIo {
            allow: true,
            ..Default::default()
        };
        let summary = agent.run_turn(&mut messages, &io).unwrap();
        assert_eq!(summary.requests, 2);
        assert_eq!(summary.tool_calls, 1);
        assert_eq!(summary.usage.total_tokens, 12);
        // user, assistant(tool_calls), tool, assistant
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].role, Role::Tool);
        assert!(messages[2].content.contains("hello"));
        assert_eq!(messages[3].content, "done");
        let reqs = provider.requests.lock().unwrap();
        assert_eq!(reqs[1].messages[0].role, Role::System);
        assert!(reqs[1].messages[0].content.contains("Working directory"));
        assert!(reqs[0].tools.iter().any(|t| t.function.name == "edit_file"));
    }

    #[test]
    fn a_question_reaches_the_user_and_the_answer_reaches_the_model() {
        let provider = MockProvider::new(vec![
            tool_call_script(
                "ask_user",
                r#"{"questions":[{"question":"Which store?","options":["sqlite","postgres"]}]}"#,
            ),
            vec![StreamEvent::Text("sqlite it is".into())],
        ]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let mut messages = vec![Message::user("pick a store")];
        let io = RecordingIo {
            answer: Some(Reply::Answered {
                answers: vec![Answer {
                    picked: vec!["sqlite".into()],
                    note: "one writer".into(),
                }],
            }),
            ..Default::default()
        };
        agent.run_turn(&mut messages, &io).unwrap();
        let asked = io.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].questions[0].question, "Which store?");
        assert_eq!(asked[0].questions[0].options.len(), 2);
        assert_eq!(
            messages[2].content,
            "Which store?\nanswer: sqlite\nnote: one writer\n"
        );
    }

    #[test]
    fn a_run_with_nobody_at_the_keyboard_tells_the_model_to_decide() {
        let provider = MockProvider::new(vec![
            tool_call_script("ask_user", r#"{"questions":["Which store?"]}"#),
            vec![StreamEvent::Text("going with sqlite".into())],
        ]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let mut messages = vec![Message::user("pick a store")];
        let io = RecordingIo::default();
        agent.run_turn(&mut messages, &io).unwrap();
        assert!(
            messages[2].content.contains("nobody to ask"),
            "{:?}",
            messages[2].content
        );
    }

    #[test]
    fn ask_mode_denies_when_user_declines() {
        let provider = MockProvider::new(vec![
            tool_call_script("bash", "{\"command\":\"rm -rf ./scratch\"}"),
            vec![StreamEvent::Text("ok".into())],
        ]);
        let mut settings = Settings::default();
        settings.permissions.mode = PermissionMode::Ask;
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let mut messages = vec![Message::user("x")];
        let io = RecordingIo {
            allow: false,
            ..Default::default()
        };
        agent.run_turn(&mut messages, &io).unwrap();
        assert!(messages[2].content.contains("declined"));
        assert!(
            io.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolDenied { .. }))
        );
    }

    #[test]
    fn transient_error_retries_then_succeeds() {
        struct Flaky(std::sync::atomic::AtomicU32);
        impl Provider for Flaky {
            fn name(&self) -> &str {
                "flaky"
            }
            fn stream(
                &self,
                _r: &ChatRequest,
                _c: &AtomicBool,
                on: crate::provider::OnEvent<'_>,
            ) -> Result<()> {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(Error::Api {
                        status: 503,
                        message: "busy".into(),
                    });
                }
                on(StreamEvent::Text("fine".into()));
                Ok(())
            }
        }
        let provider = Flaky(Default::default());
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            ".".into(),
            &cancel,
        );
        let mut messages = vec![Message::user("x")];
        let io = RecordingIo::default();
        agent.notices = false;
        agent.run_turn(&mut messages, &io).unwrap();
        assert_eq!(messages[1].content, "fine");
        assert!(
            io.events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::Retry { .. }))
        );
    }

    #[test]
    fn a_stream_cut_short_keeps_the_text_it_was_paid_for() {
        struct Cuts;
        impl Provider for Cuts {
            fn name(&self) -> &str {
                "cuts"
            }
            fn stream(
                &self,
                _r: &ChatRequest,
                _c: &AtomicBool,
                on: crate::provider::OnEvent<'_>,
            ) -> Result<()> {
                on(StreamEvent::Text("half an answer".into()));
                // A call the model never finished spelling out.
                on(StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("c1".into()),
                    name: Some("bash".into()),
                    arguments: "{\"comm".into(),
                });
                Err(Error::Http("connection reset".into()))
            }
        }
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(&Cuts, &registry, &mut hooks, &settings, ".".into(), &cancel);
        agent.notices = false;
        let mut messages = vec![Message::user("x")];
        let io = RecordingIo::default();
        let err = agent.run_turn(&mut messages, &io).unwrap_err();
        assert!(matches!(err, Error::Http(_)), "{err}");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, Role::Assistant);
        assert!(messages[1].content.starts_with("half an answer"));
        assert!(messages[1].content.contains("[cut short]"));
        // A kept call would have no result to pair with, and the API insists.
        assert!(messages[1].tool_calls.is_empty());
    }

    #[test]
    fn a_cancelled_answer_is_kept_as_well() {
        struct Interrupted<'a>(&'a AtomicBool);
        impl Provider for Interrupted<'_> {
            fn name(&self) -> &str {
                "interrupted"
            }
            fn stream(
                &self,
                _r: &ChatRequest,
                _c: &AtomicBool,
                on: crate::provider::OnEvent<'_>,
            ) -> Result<()> {
                on(StreamEvent::Text("half an ans".into()));
                // Esc, part way through the next word.
                self.0.store(true, Ordering::Relaxed);
                if !on(StreamEvent::Text("wer".into())) {
                    return Err(Error::Cancelled);
                }
                Ok(())
            }
        }
        let cancel = AtomicBool::new(false);
        let provider = Interrupted(&cancel);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            ".".into(),
            &cancel,
        );
        agent.notices = false;
        let mut messages = vec![Message::user("x")];
        let io = RecordingIo::default();
        let summary = agent.run_turn(&mut messages, &io).unwrap();
        assert!(summary.cancelled);
        assert_eq!(messages.len(), 2);
        assert!(
            messages[1].content.starts_with("half an answer"),
            "{}",
            messages[1].content
        );
    }

    fn cache_probe(model: &str, big: bool, tweak: impl FnOnce(&mut Settings)) -> Option<Value> {
        let provider = MockProvider::new(vec![vec![
            StreamEvent::Text("ok".into()),
            StreamEvent::Finish("stop".into()),
        ]]);
        let mut settings = Settings::default();
        settings.model.id = model.into();
        tweak(&mut settings);
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let text = if big { "x".repeat(40_000) } else { "hi".into() };
        let mut messages = vec![Message::user(text)];
        agent
            .run_turn(&mut messages, &RecordingIo::default())
            .unwrap();
        let reqs = provider.requests.lock().unwrap();
        reqs[0].cache_control.clone()
    }

    #[test]
    fn cache_breakpoint_only_where_it_pays() {
        // Models that cache on their own are left alone.
        assert_eq!(cache_probe("openai/gpt-5", true, |_| {}), None);
        // Below the minimum a cache write costs more than it saves.
        assert_eq!(
            cache_probe("anthropic/claude-sonnet-5", false, |_| {}),
            None
        );
        assert_eq!(
            cache_probe("anthropic/claude-sonnet-5", true, |_| {}),
            Some(serde_json::json!({"type": "ephemeral"}))
        );
        assert_eq!(
            cache_probe("anthropic/claude-sonnet-5", true, |s| s.context.cache =
                false),
            None
        );
        assert_eq!(
            cache_probe("anthropic/claude-sonnet-5", true, |s| s.context.cache_ttl =
                "1h".into()),
            Some(serde_json::json!({"type": "ephemeral", "ttl": "1h"}))
        );
    }

    #[test]
    fn the_session_id_rides_along_as_the_routing_key() {
        let reply = || {
            vec![
                StreamEvent::Text("hi".into()),
                StreamEvent::Finish("stop".into()),
            ]
        };
        let provider = MockProvider::new(vec![reply(), reply()]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            ".".into(),
            &cancel,
        );
        agent.notices = false;
        let io = RecordingIo::default();
        // A session-less run asks for no sticky routing at all.
        agent.run_turn(&mut vec![Message::user("x")], &io).unwrap();
        agent.session_id = "1a2b3c".into();
        agent.run_turn(&mut vec![Message::user("x")], &io).unwrap();
        let reqs = provider.requests.lock().unwrap();
        assert_eq!(reqs[0].session_id, None);
        assert_eq!(reqs[1].session_id.as_deref(), Some("1a2b3c"));
        // What matters is the body on the wire, not just the struct.
        let sent = serde_json::to_value(&reqs[1]).unwrap();
        assert_eq!(sent["session_id"], "1a2b3c");
        let none = serde_json::to_value(&reqs[0]).unwrap();
        assert!(none.get("session_id").is_none());
    }

    #[test]
    fn prompt_bytes_counts_tools_and_tool_calls() {
        let mut m = Message::assistant("hi");
        m.tool_calls = vec![ToolCall::new("1", "bash", "{\"command\":\"ls\"}")];
        let specs = vec![ToolSpec::new(
            "bash",
            "run a command",
            serde_json::json!({}),
        )];
        let n = prompt_bytes("system", &[m], tools_bytes(&specs));
        assert_eq!(n, 6 + 2 + 4 + 16 + 4 + 13 + 2);
    }

    /// A tool that only finishes when a second copy of itself is running, so a
    /// serial loop would time out instead of reporting the meeting.
    struct Rendezvous {
        state: std::sync::Arc<(std::sync::Mutex<usize>, std::sync::Condvar)>,
        met: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Tool for Rendezvous {
        fn spec(&self) -> ToolSpec {
            ToolSpec::new("peek", "test tool", serde_json::json!({"type": "object"}))
        }
        fn parallel(&self, _args: &Value, _settings: &ah_abi::ToolSettings) -> bool {
            true
        }
        fn run(&self, _args: &Value, _ctx: &ToolCtx<'_>) -> ToolResult {
            let (lock, cv) = &*self.state;
            let mut n = lock.lock().unwrap();
            *n += 1;
            if *n >= 2 {
                self.met.store(true, Ordering::Relaxed);
                cv.notify_all();
                return ToolResult::ok("peeked");
            }
            let (_n, wait) = cv
                .wait_timeout_while(n, Duration::from_secs(2), |n| *n < 2)
                .unwrap();
            if !wait.timed_out() {
                self.met.store(true, Ordering::Relaxed);
            }
            ToolResult::ok("peeked")
        }
    }

    fn two_peeks() -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("a".into()),
                name: Some("peek".into()),
                arguments: "{}".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                id: Some("b".into()),
                name: Some("peek".into()),
                arguments: "{}".into(),
            },
            StreamEvent::Finish("tool_calls".into()),
        ]
    }

    #[test]
    fn read_only_calls_run_at_the_same_time() {
        let met = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut registry = Registry::new();
        registry.register(Box::new(Rendezvous {
            state: std::sync::Arc::new(Default::default()),
            met: met.clone(),
        }));
        let provider = MockProvider::new(vec![
            two_peeks(),
            vec![
                StreamEvent::Text("done".into()),
                StreamEvent::Finish("stop".into()),
            ],
        ]);
        let settings = Settings::default();
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let mut messages = vec![Message::user("look twice")];
        let io = RecordingIo::default();
        agent.run_turn(&mut messages, &io).unwrap();
        assert!(met.load(Ordering::Relaxed), "calls did not overlap");
        // Both results come back, in call order.
        let tools: Vec<&Message> = messages.iter().filter(|m| m.role == Role::Tool).collect();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].tool_call_id.as_deref(), Some("a"));
        assert_eq!(tools[1].tool_call_id.as_deref(), Some("b"));
    }

    #[test]
    fn batches_stop_at_the_first_write() {
        let provider = MockProvider::new(vec![]);
        let mut settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let calls = vec![
            ToolCall::new("1", "read_file", "{\"path\":\"a\"}"),
            ToolCall::new("2", "bash", "{\"command\":\"rg todo\"}"),
            ToolCall::new("3", "write_file", "{\"path\":\"a\",\"content\":\"x\"}"),
            ToolCall::new("4", "read_file", "{\"path\":\"b\"}"),
        ];
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        {
            let agent = Agent::new(
                &provider,
                &registry,
                &mut hooks,
                &settings,
                std::env::current_dir().unwrap(),
                &cancel,
            );
            assert_eq!(agent.batch_len(&calls, 0), 2);
            assert_eq!(agent.batch_len(&calls, 2), 1);
            assert_eq!(agent.batch_len(&calls, 3), 1);
        }
        settings.tools.parallel = false;
        let mut hooks = NoHooks;
        let agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        assert_eq!(agent.batch_len(&calls, 0), 1);
    }

    #[test]
    fn a_call_cut_off_by_the_token_limit_is_asked_for_again() {
        // First reply: a tool call whose arguments stop mid-object, ended by
        // the token limit. Second: the same call, whole.
        let cut = vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("read_file".into()),
                arguments: "{\"path\": \"src/li".into(),
            },
            StreamEvent::Finish("length".into()),
        ];
        let whole = vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_2".into()),
                name: Some("read_file".into()),
                arguments: "{\"path\": \"Cargo.toml\"}".into(),
            },
            StreamEvent::Finish("tool_calls".into()),
        ];
        let done = vec![StreamEvent::Text("read it".into())];
        let provider = MockProvider::new(vec![cut, whole, done]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        agent.notices = false;
        let io = RecordingIo::default();
        let mut messages = vec![Message::user("read the manifest")];
        let summary = agent.run_turn(&mut messages, &io).unwrap();

        // Three requests: the cut one, the retry, and the reply after the tool.
        assert_eq!(summary.requests, 3);
        // Nothing of the cut attempt is left in the conversation, and the tool
        // never saw the half-written arguments.
        assert!(
            !messages.iter().any(|m| m.content.contains("invalid JSON")),
            "{messages:#?}"
        );
        assert!(
            messages
                .iter()
                .all(|m| m.tool_calls.iter().all(|c| c.id != "call_1")),
            "{messages:#?}"
        );
        // The second request asks for more room than the first.
        let asked: Vec<Option<u32>> = provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.max_tokens)
            .collect();
        assert!(asked[1] > asked[0], "{asked:?}");
    }

    #[test]
    fn a_finished_job_is_mentioned_in_the_next_request() {
        // Notices are process-wide: only one test at a time may collect them.
        let _notices = crate::jobs::notice_lock();
        let job = crate::jobs::table()
            .spawn("sh", "true", &std::env::current_dir().unwrap(), 4096, 0)
            .unwrap();
        job.announce();
        assert!(job.wait(Duration::from_secs(5)));
        let provider = MockProvider::new(vec![vec![
            StreamEvent::Text("ok".into()),
            StreamEvent::Finish("stop".into()),
        ]]);
        let settings = Settings::default();
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        let mut messages = vec![Message::user("anything")];
        agent
            .run_turn(&mut messages, &RecordingIo::default())
            .unwrap();
        crate::jobs::table().remove(job.id);
        let note = messages
            .iter()
            .find(|m| m.content.starts_with("[background]"))
            .expect("the model is told a job ended");
        assert!(note.content.contains("exited 0"), "{}", note.content);
        assert_eq!(note.role, Role::User);
    }

    #[test]
    fn a_plan_left_alone_is_recalled() {
        let _notices = crate::jobs::notice_lock();
        let _guard = crate::plan::test_lock();
        crate::plan::store()
            .edit(|p| {
                p.set(&[crate::plan::NewTask {
                    title: "write the parser".into(),
                    ..Default::default()
                }])?;
                Ok(((), true))
            })
            .unwrap();
        let script = || {
            vec![
                StreamEvent::Text("ok".into()),
                StreamEvent::Finish("stop".into()),
            ]
        };
        let provider = MockProvider::new(vec![script(), script()]);
        let mut settings = Settings::default();
        settings.context.plan_reminder_every = 1;
        let registry = Registry::builtins(&settings.tools);
        let mut hooks = NoHooks;
        let cancel = AtomicBool::new(false);
        let mut agent = Agent::new(
            &provider,
            &registry,
            &mut hooks,
            &settings,
            std::env::current_dir().unwrap(),
            &cancel,
        );
        let sent =
            |n: usize| -> Vec<Message> { provider.requests.lock().unwrap()[n].messages.clone() };
        let mut messages = vec![Message::user("go")];
        agent
            .run_turn(&mut messages, &RecordingIo::default())
            .unwrap();
        // The plan had just changed, so the first request said nothing about it.
        assert!(!sent(0).iter().any(|m| m.content.starts_with("[plan]")));
        agent
            .run_turn(&mut messages, &RecordingIo::default())
            .unwrap();
        let asked = sent(1);
        let note = asked
            .iter()
            .find(|m| m.content.starts_with("[plan]"))
            .expect("a quiet plan is recalled");
        assert!(note.content.contains("0/1 done"), "{}", note.content);
        assert_eq!(note.role, Role::User);
        // The nudge rode along with that one request and stayed out of the
        // conversation, so no later turn pays for it or reads it as current.
        assert!(!messages.iter().any(|m| m.content.starts_with("[plan]")));
    }

    #[test]
    fn date_is_sane() {
        let d = date_string();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
    }

    /// Every variant, once. The encoder's `match` is exhaustive and the
    /// compiler will say so; the decoder matches on a `&str` and cannot be
    /// told anything, so this list is the reminder.
    fn all_events() -> Vec<AgentEvent> {
        let call = ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: ToolFunction {
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            },
        };
        let message = Message::assistant("hello");
        let usage = Usage {
            prompt_tokens: 12,
            completion_tokens: 34,
            ..Default::default()
        };

        vec![
            AgentEvent::RequestStart { turn: 3 },
            AgentEvent::Text("a word".into()),
            AgentEvent::Reasoning("a thought".into()),
            AgentEvent::AssistantMessage(message.clone()),
            AgentEvent::Usage(usage),
            AgentEvent::ToolStart(call.clone()),
            AgentEvent::ToolEnd {
                call: call.clone(),
                result: ToolResult {
                    output: "out".into(),
                    is_error: false,
                    diff: None,
                },
                duration_ms: 17,
            },
            AgentEvent::ToolDenied {
                call: call.clone(),
                reason: "user declined".into(),
            },
            AgentEvent::ToolMessage(message),
            AgentEvent::Image {
                path: PathBuf::from("/tmp/a b/pic.png"),
                mime: "image/png".into(),
                width: 640,
                height: 480,
                bytes: 1024,
            },
            AgentEvent::Notice("something happened".into()),
            AgentEvent::SettingsPatch(serde_json::json!({"model": {"id": "x"}})),
            AgentEvent::Retry {
                attempt: 2,
                wait_ms: 500,
                error: "timed out".into(),
            },
            AgentEvent::Error("it broke".into()),
            AgentEvent::Compacting { auto: true },
            AgentEvent::CompactProgress {
                done: 10,
                budget: 100,
            },
            AgentEvent::Compacted {
                before: 900,
                after: 120,
                summary: "we talked".into(),
            },
            AgentEvent::TurnEnd(TurnSummary {
                usage,
                requests: 2,
                tool_calls: 1,
                cancelled: false,
            }),
        ]
    }

    #[test]
    fn every_event_survives_the_round_trip() {
        let all = all_events();
        assert_eq!(all.len(), 18, "a variant was added without a line here");
        for ev in all {
            let json = event_json(&ev);
            assert_eq!(
                event_from_json(&json).as_ref(),
                Some(&ev),
                "{json} did not come back as it went out"
            );
        }
    }

    #[test]
    fn an_event_this_build_does_not_know_is_skipped() {
        assert_eq!(
            event_from_json(&serde_json::json!({"type": "dancing"})),
            None
        );
        assert_eq!(
            event_from_json(&serde_json::json!({"text": "no type"})),
            None
        );
        // The right name with the wrong shape is not a guess either.
        assert_eq!(event_from_json(&serde_json::json!({"type": "text"})), None);
    }
}
