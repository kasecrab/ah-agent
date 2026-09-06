//! Terminal UI. Threads: render loop, input reader, engine. Idle = blocked on a channel.

mod input;
mod keys;
mod theme;
mod transcript;

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use ah_core::abi::*;
use ah_core::agent::AgentEvent;
use ah_core::models::{self, ModelInfo};
use ah_core::settings::{Origin, SettingsStack};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::{self, AnyError, Engine, EngineCmd, UiEvent};
use input::Editor;
use keys::Chord;
use theme::Palette;
use transcript::{Block, Entry, View};

enum Msg {
    Input(Event),
    Engine(UiEvent),
    Models(Result<Vec<ModelInfo>, String>),
}

/// Built-in slash commands: `(name, description, takes_args)`.
const COMMANDS: &[(&str, &str, bool)] = &[
    ("help", "list commands and keys", false),
    (
        "model",
        "pick a model (fuzzy search over OpenRouter catalogue)",
        true,
    ),
    ("clear", "clear the conversation", false),
    ("reload", "re-read config and reload plugins", false),
    ("plugins", "list active plugins", false),
    ("tools", "list tools", false),
    ("keys", "show key bindings", false),
    ("config", "show config files and layers", false),
    ("set", "override a setting: /set theme.accent magenta", true),
    ("yolo", "auto-approve tool calls", false),
    ("ask", "ask before tool calls", false),
    ("reasoning", "toggle reasoning display", false),
    ("sidebar", "toggle tool sidebar", false),
    ("session", "show session id and file", false),
    ("quit", "exit", false),
];

struct Completion {
    /// `(name, description, takes_args)`
    items: Vec<(String, String, bool)>,
    selected: usize,
}

struct Picker {
    query: String,
    all: Vec<ModelInfo>,
    results: Vec<usize>,
    selected: usize,
    loading: bool,
    error: Option<String>,
}

impl Picker {
    fn refilter(&mut self) {
        let idx: Vec<usize> = (0..self.all.len()).collect();
        let all = &self.all;
        self.results = models::rank(&self.query, &idx, |&i| {
            format!("{} {}", all[i].id, all[i].name)
        })
        .into_iter()
        .copied()
        .collect();
        self.selected = 0;
    }
}

#[derive(Clone)]
struct Binds {
    submit: Vec<Chord>,
    newline: Vec<Chord>,
    cancel: Vec<Chord>,
    quit: Vec<Chord>,
    scroll_up: Vec<Chord>,
    scroll_down: Vec<Chord>,
    page_up: Vec<Chord>,
    page_down: Vec<Chord>,
    scroll_top: Vec<Chord>,
    scroll_bottom: Vec<Chord>,
    clear: Vec<Chord>,
    toggle_tools: Vec<Chord>,
    toggle_reasoning: Vec<Chord>,
    toggle_sidebar: Vec<Chord>,
    history_prev: Vec<Chord>,
    history_next: Vec<Chord>,
    delete_word: Vec<Chord>,
    delete_line: Vec<Chord>,
    line_start: Vec<Chord>,
    line_end: Vec<Chord>,
    /// Plugin-provided `(chord, action)`; action is a Keys field or `/command`.
    extra: Vec<(Chord, String)>,
}

impl Binds {
    fn from_keys(k: &Keys, extra: &[(String, String)]) -> Self {
        let p = keys::parse_all;
        Self {
            submit: p(&k.submit),
            newline: p(&k.newline),
            cancel: p(&k.cancel),
            quit: p(&k.quit),
            scroll_up: p(&k.scroll_up),
            scroll_down: p(&k.scroll_down),
            page_up: p(&k.page_up),
            page_down: p(&k.page_down),
            scroll_top: p(&k.scroll_top),
            scroll_bottom: p(&k.scroll_bottom),
            clear: p(&k.clear),
            toggle_tools: p(&k.toggle_tools),
            toggle_reasoning: p(&k.toggle_reasoning),
            toggle_sidebar: p(&k.toggle_sidebar),
            history_prev: p(&k.history_prev),
            history_next: p(&k.history_next),
            delete_word: p(&k.delete_word),
            delete_line: p(&k.delete_line),
            line_start: p(&k.line_start),
            line_end: p(&k.line_end),
            extra: extra
                .iter()
                .filter_map(|(k, a)| keys::parse(k).map(|c| (c, a.clone())))
                .collect(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Thinking,
    Streaming,
    Tool,
}

struct App {
    overrides: crate::Overrides,
    stack: SettingsStack,
    pal: Palette,
    pal_gen: u64,
    binds: Binds,
    view: View,
    entries: Vec<Entry>,
    editor: Editor,
    scroll: usize,
    follow: bool,
    viewport_lines: usize,
    total_lines: usize,
    state: State,
    busy: bool,
    tool_name: String,
    usage: Usage,
    spinner_i: usize,
    plugin_status: Option<String>,
    plugin_commands: Vec<(String, SlashCommandSpec)>,
    plugin_count: u32,
    pending_perm: Option<(ToolCall, String)>,
    always_allow: HashSet<String>,
    git_branch: String,
    cwd: String,
    session_id: String,
    sidebar_items: Vec<(String, bool, u64)>,
    completion: Option<Completion>,
    picker: Option<Picker>,
    self_tx: Sender<Msg>,
    tx: Sender<EngineCmd>,
    perm_tx: Sender<bool>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    last_draw: Instant,
    dirty: bool,
    quit: bool,
    size: (u16, u16),
}

pub fn run(
    o: &crate::Overrides,
    resume: Option<&str>,
    initial_prompt: Option<String>,
) -> Result<(), AnyError> {
    // alt screen first, then load plugins
    let mut terminal = ratatui::init();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        hook(info);
    }));
    let r = run_inner(o, resume, initial_prompt, &mut terminal);
    disable_extras();
    ratatui::restore();
    r
}

