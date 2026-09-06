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

/// Wrap a summary as the single user message a compacted conversation starts with.
pub fn summary_message(summary: &str) -> Message {
    Message::user(format!(
        "[The conversation so far was compacted. Summary:]\n\n{}\n\n[End of summary. Continue from here.]",
        summary.trim()
    ))
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

/// Callbacks the loop uses to talk to whoever is driving it.
pub trait AgentIo {
    fn emit(&self, ev: AgentEvent);
    /// Blocking permission prompt. Return `false` to deny.
    fn ask_permission(&self, call: &ToolCall, reason: &str) -> bool;
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
    /// Model context window in tokens; 0 disables auto compaction.
    pub context_window: u64,
    /// Conversation size as of the last response.
    pub context_tokens: u64,
    /// Compactions performed by this agent (the caller re-persists messages).
    pub compactions: u32,
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
            context_window: 0,
            context_tokens: 0,
            compactions: 0,
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
        if messages.is_empty() {
            return Ok(());
        }
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
        let req = ChatRequest {
            model: self.settings.model.id.clone(),
            messages: all,
            tools: Vec::new(),
            max_tokens: Some(self.settings.context.summary_max_tokens),
            temperature: None,
            top_p: None,
            reasoning: None,
            provider: self.settings.model.provider.clone(),
        };
        let mut acc = Accumulator::default();
        self.provider.stream(&req, self.cancel, &mut |ev| {
            acc.apply(&ev);
            !self.cancel.load(Ordering::Relaxed)
        })?;
        let acc = acc.finish();
        if acc.content.trim().is_empty() {
            return Err(Error::Http("empty summary".into()));
        }
        let before = self.context_tokens;
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
                io.emit(AgentEvent::Notice("compacting context…".into()));
                match self.compact(messages, "", io) {
                    Ok(()) => {}
                    Err(Error::Cancelled) => {
                        summary.cancelled = true;
                        break;
                    }
                    Err(e) => io.emit(AgentEvent::Notice(format!("compaction failed: {e}"))),
                }
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
            };
            let req = self.hooks.before_request(req, turn);

            io.emit(AgentEvent::RequestStart { turn });
            let acc = match self.stream_with_retry(&req, io) {
                Ok(acc) => acc,
                Err(Error::Cancelled) => {
                    summary.cancelled = true;
                    break;
                }
                Err(e) => {
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

            for call in &acc.tool_calls {
                if self.cancel.load(Ordering::Relaxed) {
                    summary.cancelled = true;
                    // The API requires a tool message for every call; record the cancel.
                    let m = Message::tool_result(&call.id, "[cancelled by user]");
                    io.emit(AgentEvent::ToolMessage(m.clone()));
                    messages.push(m);
                    continue;
                }
                summary.tool_calls += 1;
                let (result, dur) = self.run_tool(call, &cwd_str, io);
                let m = Message::tool_result(
                    &call.id,
                    if result.is_error {
                        format!("ERROR: {}", result.output)
                    } else {
                        result.output.clone()
                    },
                );
                io.emit(AgentEvent::ToolEnd {
                    call: call.clone(),
                    result,
                    duration_ms: dur,
                });
                io.emit(AgentEvent::ToolMessage(m.clone()));
                messages.push(m);
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

    fn run_tool(&mut self, call: &ToolCall, cwd: &str, io: &dyn AgentIo) -> (ToolResult, u64) {
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
                    call: call.clone(),
                    reason: reason.clone(),
                });
                return (ToolResult::err(format!("denied by policy: {reason}")), 0);
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
                call: call.clone(),
                reason: reason.clone(),
            });
            return (
                ToolResult::err(format!(
                    "denied by policy: {reason}. Ask the user to run it themselves if it is really needed."
                )),
                0,
            );
        }
        if must_ask && !io.ask_permission(&call, &ask_reason) {
            io.emit(AgentEvent::ToolDenied {
                call: call.clone(),
                reason: "user declined".into(),
            });
            return (ToolResult::err("user declined to run this tool"), 0);
        }

        io.emit(AgentEvent::ToolStart(call.clone()));
        let start = Instant::now();
        let result = match self.hooks.plugin_tool(&call, cwd) {
            Some(r) => r,
            None => {
                let ctx = ToolCtx {
                    cwd: &self.cwd,
                    settings: &self.settings.tools,
                };
                self.registry.run(&call, &ctx)
            }
        };
        let dur = start.elapsed().as_millis() as u64;
        let (result, patches) = self.hooks.after_tool(&call, result, dur);
        for p in patches {
            io.emit(AgentEvent::SettingsPatch(p));
        }
        (result, dur)
    }

    fn stream_with_retry(&self, req: &ChatRequest, io: &dyn AgentIo) -> Result<Accumulator> {
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
                Ok(()) => return Ok(acc.finish()),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
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
                            return Err(Error::Cancelled);
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
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
    }

    impl AgentIo for RecordingIo {
        fn emit(&self, ev: AgentEvent) {
            self.events.lock().unwrap().push(ev);
        }
        fn ask_permission(&self, _call: &ToolCall, _reason: &str) -> bool {
            self.allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

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
            }),
        ]
    }

    #[test]
    fn compacts_mid_turn_when_over_threshold() {
        let big = Usage {
            prompt_tokens: 95,
            completion_tokens: 2,
            total_tokens: 97,
            cost: 0.0,
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
    fn date_is_sane() {
        let d = date_string();
        assert_eq!(d.len(), 10);
        assert!(d.starts_with("20"));
    }
}
