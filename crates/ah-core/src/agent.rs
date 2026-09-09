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
    /// Plan version behind the last reminder, and how many requests have gone
    /// by without the plan changing.
    plan_seen: u64,
    plan_quiet: u32,
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
            session_id: String::new(),
            context_window: 0,
            context_tokens: 0,
            compactions: 0,
            notices: true,
            plan_seen: 0,
            plan_quiet: 0,
        }
    }

    /// True once the conversation fills `context.compact_at` percent of the window.
    pub fn over_threshold(&self) -> bool {
        let c = &self.settings.context;
        c.auto_compact
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
        let cache_control = self.cache_control(prompt_bytes(&all[0].content, &all[1..], 0));
        let req = ChatRequest {
            model: self.settings.model.id.clone(),
            messages: all,
            tools: Vec::new(),
            max_tokens: Some(self.settings.context.summary_max_tokens),
            temperature: None,
            top_p: None,
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
        let msg = summary_message(&acc.content);
        let after = estimate_tokens(msg.content.len());
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
                for note in crate::jobs::table().notices(crate::jobs::Audience::Model) {
                    messages.push(Message::user(format!("[background] {note}")));
                }
            }
            if let Some(line) = self.plan_reminder().filter(|_| self.notices) {
                messages.push(Message::user(line));
            }

            let mut all = Vec::with_capacity(messages.len() + 1);
            all.push(Message::system(system.clone()));
            all.extend(messages.iter().cloned());
            let req = ChatRequest {
                model: self.settings.model.id.clone(),
                messages: all,
                tools: specs.clone(),
                max_tokens: self.settings.model.max_tokens,
                temperature: self.settings.model.temperature,
                top_p: self.settings.model.top_p,
                reasoning: self.settings.model.reasoning.clone(),
                provider: self.settings.model.provider.clone(),
                session_id: self.session_key(),
                cache_control: self.cache_control(prompt_bytes(&system, messages, specs_bytes)),
            };
            let req = self.hooks.before_request(req, turn);

            io.emit(AgentEvent::RequestStart { turn });
            let (acc, failure) = self.stream_with_retry(&req, io);
            let acc = match failure {
                None => acc,
                Some(e) => {
                    // Some models report usage before the stream ends; count it
                    // rather than lose the spend along with the answer.
                    if acc.usage.total_tokens > 0 {
                        summary.usage.add(&acc.usage);
                        self.context_tokens = acc.usage.prompt_tokens + acc.usage.completion_tokens;
                        io.emit(AgentEvent::Usage(acc.usage));
                    }
                    if let Some(m) = partial_message(acc) {
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
            let assistant = acc.clone().into_message();
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
        let plan = store.snapshot();
        let (done, total) = plan.counts();
        if total == 0 || done == total {
            return None;
        }
        Some(format!(
            "[plan] {}\nUpdate it with the plan tool as you go.",
            plan.summary()
        ))
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
            && let Some(rule) = crate::policy::denied(cmd, &self.settings.permissions.deny)
        {
            let reason = format!("matches deny rule `{rule}`");
            io.emit(AgentEvent::ToolDenied {
                call,
                reason: reason.clone(),
            });
            return Gate::Refused(ToolResult::err(format!(
                "denied by policy: {reason}. Ask the user to run it themselves if it is really needed."
            )));
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
            cancel: self.cancel,
            ask,
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
    ) -> (Accumulator, Option<Error>) {
        let mut attempt = 0u32;
        loop {
            let mut acc = Accumulator::default();
            let mut got_any = false;
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
                    StreamEvent::ToolCallDelta { .. } => got_any = true,
                    _ => {}
                }
                !self.cancel.load(Ordering::Relaxed)
            });
            match res {
                Ok(()) => return (acc.finish(), None),
                Err(Error::Cancelled) => return (acc.finish(), Some(Error::Cancelled)),
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
                            return (acc.finish(), Some(Error::Cancelled));
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(e) => return (acc.finish(), Some(e)),
            }
        }
    }
}

/// What is worth keeping from a request that ended part way: the text the
/// model had already written, said to be cut short. Tool calls are dropped —
/// one cut off mid-stream has truncated arguments and no result to pair with,
/// and every call shown to the API needs one. A stream that produced no text
/// leaves nothing to keep, reasoning alone included: an assistant message with
/// no content is not one the next request can carry.
fn partial_message(acc: Accumulator) -> Option<Message> {
    let text = acc.content.trim_end();
    if text.is_empty() {
        return None;
    }
    let mut m = Message::assistant(format!("{text}\n\n[cut short]"));
    if !acc.reasoning.is_empty() {
        m.reasoning = Some(acc.reasoning);
    }
    Some(m)
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

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::tools::Tool;

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
        let io = RecordingIo::default();
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
    fn a_finished_job_is_mentioned_in_the_next_request() {
        // Notices are process-wide: only one test at a time may collect them.
        let _notices = crate::jobs::notice_lock();
        let job = crate::jobs::table()
            .spawn("sh", "true", &std::env::current_dir().unwrap(), 4096)
            .unwrap();
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
                }])
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
        let mut messages = vec![Message::user("go")];
        agent
            .run_turn(&mut messages, &RecordingIo::default())
            .unwrap();
        // The plan had just changed, so the first request said nothing about it.
        assert!(!messages.iter().any(|m| m.content.starts_with("[plan]")));
        agent
            .run_turn(&mut messages, &RecordingIo::default())
            .unwrap();
        let note = messages
            .iter()
            .find(|m| m.content.starts_with("[plan]"))
            .expect("a quiet plan is recalled");
        assert!(note.content.contains("0/1 done"), "{}", note.content);
    }

    #[test]
    fn date_is_sane() {
        let d = date_string();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
    }
}