fn run_inner(
    o: &crate::Overrides,
    resume: Option<&str>,
    initial_prompt: Option<String>,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<(), AnyError> {
    let cwd = app::resolve_cwd(o)?;
    let mut stack = app::load_settings(o)?;
    let mut engine = Engine::new(&stack, cwd.clone(), resume, true)?;
    let (reports, patches, commands) = engine.load_plugins();
    for (name, p) in patches {
        if let Err(e) = stack.push(Origin::Plugin(name.clone()), p) {
            eprintln!("plugin {name}: bad settings patch: {e}");
        }
    }
    engine.apply_settings(stack.settings().clone(), stack.value().clone());
    let has_key = engine.has_key();
    let plugin_count = reports.iter().filter(|r| r.ok).count() as u32;
    let early_logs = engine.take_plugin_logs();

    let (ui_tx, ui_rx) = mpsc::channel::<Msg>();
    let (eng_tx, eng_rx) = mpsc::channel::<EngineCmd>();
    let (perm_tx, perm_rx) = mpsc::channel::<bool>();
    let cancel = engine.cancel.clone();
    let session_id = engine.session.id.clone();
    let resumed: Vec<Message> = engine.session.messages.clone();

    // Engine thread.
    {
        let ui_tx = ui_tx.clone();
        let (fwd_tx, fwd_rx) = mpsc::channel::<UiEvent>();
        std::thread::Builder::new()
            .name("ah-engine".into())
            .spawn(move || engine.serve(eng_rx, fwd_tx, perm_rx))
            .expect("spawn engine");
        std::thread::Builder::new()
            .name("ah-engine-fwd".into())
            .spawn(move || {
                while let Ok(ev) = fwd_rx.recv() {
                    if ui_tx.send(Msg::Engine(ev)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn forwarder");
    }

    let settings = stack.settings().clone();
    let mut app = App {
        overrides: o.clone(),
        pal: Palette::from_theme(&settings.theme),
        pal_gen: 1,
        binds: Binds::from_keys(&settings.keys, &[]),
        view: View {
            show_tool_output: settings.layout.show_tool_output,
            tool_output_lines: settings.layout.tool_output_lines,
            show_reasoning: settings.layout.show_reasoning,
            wrap: settings.layout.wrap,
        },
        stack,
        entries: Vec::new(),
        editor: Editor::default(),
        scroll: 0,
        follow: true,
        viewport_lines: 0,
        total_lines: 0,
        state: State::Idle,
        busy: false,
        tool_name: String::new(),
        usage: Usage::default(),
        spinner_i: 0,
        plugin_status: None,
        plugin_commands: commands,
        plugin_count,
        pending_perm: None,
        always_allow: HashSet::new(),
        git_branch: ah_core::plugins::git_branch(&cwd),
        cwd: cwd.display().to_string(),
        session_id,
        sidebar_items: Vec::new(),
        completion: None,
        picker: None,
        self_tx: ui_tx.clone(),
        tx: eng_tx,
        perm_tx,
        cancel,
        last_draw: Instant::now() - Duration::from_secs(1),
        dirty: true,
        quit: false,
        size: (0, 0),
    };

    for r in &reports {
        if !r.ok && r.message != "disabled" {
            app.push(Block::Error(format!("plugin {}: {}", r.name, r.message)));
        }
    }
    for (p, lvl, m) in early_logs {
        if lvl <= LogLevel::Warn {
            app.push(Block::Notice(format!("[{p}] {m}")));
        }
    }
    if !has_key {
        app.push(Block::Error(
            "no API key. Run `ah login` (or set OPENROUTER_API_KEY), then /reload.".into(),
        ));
    }
    app.load_history(&resumed);
    if app.entries.is_empty() {
        app.push(Block::Notice(format!(
            "ah {} · {} · {} plugin{} · /help for commands",
            env!("CARGO_PKG_VERSION"),
            settings.model.id,
            plugin_count,
            if plugin_count == 1 { "" } else { "s" }
        )));
    }

    // Input thread.
    {
        let ui_tx = ui_tx.clone();
        std::thread::Builder::new()
            .name("ah-input".into())
            .spawn(move || {
                while let Ok(ev) = crossterm::event::read() {
                    if ui_tx.send(Msg::Input(ev)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn input");
    }

    enable_extras(&settings);

    app.request_status();
    if let Some(p) = initial_prompt {
        app.submit(p);
    }

    let result = app.event_loop(terminal, &ui_rx);
    let _ = app.tx.send(EngineCmd::Quit);
    result
}

fn enable_extras(settings: &Settings) {
    use crossterm::event::{
        EnableBracketedPaste, EnableMouseCapture, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    };
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(out, EnableBracketedPaste);
    if settings.layout.mouse {
        let _ = crossterm::execute!(out, EnableMouseCapture);
    }
    // don't query support: blocks up to 2s on terminals that ignore it
    if settings.layout.kitty_keyboard {
        let _ = crossterm::execute!(
            out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
            )
        );
    }
}

fn disable_extras() {
    use crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags,
    };
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
    let _ = crossterm::execute!(out, DisableMouseCapture);
    let _ = crossterm::execute!(out, DisableBracketedPaste);
}

impl App {
    fn settings(&self) -> &Settings {
        self.stack.settings()
    }

    fn push(&mut self, b: Block) {
        self.entries.push(Entry::new(b));
        self.dirty = true;
    }

    fn load_history(&mut self, msgs: &[Message]) {
        for m in msgs {
            match m.role {
                Role::User => self.push(Block::User(m.content.clone())),
                Role::Assistant => {
                    if !m.content.is_empty() || m.tool_calls.is_empty() {
                        self.push(Block::Assistant {
                            text: m.content.clone(),
                            reasoning: m.reasoning.clone().unwrap_or_default(),
                            streaming: false,
                        });
                    }
                    for c in &m.tool_calls {
                        self.push(Block::Tool {
                            call: c.clone(),
                            result: None,
                            duration_ms: 0,
                            expanded: None,
                        });
                    }
                }
                Role::Tool => {
                    if let Some(id) = &m.tool_call_id {
                        let is_err = m.content.starts_with("ERROR: ");
                        let out = m
                            .content
                            .strip_prefix("ERROR: ")
                            .unwrap_or(&m.content)
                            .to_string();
                        for e in self.entries.iter_mut().rev() {
                            if let Block::Tool { call, result, .. } = &mut e.block
                                && &call.id == id
                            {
                                *result = Some(ToolResult {
                                    output: out,
                                    is_error: is_err,
                                });
                                break;
                            }
                        }
                    }
                }
                Role::System => {}
            }
        }
        if !msgs.is_empty() {
            self.push(Block::Notice(format!(
                "resumed session {} ({} messages)",
                self.session_id,
                msgs.len()
            )));
        }
    }

    /// Re-derive palette, binds and view flags from the current settings.
    fn refresh_from_settings(&mut self, plugin_binds: &[(String, String)]) {
        let s = self.stack.settings().clone();
        self.pal = Palette::from_theme(&s.theme);
        self.pal_gen += 1;
        self.binds = Binds::from_keys(&s.keys, plugin_binds);
        self.view = View {
            show_tool_output: s.layout.show_tool_output,
            tool_output_lines: s.layout.tool_output_lines,
            show_reasoning: s.layout.show_reasoning,
            wrap: s.layout.wrap,
        };
        self.dirty = true;
        let _ = self
            .tx
            .send(EngineCmd::Settings(Box::new(s), self.stack.value().clone()));
    }

    fn apply_patch(&mut self, origin: Origin, patch: serde_json::Value) {
        match self.stack.push(origin, patch) {
            Ok(()) => self.refresh_from_settings(&[]),
            Err(e) => self.push(Block::Error(format!("settings patch rejected: {e}"))),
        }
    }

    fn status_ctx(&self) -> StatusContext {
        let state = match self.state {
            State::Idle => "idle".to_string(),
            State::Thinking => "thinking".into(),
            State::Streaming => "streaming".into(),
            State::Tool => format!("tool:{}", self.tool_name),
        };
        let mut ctx = StatusContext {
            model: self.settings().model.id.clone(),
            usage: self.usage,
            cwd: self.cwd.clone(),
            git_branch: self.git_branch.clone(),
            plugins: self.plugin_count,
            state,
            session_id: self.session_id.clone(),
            width: self.size.0,
            rendered: String::new(),
        };
        ctx.rendered = app::render_status_template(&self.settings().statusline.format, &ctx);
        ctx
    }

    fn request_status(&mut self) {
        if self.plugin_count > 0 && !self.busy {
            let _ = self.tx.send(EngineCmd::Statusline(self.status_ctx()));
        }
    }

    fn set_state(&mut self, s: State) {
        if self.state != s {
            self.state = s;
            self.dirty = true;
        }
    }

    fn submit(&mut self, text: String) {
        let text = text.trim_end().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(cmd) = text.strip_prefix('/') {
            self.slash(cmd);
            return;
        }
        if self.busy {
            self.push(Block::Notice("busy; wait or press Esc to cancel".into()));
            return;
        }
        self.push(Block::User(text.clone()));
        self.follow = true;
        self.busy = true;
        self.set_state(State::Thinking);
        let _ = self.tx.send(EngineCmd::Submit(text));
    }

    fn event_loop(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        rx: &Receiver<Msg>,
    ) -> Result<(), AnyError> {
        loop {
            if self.dirty {
                let throttle = Duration::from_millis(self.settings().layout.stream_redraw_ms);
                if !self.busy || self.last_draw.elapsed() >= throttle {
                    terminal.draw(|f| self.draw(f))?;
                    self.last_draw = Instant::now();
                    self.dirty = false;
                }
            }
            if self.quit {
                return Ok(());
            }
            let msg = if self.busy || self.dirty {
                let wait = if self.dirty {
                    Duration::from_millis(self.settings().layout.stream_redraw_ms.max(1))
                } else {
                    Duration::from_millis(self.settings().layout.spinner_ms.max(20))
                };
                match rx.recv_timeout(wait) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => {
                        if self.busy {
                            self.spinner_i = self.spinner_i.wrapping_add(1);
                            self.dirty = true;
                        }
                        None
                    }
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            } else {
                Some(rx.recv()?)
            };
            let Some(first) = msg else { continue };
            self.handle(first);
            while let Ok(m) = rx.try_recv() {
                self.handle(m);
                if self.quit {
                    break;
                }
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Input(ev) => {
                self.handle_input(ev);
                self.update_completion();
            }
            Msg::Engine(ev) => self.handle_engine(ev),
            Msg::Models(res) => {
                self.dirty = true;
                if let Some(p) = self.picker.as_mut() {
                    p.loading = false;
                    match res {
                        Ok(list) => {
                            p.all = list;
                            p.error = None;
                            p.refilter();
                        }
                        Err(e) => p.error = Some(e),
                    }
                }
            }
        }
    }

    // ---- slash completion ------------------------------------------------

    fn update_completion(&mut self) {
        let t = self.editor.text.clone();
        let Some(rest) = t.strip_prefix('/') else {
            self.completion = None;
            return;
        };
        if rest.contains(' ') || rest.contains('\n') || self.editor.cursor != self.editor.char_len()
        {
            self.completion = None;
            return;
        }
        let mut all: Vec<(String, String, bool)> = COMMANDS
            .iter()
            .map(|(n, d, a)| (n.to_string(), d.to_string(), *a))
            .collect();
        for (p, c) in &self.plugin_commands {
            let takes_args = c.usage.trim().len() > c.name.len() + 1;
            all.push((
                c.name.clone(),
                format!("{} ({p})", c.description),
                takes_args,
            ));
        }
        let ranked: Vec<(String, String, bool)> = models::rank(rest, &all, |x| x.0.clone())
            .into_iter()
            .cloned()
            .collect();
        let prev_name = self
            .completion
            .as_ref()
            .and_then(|c| c.items.get(c.selected).map(|i| i.0.clone()));
        if ranked.is_empty() {
            self.completion = None;
        } else {
            // keep the highlight stable across keystrokes
            let selected = prev_name
                .and_then(|name| ranked.iter().position(|i| i.0 == name))
                .unwrap_or(0);
            self.completion = Some(Completion {
                items: ranked,
                selected,
            });
        }
        self.dirty = true;
    }

    /// Fill the selected command into the editor; with `submit`, run it when
    /// it needs no arguments.
    fn accept_completion(&mut self, submit: bool) {
        let Some(c) = self.completion.take() else {
            return;
        };
        let Some((name, _, takes_args)) = c.items.get(c.selected).cloned() else {
            return;
        };
        self.editor.clear();
        if submit && (!takes_args || name == "model") {
            self.slash(&name);
        } else {
            self.editor.insert_str(&format!("/{name} "));
        }
        self.dirty = true;
    }

    // ---- model picker ----------------------------------------------------

    fn open_picker(&mut self, query: &str, force_refresh: bool) {
        let cached = models::load_cached();
        let stale = cached
            .as_ref()
            .is_none_or(|(_, age)| age.as_secs() > 24 * 3600);
        let all = cached.map(|(m, _)| m).unwrap_or_default();
        let mut p = Picker {
            query: query.to_string(),
            all,
            results: Vec::new(),
            selected: 0,
            loading: false,
            error: None,
        };
        p.refilter();
        if stale || force_refresh {
            p.loading = true;
            let tx = self.self_tx.clone();
            let base = self.settings().model.base_url.clone();
            let key = ah_core::auth::api_key(self.settings().model.api_key.as_deref());
            std::thread::Builder::new()
                .name("ah-models".into())
                .spawn(move || {
                    let r = models::fetch(&base, key.as_deref()).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Models(r));
                })
                .ok();
        }
        self.picker = Some(p);
        self.dirty = true;
    }

    fn picker_key(&mut self, k: KeyEvent) {
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        let page = 10;
        let last = p.results.len().saturating_sub(1);
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _) => self.picker = None,
            (KeyCode::Enter, _) => {
                let chosen = p
                    .results
                    .get(p.selected)
                    .map(|&i| p.all[i].id.clone())
                    .or_else(|| {
                        // no match: accept a raw id
                        let q = p.query.trim();
                        (!q.is_empty() && q.contains('/')).then(|| q.to_string())
                    });
                self.picker = None;
                if let Some(id) = chosen {
                    self.set_model(&id);
                }
            }
            (KeyCode::Up, _) => p.selected = p.selected.saturating_sub(1),
            (KeyCode::Down, _) => p.selected = (p.selected + 1).min(last),
            (KeyCode::PageUp, _) => p.selected = p.selected.saturating_sub(page),
            (KeyCode::PageDown, _) => p.selected = (p.selected + page).min(last),
            (KeyCode::Home, _) => p.selected = 0,
            (KeyCode::End, _) => p.selected = last,
            (KeyCode::Backspace, _) => {
                p.query.pop();
                p.refilter();
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                p.query.clear();
                p.refilter();
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                let t = p.query.trim_end().to_string();
                p.query = t
                    .rsplit_once(' ')
                    .map(|(a, _)| format!("{a} "))
                    .unwrap_or_default();
                p.refilter();
            }
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                let q = p.query.clone();
                self.open_picker(&q, true);
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.picker = None,
            (KeyCode::Char(c), m) if !m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                p.query.push(c);
                p.refilter();
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn set_model(&mut self, id: &str) {
        self.apply_patch(
            Origin::Runtime("slash".into()),
            serde_json::json!({"model": {"id": id}}),
        );
        self.push(Block::Notice(format!(
            "model → {id}  (persist with `--set model.id={id}` or config.toml)"
        )));
        self.request_status();
    }

    fn handle_engine(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Agent(a) => self.handle_agent(a),
            UiEvent::Busy(b) => {
                self.busy = b;
                if !b {
                    self.set_state(State::Idle);
                    self.pending_perm = None;
                    self.git_branch = ah_core::plugins::git_branch(std::path::Path::new(&self.cwd));
                    self.request_status();
                }
                self.dirty = true;
            }
            UiEvent::PluginsLoaded {
                reports,
                patches,
                commands,
            } => {
                self.plugin_count = reports.iter().filter(|r| r.ok).count() as u32;
                self.plugin_commands = commands;
                for r in &reports {
                    if !r.ok && r.message != "disabled" {
                        self.push(Block::Error(format!("plugin {}: {}", r.name, r.message)));
                    }
                }
                let mut failed = None;
                for (name, p) in patches {
                    if let Err(e) = self.stack.push(Origin::Plugin(name.clone()), p) {
                        failed = Some(format!("plugin {name}: {e}"));
                    }
                }
                if let Some(f) = failed {
                    self.push(Block::Error(f));
                }
                self.refresh_from_settings(&[]);
                self.plugin_status = None;
                self.push(Block::Notice(format!(
                    "reloaded: {} plugin{} active",
                    self.plugin_count,
                    if self.plugin_count == 1 { "" } else { "s" }
                )));
                self.request_status();
            }
            UiEvent::PluginLogs(logs) => {
                for (p, lvl, m) in logs {
                    if lvl <= LogLevel::Warn {
                        self.push(Block::Notice(format!("[{p}] {m}")));
                    }
                }
            }
            UiEvent::Statusline(s) => {
                self.plugin_status = Some(s);
                self.dirty = true;
            }
            UiEvent::Slash(out) => {
                if let Some(m) = out.message {
                    self.push(Block::Notice(m));
                }
                if let Some(p) = out.settings_patch {
                    self.apply_patch(Origin::Runtime("slash".into()), p);
                }
                if let Some(t) = out.send_to_model {
                    self.submit(t);
                }
            }
            UiEvent::AskPermission { call, reason } => {
                if self.always_allow.contains(&call.function.name) {
                    let _ = self.perm_tx.send(true);
                } else {
                    self.pending_perm = Some((call, reason));
                    self.dirty = true;
                }
            }
        }
    }

    fn handle_agent(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::RequestStart { .. } => {
                self.set_state(State::Thinking);
                self.push(Block::Assistant {
                    text: String::new(),
                    reasoning: String::new(),
                    streaming: true,
                });
            }
            AgentEvent::Text(t) => {
                self.set_state(State::Streaming);
                if let Some(Block::Assistant { text, .. }) =
                    self.entries.last_mut().map(|e| &mut e.block)
                {
                    text.push_str(&t);
                }
                self.dirty = true;
            }
            AgentEvent::Reasoning(t) => {
                self.set_state(State::Streaming);
                if let Some(Block::Assistant { reasoning, .. }) =
                    self.entries.last_mut().map(|e| &mut e.block)
                {
                    reasoning.push_str(&t);
                }
                self.dirty = true;
            }
            AgentEvent::AssistantMessage(m) => {
                if let Some(Block::Assistant {
                    text,
                    reasoning,
                    streaming,
                }) = self.entries.last_mut().map(|e| &mut e.block)
                {
                    *text = m.content.clone();
                    *reasoning = m.reasoning.clone().unwrap_or_default();
                    *streaming = false;
                }
                if let Some(Block::Assistant {
                    text, reasoning, ..
                }) = self.entries.last().map(|e| &e.block)
                    && text.is_empty()
                    && reasoning.is_empty()
                {
                    self.entries.pop();
                }
                self.dirty = true;
            }
            AgentEvent::Usage(u) => {
                self.usage.add(&u);
                self.dirty = true;
            }
            AgentEvent::ToolStart(call) => {
                self.tool_name = call.function.name.clone();
                self.set_state(State::Tool);
                self.push(Block::Tool {
                    call,
                    result: None,
                    duration_ms: 0,
                    expanded: None,
                });
            }
            AgentEvent::ToolEnd {
                call,
                result,
                duration_ms,
            } => {
                self.sidebar_items.push((
                    call.function.name.clone(),
                    !result.is_error,
                    duration_ms,
                ));
                for e in self.entries.iter_mut().rev() {
                    if let Block::Tool {
                        call: c,
                        result: r,
                        duration_ms: d,
                        ..
                    } = &mut e.block
                        && c.id == call.id
                    {
                        *r = Some(result);
                        *d = duration_ms;
                        break;
                    }
                }
                self.set_state(State::Thinking);
                self.dirty = true;
            }
            AgentEvent::ToolDenied { call, reason } => {
                self.push(Block::Error(format!(
                    "{} denied: {reason}",
                    call.function.name
                )));
            }
            AgentEvent::Notice(n) => self.push(Block::Notice(n)),
            AgentEvent::SettingsPatch(p) => self.apply_patch(Origin::Runtime("plugin".into()), p),
            AgentEvent::Retry {
                attempt,
                wait_ms,
                error,
            } => self.push(Block::Notice(format!(
                "retry {attempt} in {wait_ms} ms: {error}"
            ))),
            AgentEvent::Error(e) => self.push(Block::Error(e)),
            AgentEvent::TurnEnd(s) => {
                if s.cancelled {
                    self.push(Block::Notice("cancelled".into()));
                }
                self.set_state(State::Idle);
            }
            AgentEvent::ToolMessage(_) => {}
        }
    }

    fn handle_input(&mut self, ev: Event) {
        match ev {
            Event::Key(k) => self.handle_key(k),
            Event::Paste(s) => {
                if let Some(p) = self.picker.as_mut() {
                    p.query.push_str(s.trim());
                    p.refilter();
                } else {
                    let n = self.settings().layout.paste_collapse_lines;
                    self.editor.insert_paste(&s, n);
                }
                self.dirty = true;
            }
            Event::Resize(w, h) => {
                self.size = (w, h);
                self.dirty = true;
            }
            Event::Mouse(m) => {
                let step = self.settings().layout.scroll_step.max(1) as usize;
                match m.kind {
                    MouseEventKind::ScrollUp => self.scroll_by(-(step as isize)),
                    MouseEventKind::ScrollDown => self.scroll_by(step as isize),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.total_lines.saturating_sub(self.viewport_lines);
        let cur = if self.follow { max } else { self.scroll };
        let new = (cur as isize + delta).clamp(0, max as isize) as usize;
        self.scroll = new;
        self.follow = new >= max;
        self.dirty = true;
    }

    fn cancel_turn(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if self.pending_perm.take().is_some() {
            let _ = self.perm_tx.send(false);
        }
        self.dirty = true;
    }

    fn handle_key(&mut self, k: KeyEvent) {
        if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }
        self.dirty = true;
        let b = self.binds.clone();

        if self.picker.is_some() {
            self.picker_key(k);
            return;
        }

        if let Some(c) = self.completion.as_mut() {
            match k.code {
                KeyCode::Up | KeyCode::BackTab => {
                    c.selected = if c.selected == 0 {
                        c.items.len() - 1
                    } else {
                        c.selected - 1
                    };
                    return;
                }
                KeyCode::Down => {
                    c.selected = (c.selected + 1) % c.items.len();
                    return;
                }
                KeyCode::Tab => {
                    self.accept_completion(false);
                    return;
                }
                KeyCode::Enter
                    if !k
                        .modifiers
                        .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                {
                    self.accept_completion(true);
                    return;
                }
                _ => {}
            }
        }

        if let Some((call, _)) = &self.pending_perm {
            let name = call.function.name.clone();
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.pending_perm = None;
                    let _ = self.perm_tx.send(true);
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.always_allow.insert(name);
                    self.pending_perm = None;
                    let _ = self.perm_tx.send(true);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.pending_perm = None;
                    let _ = self.perm_tx.send(false);
                }
                _ if keys::any_match(&b.quit, &k) => self.cancel_turn(),
                _ => {}
            }
            return;
        }

        for (chord, action) in &b.extra {
            if keys::matches(chord, &k) {
                let action = action.clone();
                self.run_action(&action);
                return;
            }
        }

        if keys::any_match(&b.quit, &k) {
            if self.busy {
                self.cancel_turn();
            } else if !self.editor.is_empty() && k.code == KeyCode::Char('c') {
                self.editor.clear();
            } else {
                self.quit = true;
            }
        } else if keys::any_match(&b.cancel, &k) {
            if self.busy {
                self.cancel_turn();
            } else if !self.editor.is_empty() {
                self.editor.clear();
            } else if !self.follow {
                self.follow = true;
            }
        } else if keys::any_match(&b.newline, &k) {
            self.editor.insert_char('\n');
        } else if keys::any_match(&b.submit, &k) {
            let t = self.editor.take();
            self.submit(t);
        } else if keys::any_match(&b.scroll_up, &k) {
            self.scroll_by(-(self.settings().layout.scroll_step.max(1) as isize));
        } else if keys::any_match(&b.scroll_down, &k) {
            self.scroll_by(self.settings().layout.scroll_step.max(1) as isize);
        } else if keys::any_match(&b.page_up, &k) {
            self.scroll_by(-(self.viewport_lines.saturating_sub(1).max(1) as isize));
        } else if keys::any_match(&b.page_down, &k) {
            self.scroll_by(self.viewport_lines.saturating_sub(1).max(1) as isize);
        } else if keys::any_match(&b.scroll_top, &k) {
            self.scroll = 0;
            self.follow = false;
        } else if keys::any_match(&b.scroll_bottom, &k) {
            self.follow = true;
        } else if keys::any_match(&b.clear, &k) {
            self.run_action("/clear");
        } else if keys::any_match(&b.toggle_tools, &k) {
            self.toggle_tools();
        } else if keys::any_match(&b.toggle_reasoning, &k) {
            let v = !self.view.show_reasoning;
            self.apply_patch(
                Origin::Runtime("ui".into()),
                serde_json::json!({"layout": {"show_reasoning": v}}),
            );
        } else if keys::any_match(&b.toggle_sidebar, &k) {
            let v = !self.settings().layout.sidebar;
            self.apply_patch(
                Origin::Runtime("ui".into()),
                serde_json::json!({"layout": {"sidebar": v}}),
            );
        } else if keys::any_match(&b.delete_word, &k) {
            self.editor.delete_word();
        } else if keys::any_match(&b.delete_line, &k) {
            self.editor.delete_to_line_start();
        } else if keys::any_match(&b.line_start, &k) {
            self.editor.home();
        } else if keys::any_match(&b.line_end, &k) {
            self.editor.end();
        } else if keys::any_match(&b.history_prev, &k)
            && (self.editor.is_empty() || self.editor.line_col().0 == 0)
        {
            if !self.editor.history_prev() {
                self.editor.up();
            }
        } else if keys::any_match(&b.history_next, &k)
            && (self.editor.is_empty() || self.editor.line_col().0 + 1 == self.editor.line_count())
        {
            if !self.editor.history_next() {
                self.editor.down();
            }
        } else {
            match k.code {
                KeyCode::Char(c)
                    if !k.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    self.editor.insert_char(c)
                }
                KeyCode::Backspace => {
                    if !self.editor.backspace_chip() {
                        self.editor.backspace();
                    }
                }
                KeyCode::Delete => self.editor.delete(),
                KeyCode::Left => self.editor.left(),
                KeyCode::Right => self.editor.right(),
                KeyCode::Up => {
                    self.editor.up();
                }
                KeyCode::Down => {
                    self.editor.down();
                }
                KeyCode::Home => self.editor.home(),
                KeyCode::End => self.editor.end(),
                KeyCode::Tab => self.editor.insert_str("    "),
                KeyCode::Enter => {
                    self.editor.insert_char('\n');
                }
                _ => {}
            }
        }
    }

    fn toggle_tools(&mut self) {
        let any_expanded = self.entries.iter().any(|e| {
            matches!(
                &e.block,
                Block::Tool {
                    expanded: Some(true),
                    ..
                }
            )
        }) || self.view.show_tool_output;
        let v = !any_expanded;
        for e in &mut self.entries {
            if let Block::Tool { expanded, .. } = &mut e.block {
                *expanded = None;
            }
        }
        self.apply_patch(
            Origin::Runtime("ui".into()),
            serde_json::json!({"layout": {"show_tool_output": v}}),
        );
    }

    fn run_action(&mut self, action: &str) {
        if let Some(cmd) = action.strip_prefix('/') {
            self.slash(cmd);
            return;
        }
        let synth = |code: KeyCode, mods: KeyModifiers| KeyEvent::new(code, mods);
        match action {
            "submit" => {
                let t = self.editor.take();
                self.submit(t);
            }
            "cancel" => self.cancel_turn(),
            "quit" => self.quit = true,
            "clear" => self.slash("clear"),
            "toggle_tools" => self.toggle_tools(),
            "scroll_top" => {
                self.scroll = 0;
                self.follow = false;
            }
            "scroll_bottom" => self.follow = true,
            "page_up" => self.handle_key(synth(KeyCode::PageUp, KeyModifiers::NONE)),
            "page_down" => self.handle_key(synth(KeyCode::PageDown, KeyModifiers::NONE)),
            other => self.push(Block::Notice(format!("unknown action `{other}`"))),
        }
    }

    fn slash(&mut self, cmd: &str) {
        let (name, args) = cmd
            .split_once(' ')
            .map(|(n, a)| (n, a.trim()))
            .unwrap_or((cmd, ""));
        match name {
            "help" | "?" => {
                let mut s = String::from(
                    "/help  /model [id]  /clear  /reload  /plugins  /tools  /keys  /config  /set key value  /yolo  /ask  /reasoning  /sidebar  /session  /quit",
                );
                for (_, c) in &self.plugin_commands {
                    s.push_str(&format!("\n/{} — {}", c.name, c.description));
                }
                s.push_str("\nkeys: Enter send · Shift/Alt-Enter newline · Esc cancel · Ctrl-T tool output · Ctrl-R reasoning · PgUp/PgDn scroll · Ctrl-C quit");
                self.push(Block::Notice(s));
            }
            "model" | "models" => {
                if args == "refresh" {
                    self.open_picker("", true);
                } else if args.is_empty() {
                    self.open_picker("", false);
                } else if models::load_cached().is_some_and(|(m, _)| m.iter().any(|x| x.id == args))
                {
                    self.set_model(args);
                } else {
                    self.open_picker(args, false);
                }
            }
            "clear" => {
                self.entries.clear();
                self.sidebar_items.clear();
                self.usage = Usage::default();
                let _ = self.tx.send(EngineCmd::Clear);
                self.push(Block::Notice("conversation cleared".into()));
            }
            "reload" => match app::load_settings(&self.overrides) {
                Ok(stack) => {
                    self.stack = stack;
                    self.refresh_from_settings(&[]);
                    let _ = self.tx.send(EngineCmd::Reload);
                }
                Err(e) => self.push(Block::Error(format!("reload failed: {e}"))),
            },
            "plugins" => {
                let n = self.plugin_count;
                let mut s = format!("{n} plugin(s) active");
                for (p, c) in &self.plugin_commands {
                    s.push_str(&format!("\n  /{} ({p})", c.name));
                }
                for d in ah_core::paths::plugin_dirs() {
                    s.push_str(&format!("\n  dir: {}", d.display()));
                }
                self.push(Block::Notice(s));
            }
            "tools" => {
                let s = self.settings().tools.enabled.join(", ");
                self.push(Block::Notice(format!(
                    "built-in tools: {s} (plugin tools are added automatically)"
                )));
            }
            "keys" => {
                let k = &self.settings().keys;
                let s = format!(
                    "submit {:?}\nnewline {:?}\ncancel {:?}\nquit {:?}\nscroll {:?}/{:?} page {:?}/{:?}\ntools {:?} reasoning {:?} sidebar {:?}",
                    k.submit,
                    k.newline,
                    k.cancel,
                    k.quit,
                    k.scroll_up,
                    k.scroll_down,
                    k.page_up,
                    k.page_down,
                    k.toggle_tools,
                    k.toggle_reasoning,
                    k.toggle_sidebar
                );
                self.push(Block::Notice(s));
            }
            "config" => {
                let s = format!(
                    "user: {}\nproject: {}\nlayers: {}",
                    ah_core::paths::user_config_file().display(),
                    ah_core::paths::project_config_file().display(),
                    self.stack
                        .layers()
                        .iter()
                        .map(|l| format!("{:?}", l.origin))
                        .collect::<Vec<_>>()
                        .join(" → ")
                );
                self.push(Block::Notice(s));
            }
            "set" => {
                let spec = args.replacen(' ', "=", 1);
                match app::set_to_patch(&spec) {
                    Ok(p) => {
                        self.apply_patch(Origin::Runtime("slash".into()), p);
                        self.push(Block::Notice(format!("set {spec}")));
                    }
                    Err(e) => self.push(Block::Error(e.to_string())),
                }
            }
            "yolo" => {
                self.apply_patch(
                    Origin::Runtime("slash".into()),
                    serde_json::json!({"permissions": {"mode": "auto"}}),
                );
                self.push(Block::Notice("permissions: auto".into()));
            }
            "ask" => {
                self.apply_patch(
                    Origin::Runtime("slash".into()),
                    serde_json::json!({"permissions": {"mode": "ask"}}),
                );
                self.push(Block::Notice("permissions: ask".into()));
            }
            "reasoning" => {
                let v = !self.view.show_reasoning;
                self.apply_patch(
                    Origin::Runtime("slash".into()),
                    serde_json::json!({"layout": {"show_reasoning": v}}),
                );
            }
            "sidebar" => {
                let v = !self.settings().layout.sidebar;
                self.apply_patch(
                    Origin::Runtime("slash".into()),
                    serde_json::json!({"layout": {"sidebar": v}}),
                );
            }
            "session" => {
                let s = format!(
                    "session {} · {}",
                    self.session_id,
                    ah_core::paths::sessions_dir()
                        .join(format!("{}.jsonl", self.session_id))
                        .display()
                );
                self.push(Block::Notice(s));
            }
            "quit" | "exit" | "q" => self.quit = true,
            other => {
                if self.plugin_commands.iter().any(|(_, c)| c.name == other) {
                    let _ = self.tx.send(EngineCmd::Slash {
                        name: other.into(),
                        args: args.into(),
                    });
                } else {
                    self.push(Block::Notice(format!(
                        "unknown command /{other} (try /help)"
                    )));
                }
            }
        }
    }

    // ---- rendering --------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        self.size = (area.width, area.height);
        let pal = self.pal.clone();
        let layout = self.settings().layout.clone();
        f.render_widget(ratatui::widgets::Block::default().style(pal.base()), area);

        let border = if pal.has_borders() { 2 } else { 0 };
        let input_width = area.width.saturating_sub(border) as usize;
        let (rows, cursor_rc) = self.editor.layout(input_width.max(1));
        let input_rows = rows.len().clamp(
            layout.input_height.max(1) as usize,
            layout.input_max_height.max(1) as usize,
        ) as u16;
        let perm_rows: u16 = if self.pending_perm.is_some() {
            3 + border
        } else {
            0
        };
        let status_rows: u16 = if layout.show_status { 1 } else { 0 };

        let [transcript_area, perm_area, input_area, status_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(perm_rows),
            Constraint::Length(input_rows + border),
            Constraint::Length(status_rows),
        ])
        .areas(area);

        let (transcript_area, sidebar_area) =
            if layout.sidebar && area.width > layout.sidebar_width + 20 {
                let [t, s] = Layout::horizontal([
                    Constraint::Min(10),
                    Constraint::Length(layout.sidebar_width),
                ])
                .areas(transcript_area);
                (t, Some(s))
            } else {
                (transcript_area, None)
            };

        self.draw_transcript(f, transcript_area, &pal, layout.transcript_max_width);
        if let Some(s) = sidebar_area {
            self.draw_sidebar(f, s, &pal);
        }
        if let Some((call, reason)) = &self.pending_perm {
            let block = pal.block(true).title(" permission ");
            let inner = block.inner(perm_area);
            f.render_widget(Clear, perm_area);
            f.render_widget(block, perm_area);
            let lines = vec![
                Line::from(vec![
                    Span::styled(format!("{} ", call.function.name), pal.bold(pal.tool)),
                    Span::raw(crate::cli::compact_args(&call.function.arguments)),
                ]),
                Line::from(Span::styled(
                    if reason.is_empty() {
                        "run this tool?".to_string()
                    } else {
                        reason.clone()
                    },
                    pal.dim(),
                )),
                Line::from(vec![
                    Span::styled("[y]", pal.bold(pal.accent)),
                    Span::raw(" yes  "),
                    Span::styled("[n]", pal.bold(pal.accent)),
                    Span::raw(" no  "),
                    Span::styled("[a]", pal.bold(pal.accent)),
                    Span::raw(" always this session"),
                ]),
            ];
            f.render_widget(Paragraph::new(lines), inner);
        }

        // Input box.
        let title = if self.busy {
            format!(" {} ", self.spinner())
        } else {
            String::new()
        };
        let block = pal
            .block(!self.busy)
            .title(title)
            .style(Style::default().fg(pal.input_fg).bg(pal.input_bg));
        let inner = block.inner(input_area);
        f.render_widget(block, input_area);
        let first_row = cursor_rc
            .0
            .saturating_sub(inner.height.saturating_sub(1) as usize);
        let visible: Vec<Line> = rows
            .iter()
            .skip(first_row)
            .take(inner.height as usize)
            .map(|r| Line::from(r.clone()))
            .collect();
        if self.editor.is_empty() && !self.busy {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Type a message, /help for commands",
                    pal.dim(),
                ))),
                inner,
            );
        } else {
            f.render_widget(Paragraph::new(visible), inner);
        }
        if self.pending_perm.is_none() && self.picker.is_none() {
            f.set_cursor_position((
                inner.x + cursor_rc.1 as u16,
                inner.y + (cursor_rc.0 - first_row) as u16,
            ));
        }

        if layout.show_status {
            self.draw_status(f, status_area, &pal);
        }
        if let Some(c) = &self.completion {
            self.draw_completion(f, c, input_area, &pal);
        }
        if self.picker.is_some() {
            self.draw_picker(f, area, &pal);
        }
    }

    fn draw_completion(&self, f: &mut Frame, c: &Completion, input_area: Rect, pal: &Palette) {
        let rows = c.items.len().min(8) as u16;
        let width = input_area.width.clamp(20, 70);
        let height = rows + 2;
        let y = input_area.y.saturating_sub(height);
        let r = Rect {
            x: input_area.x,
            y,
            width,
            height,
        };
        f.render_widget(Clear, r);
        let block = pal.block(true).title(" commands · Tab fill · Enter run ");
        let inner = block.inner(r);
        f.render_widget(block, r);
        let first = c.selected.saturating_sub(rows.saturating_sub(1) as usize);
        let lines: Vec<Line> = c
            .items
            .iter()
            .enumerate()
            .skip(first)
            .take(rows as usize)
            .map(|(i, (name, desc, _))| {
                let sel = i == c.selected;
                let name_style = if sel {
                    pal.bold(pal.accent).add_modifier(Modifier::REVERSED)
                } else {
                    pal.bold(pal.accent)
                };
                Line::from(vec![
                    Span::styled(format!(" /{name:<10}"), name_style),
                    Span::styled(format!(" {desc}"), pal.dim()),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_picker(&self, f: &mut Frame, area: Rect, pal: &Palette) {
        let Some(p) = &self.picker else { return };
        let width = (area.width * 9 / 10).clamp(40, 110).min(area.width);
        let height = (area.height * 4 / 5).clamp(8, 40).min(area.height);
        let r = Rect {
            x: (area.width - width) / 2,
            y: (area.height - height) / 2,
            width,
            height,
        };
        f.render_widget(Clear, r);
        let block = pal
            .block(true)
            .title(" model · type to search · Enter select · Esc close · Ctrl-R refresh ");
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [q_area, list_area, foot_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", pal.bold(pal.accent)),
                Span::raw(p.query.clone()),
            ])),
            q_area,
        );
        f.set_cursor_position((q_area.x + 2 + p.query.chars().count() as u16, q_area.y));
        let rows = list_area.height as usize;
        let first = p.selected.saturating_sub(rows.saturating_sub(1));
        let id_w = (inner.width as usize).saturating_sub(34).max(20);
        let lines: Vec<Line> = p
            .results
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(i, &mi)| {
                let m = &p.all[mi];
                let sel = i == p.selected;
                let cur = m.id == self.settings().model.id;
                let mut id: String = m.id.chars().take(id_w).collect();
                if cur {
                    id.push_str(" •");
                }
                let ctx = if m.context_length >= 1000 {
                    format!("{}k", m.context_length / 1000)
                } else {
                    m.context_length.to_string()
                };
                let style = if sel {
                    pal.bold(pal.accent).add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(pal.fg)
                };
                Line::from(vec![
                    Span::styled(format!(" {id:<id_w$}"), style),
                    Span::styled(format!(" {ctx:>6} "), pal.dim()),
                    Span::styled(
                        format!("${:>6.2}/${:<6.2}", m.prompt_per_m, m.completion_per_m),
                        pal.dim(),
                    ),
                    Span::styled(
                        if m.tools { " tools" } else { "      " },
                        Style::default().fg(pal.tool),
                    ),
                    Span::styled(
                        if m.reasoning { " think" } else { "" },
                        Style::default().fg(pal.reasoning),
                    ),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list_area);
        let foot = match (&p.error, p.loading, p.all.is_empty()) {
            (Some(e), _, _) => format!(" fetch failed: {e}"),
            (None, true, true) => " fetching model list from OpenRouter…".to_string(),
            (None, true, false) => format!(" {} of {} · refreshing…", p.results.len(), p.all.len()),
            (None, false, true) => " no models cached; press Ctrl-R to fetch".to_string(),
            (None, false, false) => format!(
                " {} of {} models · $/M tokens in/out",
                p.results.len(),
                p.all.len()
            ),
        };
        f.render_widget(Paragraph::new(Span::styled(foot, pal.dim())), foot_area);
    }

    fn spinner(&self) -> &str {
        let s = &self.pal.spinner;
        &s[self.spinner_i % s.len()]
    }

    fn draw_transcript(&mut self, f: &mut Frame, area: Rect, pal: &Palette, max_width: u16) {
        let width = if max_width > 0 {
            area.width.min(max_width)
        } else {
            area.width
        };
        let area = Rect { width, ..area };
        let view = self.view;
        let pal_gen = self.pal_gen;
        let mut lines: Vec<Line<'static>> = Vec::new();
        for e in &mut self.entries {
            lines.extend_from_slice(e.lines(width.saturating_sub(1), &view, pal, pal_gen));
        }
        self.total_lines = lines.len();
        self.viewport_lines = area.height as usize;
        let max_scroll = self.total_lines.saturating_sub(self.viewport_lines);
        if self.follow {
            self.scroll = max_scroll;
        } else {
            self.scroll = self.scroll.min(max_scroll);
        }
        let visible: Vec<Line<'static>> = lines
            .into_iter()
            .skip(self.scroll)
            .take(self.viewport_lines)
            .collect();
        f.render_widget(Paragraph::new(visible), area);
        if !self.follow && max_scroll > 0 {
            let tag = format!(" ↓ {} more ", max_scroll - self.scroll);
            let w = tag.chars().count() as u16;
            let r = Rect {
                x: area.x + area.width.saturating_sub(w + 1),
                y: area.y + area.height.saturating_sub(1),
                width: w,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    tag,
                    Style::default().fg(pal.status_fg).bg(pal.status_bg),
                )),
                r,
            );
        }
    }

    fn draw_sidebar(&self, f: &mut Frame, area: Rect, pal: &Palette) {
        let block = pal.block(false).title(" tools ");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let h = inner.height as usize;
        let items: Vec<Line> = self
            .sidebar_items
            .iter()
            .rev()
            .take(h)
            .rev()
            .map(|(name, ok, ms)| {
                Line::from(vec![
                    Span::styled(
                        if *ok { "✓ " } else { "✗ " },
                        Style::default().fg(if *ok { pal.user } else { pal.error }),
                    ),
                    Span::raw(name.clone()),
                    Span::styled(format!(" {ms}ms"), pal.dim()),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(items), inner);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect, pal: &Palette) {
        let ctx = self.status_ctx();
        let mut text = self.plugin_status.clone().unwrap_or(ctx.rendered);
        if self.busy {
            text = format!(" {}{}", self.spinner(), text);
        }
        let style = Style::default().fg(pal.status_fg).bg(pal.status_bg);
        let line = Line::from(Span::styled(text, style.add_modifier(Modifier::BOLD)));
        f.render_widget(Paragraph::new(line).style(style), area);
    }
}
