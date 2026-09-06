//! Terminal UI. Threads: render loop, input reader, engine. Idle = blocked on a channel.

mod highlight;
mod input;
mod keys;
mod markdown;
mod picker;
mod theme;
mod transcript;
mod usage;

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use ah_core::abi::*;
use ah_core::agent::AgentEvent;
use ah_core::models::{self, ModelInfo};
use ah_core::settings::{Origin, SettingsStack};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::{self, AnyError, Engine, EngineCmd, UiEvent};
use input::Editor;
use keys::Chord;
use picker::{Action, Kind, Picker, Row};

/// Row id of the "+ new favorite" entry in the favorites picker.
const NEW_FAVORITE: &str = "\0new";
use theme::Palette;
use transcript::{Block, Entry, View};
use usage::Stats;

enum Msg {
    Input(Event),
    Engine(UiEvent),
    Models(Result<Vec<ModelInfo>, String>),
    Usage(Result<usage::Remote, String>),
}

/// Built-in slash commands, alphabetical: `(name, description, takes_args)`.
const COMMANDS: &[(&str, &str, bool)] = &[
    ("ask", "ask before tool calls", false),
    ("clear", "clear the conversation", false),
    (
        "compact",
        "summarise the conversation: /compact [focus]",
        true,
    ),
    ("config", "show config files and layers", false),
    ("effort", "set reasoning effort: /effort [level]", true),
    (
        "favorite",
        "manage favorite models: /favorite [name] (Shift-Tab cycles)",
        true,
    ),
    ("help", "list commands and keys", false),
    ("init", "write an AGENTS.md for this project", false),
    ("keys", "show key bindings", false),
    (
        "model",
        "pick a model (fuzzy search over OpenRouter catalogue)",
        true,
    ),
    ("plugins", "list active plugins", false),
    ("quit", "exit", false),
    ("reasoning", "toggle reasoning display", false),
    ("reload", "re-read config and reload plugins", false),
    (
        "rename",
        "name this session; /resume finds it by name",
        true,
    ),
    ("resume", "switch to a previous session", true),
    ("session", "show session id and file", false),
    ("skills", "run a saved prompt: /skills [name] [args]", true),
    ("set", "override a setting: /set theme.accent magenta", true),
    ("tools", "list tools", false),
    ("usage", "session cost, account balance, top models", false),
    ("yolo", "auto-approve tool calls", false),
];

/// Other spellings the command popup should find and show.
const ALIASES: &[(&str, &str)] = &[("favorite", "fav"), ("quit", "exit")];

fn alias_of(name: &str) -> Option<&'static str> {
    ALIASES.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
}

/// Mouse selection in screen cells.
#[derive(Clone, Copy)]
struct Selection {
    anchor: (u16, u16),
    cur: (u16, u16),
    dragging: bool,
}

struct Completion {
    /// `(name, description, takes_args)`
    items: Vec<(String, String, bool)>,
    selected: usize,
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
    cycle_model: Vec<Chord>,
    history_prev: Vec<Chord>,
    history_next: Vec<Chord>,
    delete_word: Vec<Chord>,
    delete_line: Vec<Chord>,
    line_start: Vec<Chord>,
    line_end: Vec<Chord>,
    paste_image: Vec<Chord>,
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
            cycle_model: p(&k.cycle_model),
            history_prev: p(&k.history_prev),
            history_next: p(&k.history_next),
            delete_word: p(&k.delete_word),
            delete_line: p(&k.delete_line),
            line_start: p(&k.line_start),
            line_end: p(&k.line_end),
            paste_image: p(&k.paste_image),
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
    /// Messages typed while a turn ran; sent one per turn once it ends.
    queue: std::collections::VecDeque<(String, Vec<String>)>,
    /// Input modalities of the current model from the catalogue.
    modalities: Vec<String>,
    /// `TI→T` for the current model, empty when unknown.
    modality_icons: String,
    /// A catalogue fetch is in flight.
    fetching_models: bool,
    scroll: usize,
    follow: bool,
    viewport_lines: usize,
    total_lines: usize,
    state: State,
    busy: bool,
    tool_name: String,
    usage: Usage,
    /// Conversation size as of the last response, and the model's window.
    context_tokens: u64,
    context_window: u64,
    spinner_i: usize,
    plugin_status: Option<String>,
    plugin_commands: Vec<(String, SlashCommandSpec)>,
    plugin_count: u32,
    pending_perm: Option<(ToolCall, String)>,
    always_allow: HashSet<String>,
    git_branch: String,
    cwd: String,
    session_id: String,
    session_name: Option<String>,
    completion: Option<Completion>,
    picker: Option<Picker>,
    usage_pane: Option<usage::Pane>,
    stats: Stats,
    /// When the current reply started streaming reasoning.
    think_start: Option<Instant>,
    sel: Option<Selection>,
    copy_pending: bool,
    mouse_on: bool,
    /// Model catalogue, loaded lazily for the picker and reasoning checks.
    catalogue: Option<Vec<ModelInfo>>,
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
    let hint = r?;
    if let Some(what) = hint {
        println!("Resume this session with:\n  ah -r {what}");
    }
    Ok(())
}

fn run_inner(
    o: &crate::Overrides,
    resume: Option<&str>,
    initial_prompt: Option<String>,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<Option<String>, AnyError> {
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
    let session_name = engine.session.name.clone();
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
            markdown: settings.layout.markdown,
            code_highlight: settings.layout.code_highlight,
        },
        stack,
        entries: Vec::new(),
        editor: Editor::default(),
        queue: std::collections::VecDeque::new(),
        modalities: Vec::new(),
        modality_icons: String::new(),
        fetching_models: false,
        scroll: 0,
        follow: true,
        viewport_lines: 0,
        total_lines: 0,
        state: State::Idle,
        busy: false,
        tool_name: String::new(),
        usage: Usage::default(),
        context_tokens: 0,
        context_window: 0,
        spinner_i: 0,
        plugin_status: None,
        plugin_commands: commands,
        plugin_count,
        pending_perm: None,
        always_allow: HashSet::new(),
        git_branch: ah_core::plugins::git_branch(&cwd),
        cwd: cwd.display().to_string(),
        session_id,
        session_name,
        completion: None,
        picker: None,
        usage_pane: None,
        stats: Stats::default(),
        think_start: None,
        sel: None,
        copy_pending: false,
        mouse_on: settings.layout.mouse,
        catalogue: None,
        self_tx: ui_tx.clone(),
        tx: eng_tx,
        perm_tx,
        cancel,
        last_draw: Instant::now() - Duration::from_secs(1),
        dirty: true,
        quit: false,
        size: (0, 0),
    };
    app.refresh_window();

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
    app.editor
        .set_history(Editor::load_history_file(&ah_core::paths::history_file()));

    app.request_status();
    if let Some(p) = initial_prompt {
        app.submit(p);
    }

    let result = app.event_loop(terminal, &ui_rx);
    let _ = app.tx.send(EngineCmd::Quit);
    result?;
    Ok(app.resume_hint())
}

/// OSC 52: hand the text to the terminal's clipboard. Works in WezTerm,
/// kitty, foot, alacritty, iTerm2 and over ssh; terminals that ignore it
/// still offer Shift-drag for native selection.
/// Transcript text for a user message with `images` attachments.
fn user_display(text: &str, images: usize) -> String {
    match images {
        0 => text.to_string(),
        n => {
            let tag = (1..=n)
                .map(|i| format!("[Image #{i}]"))
                .collect::<Vec<_>>()
                .join(" ");
            if text.is_empty() {
                tag
            } else {
                format!("{text}\n{tag}")
            }
        }
    }
}

fn copy_to_clipboard(text: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "\x1b]52;c;{}\x07",
        ah_core::clipboard::base64(text.as_bytes())
    );
    let _ = out.flush();
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
                Role::User => self.push(Block::User(user_display(&m.content, m.images.len()))),
                Role::Assistant => {
                    if !m.content.is_empty() || m.tool_calls.is_empty() {
                        self.push(Block::Assistant {
                            text: m.content.clone(),
                            reasoning: m.reasoning.clone().unwrap_or_default(),
                            streaming: false,
                            think_ms: 0,
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
                                    diff: None,
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
            let what = match &self.session_name {
                Some(n) => format!("\u{201c}{n}\u{201d}"),
                None => format!("session {}", self.session_id),
            };
            self.push(Block::Notice(format!(
                "resumed {what} ({} messages)",
                msgs.len()
            )));
        }
    }

    /// Window and modalities of the current model from the cached catalogue
    /// (`context.window` overrides the window). Without a usable cache the
    /// catalogue is fetched once in the background.
    fn refresh_window(&mut self) {
        let s = self.stack.settings();
        let id = s.model.id.clone();
        let override_window = s.context.window;
        if self.catalogue.is_none() {
            match models::load_cached() {
                Some((m, _)) => self.catalogue = Some(m),
                None => self.fetch_models(),
            }
        }
        let info = self
            .catalogue
            .as_deref()
            .and_then(|c| c.iter().find(|m| m.id == id));
        self.context_window = if override_window > 0 {
            override_window
        } else {
            info.map(|m| m.context_length).unwrap_or(0)
        };
        self.modalities = info.map(|m| m.input_modalities.clone()).unwrap_or_default();
        self.modality_icons = info.map(|m| m.modality_icons()).unwrap_or_default();
    }

    /// Fetch the catalogue on a worker thread; the result arrives as `Msg::Models`.
    fn fetch_models(&mut self) {
        if self.fetching_models {
            return;
        }
        let key = ah_core::auth::api_key(self.settings().model.api_key.as_deref());
        let Some(key) = key else { return };
        self.fetching_models = true;
        let tx = self.self_tx.clone();
        let base = self.settings().model.base_url.clone();
        std::thread::Builder::new()
            .name("ah-models".into())
            .spawn(move || {
                let r = models::fetch(&base, Some(&key)).map_err(|e| e.to_string());
                let _ = tx.send(Msg::Models(r));
            })
            .ok();
    }

    /// Modality icons for `id` when the catalogue knows it and the setting is on.
    fn icons_for(&self, id: &str) -> String {
        if !self.settings().layout.show_modalities {
            return String::new();
        }
        self.catalogue
            .as_deref()
            .and_then(|c| c.iter().find(|m| m.id == id))
            .map(|m| m.modality_icons())
            .unwrap_or_default()
    }

    /// Re-derive palette, binds and view flags from the current settings.
    fn refresh_from_settings(&mut self, plugin_binds: &[(String, String)]) {
        self.refresh_window();
        let s = self.stack.settings().clone();
        self.pal = Palette::from_theme(&s.theme);
        self.pal_gen += 1;
        self.binds = Binds::from_keys(&s.keys, plugin_binds);
        self.view = View {
            show_tool_output: s.layout.show_tool_output,
            tool_output_lines: s.layout.tool_output_lines,
            show_reasoning: s.layout.show_reasoning,
            wrap: s.layout.wrap,
            markdown: s.layout.markdown,
            code_highlight: s.layout.code_highlight,
        };
        if s.layout.mouse != self.mouse_on {
            use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
            let mut out = std::io::stdout();
            let _ = if s.layout.mouse {
                crossterm::execute!(out, EnableMouseCapture)
            } else {
                crossterm::execute!(out, DisableMouseCapture)
            };
            self.mouse_on = s.layout.mouse;
        }
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
        let m = &self.settings().model;
        let favorite = self
            .favorite_index()
            .and_then(|i| m.favorites.keys().nth(i))
            .map(|k| format!("★{k}"))
            .unwrap_or_default();
        let mut ctx = StatusContext {
            model: m.id.clone(),
            usage: self.usage,
            cwd: self.cwd.clone(),
            git_branch: self.git_branch.clone(),
            plugins: self.plugin_count,
            state,
            session_id: self.session_id.clone(),
            width: self.size.0,
            favorite,
            effort: m.effort().unwrap_or("").to_string(),
            context_tokens: self.context_tokens,
            context_window: self.context_window,
            modalities: if self.settings().layout.show_modalities {
                self.modality_icons.clone()
            } else {
                String::new()
            },
            rendered: String::new(),
        };
        ctx.rendered = app::render_status_template(&self.settings().statusline.format, &ctx);
        ctx
    }

    fn request_status(&mut self) {
        if self.plugin_count > 0 && !self.busy {
            let _ = self
                .tx
                .send(EngineCmd::Statusline(Box::new(self.status_ctx())));
        }
    }

    fn set_state(&mut self, s: State) {
        if self.state != s {
            self.state = s;
            self.dirty = true;
        }
    }

    fn record_history(&mut self) {
        if let Some(e) = self.editor.history_added() {
            Editor::append_history_file(&ah_core::paths::history_file(), &e);
        }
    }

    fn submit(&mut self, text: String) {
        self.submit_with(text, Vec::new());
    }

    /// Take the editor's text and images and send them.
    fn submit_editor(&mut self) {
        let t = self.editor.take();
        let images = self.editor.take_images();
        self.record_history();
        self.submit_with(t, images);
    }

    fn submit_with(&mut self, text: String, images: Vec<String>) {
        let text = text.trim_end().to_string();
        if text.is_empty() && images.is_empty() {
            return;
        }
        if images.is_empty()
            && let Some(cmd) = text.strip_prefix('/')
        {
            self.slash(cmd);
            return;
        }
        if self.busy {
            let max = self.settings().layout.queue_max;
            if max == 0 {
                self.push(Block::Notice("busy; wait or press Esc to cancel".into()));
            } else if self.queue.len() >= max {
                self.push(Block::Notice(format!(
                    "queue full ({max}); Up edits the last queued message"
                )));
            } else {
                self.queue.push_back((text, images));
            }
            return;
        }
        self.push(Block::User(user_display(&text, images.len())));
        self.follow = true;
        self.busy = true;
        self.set_state(State::Thinking);
        let _ = self.tx.send(EngineCmd::Submit { text, images });
    }

    /// Ctrl-V: attach the clipboard image to the draft.
    fn paste_image(&mut self) {
        let model = self.settings().model.id.clone();
        if !self.modalities.is_empty() && !self.modalities.iter().any(|m| m == "image") {
            self.push(Block::Notice(format!(
                "{model} doesn't accept images as input (see /model)"
            )));
            return;
        }
        let cmd = self.settings().layout.image_paste_cmd.clone();
        match ah_core::clipboard::image(&cmd) {
            Ok(bytes) => {
                let url = ah_core::clipboard::data_url(&bytes);
                self.editor.insert_image(url, bytes.len());
            }
            Err(e) => self.push(Block::Notice(e.to_string())),
        }
    }

    /// Send the oldest queued message once the engine is free.
    fn drain_queue(&mut self) {
        if !self.busy
            && let Some((text, images)) = self.queue.pop_front()
        {
            self.submit_with(text, images);
        }
    }

    /// Move the last queued message back into the editor.
    fn unqueue_last(&mut self) -> bool {
        let Some((text, images)) = self.queue.pop_back() else {
            return false;
        };
        let n = self.settings().layout.paste_collapse_lines;
        if !self.editor.is_empty() {
            self.editor.insert_char('\n');
        }
        self.editor.insert_paste(&text, n);
        for url in images {
            let bytes = url.len() * 3 / 4;
            self.editor.insert_image(url, bytes);
        }
        true
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
            Msg::Usage(res) => {
                if let Some(p) = self.usage_pane.as_mut() {
                    p.loading = false;
                    match res {
                        Ok(r) => {
                            p.remote = Some(r);
                            p.error = None;
                            p.fetched = Some(Instant::now());
                        }
                        Err(e) => p.error = Some(e),
                    }
                    self.dirty = true;
                }
            }
            Msg::Models(res) => {
                self.dirty = true;
                self.fetching_models = false;
                match res {
                    Ok(list) => {
                        self.catalogue = Some(list);
                        self.refresh_window();
                        if let Some(p) = &self.picker
                            && let Kind::Model { favorite } = &p.kind
                        {
                            let (q, fav) = (p.query.clone(), favorite.clone());
                            self.open_model_picker(&q, fav, false);
                        }
                    }
                    Err(e) => {
                        if let Some(p) = self.picker.as_mut()
                            && matches!(p.kind, Kind::Model { .. })
                        {
                            p.loading = false;
                            p.error = Some(format!("fetch failed: {e}"));
                        }
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
        // a recalled command must stay reachable by more Up/Down presses
        if rest.contains(' ')
            || rest.contains('\n')
            || self.editor.cursor != self.editor.char_len()
            || self.editor.browsing_history()
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
        all.sort_by(|a, b| a.0.cmp(&b.0));
        let ranked: Vec<(String, String, bool)> =
            models::rank(rest, &all, |x| match alias_of(&x.0) {
                Some(a) => format!("{} {a}", x.0),
                None => x.0.clone(),
            })
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

    /// Enter runs the highlighted command (arguments are always optional);
    /// Tab fills it in so arguments can follow.
    fn accept_completion(&mut self, submit: bool) {
        let Some(c) = self.completion.take() else {
            return;
        };
        let Some((name, _, takes_args)) = c.items.get(c.selected).cloned() else {
            return;
        };
        self.editor.clear();
        if submit {
            self.editor.remember(&format!("/{name}"));
            self.record_history();
            self.slash(&name);
        } else if takes_args {
            self.editor.insert_str(&format!("/{name} "));
        } else {
            self.editor.insert_str(&format!("/{name}"));
        }
        self.dirty = true;
    }

    // ---- pickers ---------------------------------------------------------

    fn open_model_picker(&mut self, query: &str, favorite: Option<String>, force_refresh: bool) {
        let mut stale = force_refresh;
        if self.catalogue.is_none() {
            match models::load_cached() {
                Some((m, age)) => {
                    stale |= age.as_secs() > 24 * 3600;
                    self.catalogue = Some(m);
                }
                None => stale = true,
            }
        }
        let pal = &self.pal;
        let show_modalities = self.settings().layout.show_modalities;
        let m = &self.settings().model;
        let current = m.id.clone();
        // start on the model the favorite already points at, else the current one
        let preselect = favorite
            .as_ref()
            .and_then(|k| m.favorites.get(k))
            .map(|f| f.id().to_string())
            .unwrap_or_else(|| current.clone());
        let rows: Vec<Row> = self
            .catalogue
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|m| {
                let ctx = if m.context_length >= 1000 {
                    format!("{}k", m.context_length / 1000)
                } else {
                    m.context_length.to_string()
                };
                let mut row = Row {
                    style: None,
                    id: m.id.clone(),
                    search: format!("{} {}", m.id, m.name),
                    label: if m.id == current {
                        format!("{} •", m.id)
                    } else {
                        m.id.clone()
                    },
                    cols: vec![
                        (format!("{ctx:>6} "), pal.dim()),
                        (
                            format!("${:>6.2}/${:<6.2}", m.prompt_per_m, m.completion_per_m),
                            pal.dim(),
                        ),
                        (
                            if m.tools { " tools" } else { "      " }.into(),
                            Style::default().fg(pal.tool),
                        ),
                        (
                            if m.reasoning { " think" } else { "      " }.into(),
                            Style::default().fg(pal.reasoning),
                        ),
                    ],
                };
                if show_modalities {
                    row.cols.push((" ".into(), pal.dim()));
                    let on_style = Style::default().fg(pal.accent);
                    for (name, icon) in models::MODALITIES {
                        let on = m.accepts(name);
                        row.cols.push((
                            if on { (*icon).to_string() } else { "·".into() },
                            if on { on_style } else { pal.dim() },
                        ));
                    }
                    row.cols.push((models::MODALITY_ARROW.into(), pal.dim()));
                    for (name, icon) in models::MODALITIES {
                        let on = m.produces(name);
                        row.cols.push((
                            if on { (*icon).to_string() } else { "·".into() },
                            if on { on_style } else { pal.dim() },
                        ));
                    }
                }
                row
            })
            .collect();
        let title = match &favorite {
            Some(k) => format!("model for ★{k} · Enter select · Esc close · Ctrl-R refresh"),
            None => "model · Enter select · Esc close · Ctrl-R refresh".to_string(),
        };
        let mut p = Picker::new(Kind::Model { favorite }, &title, query, rows);
        if query.is_empty() {
            p.selected = p
                .results
                .iter()
                .position(|&i| p.rows[i].id == preselect)
                .unwrap_or(0);
        }
        p.hint = "$/M tokens in/out".into();
        if stale {
            p.loading = true;
            if p.rows.is_empty() {
                p.hint = "fetching model list from OpenRouter…".into();
            }
        }
        self.picker = Some(p);
        self.dirty = true;
        if stale {
            self.fetch_models();
        }
    }

    fn open_effort_picker(&mut self, model: Option<String>, favorite: Option<String>) {
        let m = &self.settings().model;
        let current = favorite
            .as_ref()
            .and_then(|k| m.favorites.get(k))
            .and_then(|f| f.effort())
            .or(m.effort())
            .unwrap_or("off")
            .to_string();
        let pal = &self.pal;
        let rows: Vec<Row> = picker::EFFORTS
            .iter()
            .map(|(name, desc)| Row {
                style: None,
                id: name.to_string(),
                search: name.to_string(),
                label: if *name == current {
                    format!("{name} •")
                } else {
                    name.to_string()
                },
                cols: vec![(format!("{desc:<24}"), pal.dim())],
            })
            .collect();
        let title = match (&model, &favorite) {
            (Some(m), Some(k)) => format!("effort for ★{k} ({m})"),
            (Some(m), None) => format!("reasoning effort for {m}"),
            (None, _) => "reasoning effort".to_string(),
        };
        let mut p = Picker::new(Kind::Effort { model, favorite }, &title, "", rows);
        p.selected = picker::EFFORTS
            .iter()
            .position(|(n, _)| *n == current)
            .unwrap_or(0);
        self.picker = Some(p);
        self.dirty = true;
    }

    fn open_favorites_picker(&mut self) {
        let pal = &self.pal;
        let cur = self.favorite_index();
        let mut rows: Vec<Row> = self
            .settings()
            .model
            .favorites
            .iter()
            .enumerate()
            .map(|(i, (k, f))| Row {
                style: None,
                id: k.clone(),
                search: format!("{k} {}", f.id()),
                label: if Some(i) == cur {
                    format!("★ {k} •")
                } else {
                    format!("★ {k}")
                },
                cols: vec![
                    (format!("{:<44.44} ", f.id()), pal.dim()),
                    (
                        format!("{:<8}", f.effort().unwrap_or("")),
                        Style::default().fg(pal.reasoning),
                    ),
                    (
                        format!("{:<12}", self.icons_for(f.id())),
                        Style::default().fg(pal.accent),
                    ),
                ],
            })
            .collect();
        rows.push(Row {
            style: None,
            id: NEW_FAVORITE.into(),
            search: "new favorite".into(),
            label: "+ new favorite…".into(),
            cols: Vec::new(),
        });
        let mut p = Picker::new(
            Kind::Favorites,
            "favorites · Enter use · n new · m model · e effort · r rename · d remove",
            "",
            rows,
        );
        p.hotkeys = true;
        p.hint = "Shift-Tab cycles favorites in this order · Esc close".into();
        p.selected = cur.unwrap_or(0);
        self.picker = Some(p);
        self.dirty = true;
    }

    fn open_name_picker(&mut self, rename: Option<String>) {
        let title = match &rename {
            Some(k) => format!("rename ★{k}"),
            None => "new favorite".to_string(),
        };
        let query = rename.clone().unwrap_or_default();
        let mut p = Picker::new(Kind::Name { rename }, &title, &query, Vec::new());
        p.hint = "name it (fast, pro, …); Enter picks the model".into();
        self.picker = Some(p);
        self.dirty = true;
    }

    fn open_session_name_picker(&mut self) {
        let query = self.session_name.clone().unwrap_or_default();
        let mut p = Picker::new(Kind::SessionName, "rename session", &query, Vec::new());
        p.hint = "Enter saves; empty removes the name".into();
        self.picker = Some(p);
        self.dirty = true;
    }

    fn open_skills_picker(&mut self, query: &str) {
        let pal = &self.pal;
        let skills = ah_core::skills::load(std::path::Path::new(&self.cwd));
        let rows: Vec<Row> = skills
            .iter()
            .map(|s| Row {
                style: None,
                id: s.name.clone(),
                search: format!("{} {}", s.name, s.description),
                label: s.name.clone(),
                cols: vec![
                    (format!("{:<40.40} ", s.description), pal.dim()),
                    (
                        format!("{:<7}", s.scope),
                        Style::default().fg(if s.scope == "project" {
                            pal.user
                        } else {
                            pal.tool
                        }),
                    ),
                ],
            })
            .collect();
        let mut p = Picker::new(Kind::Skills, "skills · Enter run · Esc close", query, rows);
        p.hint = if p.rows.is_empty() {
            format!(
                "no skills; add <name>.md under {} or .ah/skills",
                ah_core::paths::config_dir().join("skills").display()
            )
        } else {
            "$ARGUMENTS in a skill asks for arguments".into()
        };
        self.picker = Some(p);
        self.dirty = true;
    }

    fn open_skill_args_picker(&mut self, name: &str) {
        let mut p = Picker::new(
            Kind::SkillArgs { name: name.into() },
            &format!("arguments for {name}"),
            "",
            Vec::new(),
        );
        p.hint = "replaces $ARGUMENTS; Enter runs".into();
        self.picker = Some(p);
        self.dirty = true;
    }

    /// Send a skill. With `ask`, a skill that wants arguments and got none
    /// prompts for them first.
    fn run_skill(&mut self, name: &str, args: &str, ask: bool) {
        let skills = ah_core::skills::load(std::path::Path::new(&self.cwd));
        let Some(s) = ah_core::skills::find(&skills, name) else {
            self.push(Block::Notice(format!(
                "no skill named {name:?} (try /skills)"
            )));
            return;
        };
        if ask && s.takes_args() && args.trim().is_empty() {
            self.open_skill_args_picker(name);
            return;
        }
        self.submit(s.render(args));
    }

    fn open_session_picker(&mut self, query: &str) {
        let pal = &self.pal;
        let rows: Vec<Row> = ah_core::session::summaries()
            .into_iter()
            .filter(|s| s.id != self.session_id)
            .map(|s| {
                let title = if s.title.is_empty() {
                    "(no messages)".to_string()
                } else {
                    s.title.clone()
                };
                let (label, style) = match &s.name {
                    Some(n) => (n.clone(), Some(Style::default().fg(pal.accent))),
                    None => (title.clone(), None),
                };
                let short_cwd = std::path::Path::new(&s.cwd)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let model = s.model.rsplit('/').next().unwrap_or(&s.model).to_string();
                Row {
                    style,
                    id: s.id.clone(),
                    search: format!("{label} {title} {short_cwd} {model} {}", s.id),
                    label,
                    cols: vec![
                        (format!("{:>8} ", picker::age(s.started_ms)), pal.dim()),
                        (format!("{:>4} msg ", s.messages), pal.dim()),
                        (
                            format!("{short_cwd:<16.16} "),
                            Style::default().fg(pal.user),
                        ),
                        (format!("{model:<20.20}"), pal.dim()),
                    ],
                }
            })
            .collect();
        let mut p = Picker::new(
            Kind::Session,
            "resume · Enter switch · Esc close",
            query,
            rows,
        );
        p.hint = if p.rows.is_empty() {
            "no previous sessions".into()
        } else {
            format!("sessions in {}", ah_core::paths::sessions_dir().display())
        };
        self.picker = Some(p);
        self.dirty = true;
    }

    fn picker_key(&mut self, k: KeyEvent) {
        let Some(p) = self.picker.as_mut() else {
            return;
        };
        self.dirty = true;
        match p.key(k) {
            Action::None => {}
            Action::Close => self.picker = None,
            Action::Key(c) => self.picker_shortcut(c),
            Action::Accept => {
                let chosen = p.current().map(|r| r.id.clone());
                let Some(p) = self.picker.take() else { return };
                let query = p.query.trim().to_string();
                match p.kind {
                    Kind::Model { favorite } => {
                        // no match: accept a raw id
                        let id = chosen.or_else(|| {
                            (!query.is_empty() && query.contains('/')).then(|| query.clone())
                        });
                        if let Some(id) = id {
                            self.pick_model(&id, favorite);
                        }
                    }
                    Kind::Session => {
                        if let Some(id) = chosen {
                            self.resume(&id);
                        }
                    }
                    Kind::Effort { model, favorite } => {
                        if let Some(level) = chosen {
                            match (model, favorite) {
                                (Some(id), Some(name)) => {
                                    self.save_favorite(&name, &id, Some(&level))
                                }
                                (Some(id), None) => self.set_model(&id, Some(&level)),
                                (None, _) => self.set_effort(&level),
                            }
                        }
                    }
                    Kind::Favorites => match chosen.as_deref() {
                        Some(NEW_FAVORITE) => self.open_name_picker(None),
                        Some(name) => self.use_favorite(name),
                        None => {}
                    },
                    Kind::SessionName => self.rename_session(&query),
                    Kind::Skills => {
                        if let Some(name) = chosen {
                            self.run_skill(&name, "", true);
                        }
                    }
                    Kind::SkillArgs { name } => self.run_skill(&name, &query, false),
                    Kind::Name { rename } => {
                        if query.is_empty() || query.starts_with('/') {
                            self.open_name_picker(rename);
                            return;
                        }
                        match rename {
                            Some(old) => self.rename_favorite(&old, &query),
                            None => self.open_model_picker("", Some(query), false),
                        }
                    }
                }
            }
        }
    }

    fn picker_shortcut(&mut self, c: char) {
        let Some(p) = self.picker.as_ref() else {
            return;
        };
        match (&p.kind, c) {
            (Kind::Model { favorite }, 'r') => {
                let (q, fav) = (p.query.clone(), favorite.clone());
                self.open_model_picker(&q, fav, true);
            }
            (Kind::Favorites, 'n') => self.open_name_picker(None),
            (Kind::Favorites, c) => {
                let Some(name) = p
                    .current()
                    .map(|r| r.id.clone())
                    .filter(|n| n != NEW_FAVORITE)
                else {
                    return;
                };
                let model = self
                    .settings()
                    .model
                    .favorites
                    .get(&name)
                    .map(|f| f.id().to_string());
                match c {
                    'm' => self.open_model_picker("", Some(name), false),
                    'e' => self.open_effort_picker(model, Some(name)),
                    'r' => self.open_name_picker(Some(name)),
                    'd' => {
                        self.unfavorite(&name);
                        self.open_favorites_picker();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn fetch_usage(&mut self) {
        let Some(p) = self.usage_pane.as_mut() else {
            return;
        };
        if p.loading && p.fetched.is_some() {
            return;
        }
        p.loading = true;
        self.dirty = true;
        let tx = self.self_tx.clone();
        let base = self.settings().model.base_url.clone();
        let key = ah_core::auth::api_key(self.settings().model.api_key.as_deref());
        std::thread::Builder::new()
            .name("ah-usage".into())
            .spawn(move || {
                let r = match key {
                    None => Err("no API key".to_string()),
                    Some(k) => ah_core::auth::account(&base, &k)
                        .map_err(|e| e.to_string())
                        .map(|account| usage::Remote {
                            account,
                            top: ah_core::auth::activity(&base, &k).map_err(|e| e.to_string()),
                        }),
                };
                let _ = tx.send(Msg::Usage(r));
            })
            .ok();
    }

    /// A model was chosen: ask for the effort when it reasons, else apply.
    fn pick_model(&mut self, id: &str, favorite: Option<String>) {
        if self.model_reasons(id) {
            self.open_effort_picker(Some(id.to_string()), favorite);
        } else {
            match favorite {
                // pin "off" so cycling to it never inherits another model's effort
                Some(name) => self.save_favorite(&name, id, Some("off")),
                None => self.set_model(id, None),
            }
        }
    }

    fn catalogue_has(&mut self, id: &str) -> bool {
        if self.catalogue.is_none() {
            self.catalogue = models::load_cached().map(|(m, _)| m);
        }
        self.catalogue
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|m| m.id == id)
    }

    /// Whether the catalogue says `id` supports reasoning. Unknown ids: `false`.
    fn model_reasons(&mut self, id: &str) -> bool {
        if self.catalogue.is_none() {
            self.catalogue = models::load_cached().map(|(m, _)| m);
        }
        self.catalogue
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|m| m.id == id && m.reasoning)
    }

    fn effort_patch(level: &str) -> serde_json::Value {
        if level == "off" {
            serde_json::Value::Null
        } else {
            serde_json::json!({"effort": level})
        }
    }

    /// Switch model, optionally pinning a reasoning effort (`"off"` clears it).
    fn set_model(&mut self, id: &str, effort: Option<&str>) {
        let mut patch = serde_json::json!({"model": {"id": id}});
        if let Some(e) = effort {
            patch["model"]["reasoning"] = Self::effort_patch(e);
        }
        self.apply_patch(Origin::Runtime("slash".into()), patch);
        let favorite = self
            .favorite_index()
            .and_then(|i| self.settings().model.favorites.keys().nth(i).cloned())
            .map(|k| format!(" ★{k}"))
            .unwrap_or_default();
        let effort = match self.settings().model.effort() {
            Some(e) => format!(" ({e})"),
            None => String::new(),
        };
        // repeated switches (Shift-Tab) update one line instead of stacking
        if matches!(self.entries.last().map(|e| &e.block), Some(Block::Notice(t)) if t.starts_with("model → "))
        {
            self.entries.pop();
        }
        self.push(Block::Notice(format!("model → {id}{effort}{favorite}")));
        self.request_status();
    }

    fn set_effort(&mut self, level: &str) {
        if !picker::EFFORTS.iter().any(|(n, _)| *n == level) {
            self.push(Block::Error(format!(
                "unknown effort `{level}` (off, minimal, low, medium, high, xhigh)"
            )));
            return;
        }
        let id = self.settings().model.id.clone();
        if level != "off" && !self.model_reasons(&id) {
            self.push(Block::Notice(format!(
                "{id} is not listed as a reasoning model; sending effort anyway"
            )));
        }
        self.apply_patch(
            Origin::Runtime("slash".into()),
            serde_json::json!({"model": {"reasoning": Self::effort_patch(level)}}),
        );
        self.push(Block::Notice(format!("reasoning effort → {level}")));
        self.request_status();
    }

    // ---- favorites -------------------------------------------------------

    /// Index into `model.favorites` (key order) of the entry matching the
    /// current model and effort.
    fn favorite_index(&self) -> Option<usize> {
        let m = &self.settings().model;
        let effort = m.effort().unwrap_or("off");
        let exact = m
            .favorites
            .values()
            .position(|f| f.id() == m.id && f.effort().is_none_or(|e| e == effort));
        exact.or_else(|| m.favorites.values().position(|f| f.id() == m.id))
    }

    fn cycle_model(&mut self) {
        let favorites = &self.settings().model.favorites;
        if favorites.is_empty() {
            self.push(Block::Notice("no favorites yet: /favorite adds one".into()));
            return;
        }
        let next = self
            .favorite_index()
            .map(|i| (i + 1) % favorites.len())
            .unwrap_or(0);
        let (_, f) = favorites.iter().nth(next).expect("index in range");
        let (id, effort) = (f.id().to_string(), f.effort().map(str::to_string));
        self.set_model(&id, effort.as_deref());
    }

    fn use_favorite(&mut self, name: &str) {
        if let Some(f) = self.settings().model.favorites.get(name).cloned() {
            self.set_model(f.id(), f.effort());
        }
    }

    /// Store `name = model[/effort]` in favorites.toml and switch to it.
    fn save_favorite(&mut self, name: &str, id: &str, effort: Option<&str>) {
        let fav = match effort {
            Some(e) => Favorite::Full {
                id: id.to_string(),
                effort: Some(e.to_string()),
            },
            None => Favorite::Id(id.to_string()),
        };
        let mut file = self.load_favorites_file();
        file.insert(name.to_string(), fav.clone());
        if let Err(e) = ah_core::settings::save_favorites(&file) {
            self.push(Block::Error(format!("could not save favorites: {e}")));
        }
        self.apply_patch(
            Origin::Runtime("slash".into()),
            serde_json::json!({"model": {"favorites": {name: fav}}}),
        );
        self.set_model(id, effort);
    }

    fn rename_favorite(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        let Some(fav) = self.settings().model.favorites.get(old).cloned() else {
            return;
        };
        let mut file = self.load_favorites_file();
        file.remove(old);
        file.insert(new.to_string(), fav.clone());
        if let Err(e) = ah_core::settings::save_favorites(&file) {
            self.push(Block::Error(format!("could not save favorites: {e}")));
        }
        self.apply_patch(
            Origin::Runtime("slash".into()),
            serde_json::json!({"model": {"favorites": {old: null, new: fav}}}),
        );
        self.push(Block::Notice(format!("★ {old} → ★ {new}")));
        self.request_status();
    }

    fn unfavorite(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() || !self.settings().model.favorites.contains_key(name) {
            self.push(Block::Notice(format!("no favorite named `{name}`")));
            return;
        }
        let mut file = self.load_favorites_file();
        if file.remove(name).is_some() {
            if let Err(e) = ah_core::settings::save_favorites(&file) {
                self.push(Block::Error(format!("could not save favorites: {e}")));
            }
        } else {
            self.push(Block::Notice(format!(
                "{name} comes from a config file; removed for this session only"
            )));
        }
        self.apply_patch(
            Origin::Runtime("slash".into()),
            serde_json::json!({"model": {"favorites": {name: null}}}),
        );
        self.push(Block::Notice(format!("removed ★ {name}")));
        self.request_status();
    }

    fn load_favorites_file(&self) -> std::collections::BTreeMap<String, Favorite> {
        std::fs::read_to_string(ah_core::paths::favorites_file())
            .ok()
            .and_then(|t| ah_core::settings::parse_toml_patch(&t).ok())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    /// What to pass to `ah -r` to get this conversation back; `None` when
    /// nothing was said, since an empty session is not listed anyway.
    /// `/init`: have the model write (or refresh) the project's AGENTS.md.
    fn init_instructions(&mut self) {
        let path = std::path::Path::new(&self.cwd).join("AGENTS.md");
        let exists = path.is_file();
        let names = self.settings().prompt.instructions.join(", ");
        let mut p = format!(
            "Study this repository and {} `{}`, the instruction file coding agents read \
             before working here (this harness loads {names} from the repository root, \
             every directory down to the working directory, and .ah/). Cover: what the \
             project is, how to build, test and lint it with exact commands, where things \
             live, conventions and style rules that are not obvious from the code, and \
             anything an agent must not do. Plain markdown, under 60 lines, no filler.",
            if exists { "update" } else { "write" },
            path.display()
        );
        if exists {
            p.push_str(" Keep what is still true and fix what is not; do not drop sections you cannot verify.");
        }
        self.submit(p);
    }

    fn resume_hint(&self) -> Option<String> {
        let spoke = self
            .entries
            .iter()
            .any(|e| matches!(e.block, Block::User(_)));
        if !spoke {
            return None;
        }
        Some(match &self.session_name {
            Some(n) if !n.contains(char::is_whitespace) => n.clone(),
            Some(n) => format!("{n:?}"),
            None => self.session_id.clone(),
        })
    }

    fn rename_session(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() && self.session_name.is_none() {
            return;
        }
        let _ = self.tx.send(EngineCmd::Rename(name.to_string()));
    }

    fn resume(&mut self, id: &str) {
        if self.busy {
            self.push(Block::Notice("busy; wait or press Esc to cancel".into()));
            return;
        }
        let _ = self.tx.send(EngineCmd::Resume(id.to_string()));
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
                    self.drain_queue();
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
            UiEvent::Renamed(name) => {
                self.session_name = name;
                let s = match &self.session_name {
                    Some(n) => format!("session named \u{201c}{n}\u{201d}"),
                    None => "session name removed".into(),
                };
                self.push(Block::Notice(s));
            }
            UiEvent::Resumed { id, name, messages } => {
                self.entries.clear();
                self.usage = Usage::default();
                self.stats = Stats::default();
                self.context_tokens = 0;
                self.session_id = id;
                self.session_name = name;
                self.follow = true;
                self.load_history(&messages);
                self.request_status();
                self.dirty = true;
            }
        }
    }

    /// Stamp the reasoning time on the reply being streamed.
    fn finish_thinking(&mut self) {
        if let Some(t) = self.think_start.take()
            && let Some(Block::Assistant { think_ms, .. }) =
                self.entries.last_mut().map(|e| &mut e.block)
        {
            *think_ms = t.elapsed().as_millis() as u64;
        }
    }

    fn handle_agent(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::RequestStart { .. } => {
                self.stats.request_start();
                self.set_state(State::Thinking);
                self.think_start = None;
                self.push(Block::Assistant {
                    text: String::new(),
                    reasoning: String::new(),
                    streaming: true,
                    think_ms: 0,
                });
            }
            AgentEvent::Text(t) => {
                self.set_state(State::Streaming);
                self.finish_thinking();
                if let Some(Block::Assistant { text, .. }) =
                    self.entries.last_mut().map(|e| &mut e.block)
                {
                    text.push_str(&t);
                }
                self.dirty = true;
            }
            AgentEvent::Reasoning(t) => {
                self.set_state(State::Streaming);
                self.think_start.get_or_insert_with(Instant::now);
                if let Some(Block::Assistant { reasoning, .. }) =
                    self.entries.last_mut().map(|e| &mut e.block)
                {
                    reasoning.push_str(&t);
                }
                self.dirty = true;
            }
            AgentEvent::AssistantMessage(m) => {
                self.stats.request_end();
                self.finish_thinking();
                if let Some(Block::Assistant {
                    text,
                    reasoning,
                    streaming,
                    ..
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
                self.context_tokens = u.prompt_tokens + u.completion_tokens;
                let model = self.settings().model.id.clone();
                self.stats.add_usage(&model, &u);
                self.dirty = true;
            }
            AgentEvent::ToolStart(call) => {
                self.stats.tool_calls += 1;
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
                if let Some(d) = &result.diff {
                    self.stats.add_diff(d);
                }
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
            AgentEvent::Error(e) => {
                self.stats.request_end();
                self.push(Block::Error(e));
            }
            AgentEvent::Compacted {
                before,
                after,
                summary,
            } => {
                self.context_tokens = after;
                self.push(Block::User(
                    ah_core::agent::summary_message(&summary).content,
                ));
                self.push(Block::Notice(format!(
                    "context compacted: {} → ~{} tokens",
                    usage::tokens(before),
                    usage::tokens(after)
                )));
            }
            AgentEvent::TurnEnd(s) => {
                self.stats.request_end();
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
                let at = (m.column, m.row);
                match m.kind {
                    MouseEventKind::ScrollUp => self.scroll_by(-(step as isize)),
                    MouseEventKind::ScrollDown => self.scroll_by(step as isize),
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.sel = Some(Selection {
                            anchor: at,
                            cur: at,
                            dragging: true,
                        });
                        self.dirty = true;
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(sel) = self.sel.as_mut().filter(|s| s.dragging) {
                            sel.cur = at;
                            self.dirty = true;
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(sel) = self.sel.as_mut() {
                            sel.cur = at;
                            sel.dragging = false;
                            if sel.anchor == sel.cur {
                                self.sel = None;
                            } else {
                                self.copy_pending = true;
                            }
                            self.dirty = true;
                        }
                    }
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
        while self.unqueue_last() {}
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
        self.sel = None;
        let b = self.binds.clone();

        if self.usage_pane.is_some() {
            match (k.code, k.modifiers) {
                (KeyCode::Esc | KeyCode::Char('q'), _)
                | (KeyCode::Char('c'), KeyModifiers::CONTROL) => self.usage_pane = None,
                (KeyCode::Char('r'), _) => self.fetch_usage(),
                _ => {}
            }
            return;
        }
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
            self.submit_editor();
        } else if keys::any_match(&b.paste_image, &k) {
            self.paste_image();
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
        } else if keys::any_match(&b.cycle_model, &k) {
            self.cycle_model();
        } else if keys::any_match(&b.delete_word, &k) {
            self.editor.delete_word();
        } else if keys::any_match(&b.delete_line, &k) {
            self.editor.delete_to_line_start();
        } else if keys::any_match(&b.line_start, &k) {
            self.editor.home();
        } else if keys::any_match(&b.line_end, &k) {
            self.editor.end();
        } else if keys::any_match(&b.history_prev, &k)
            && self.editor.is_empty()
            && !self.queue.is_empty()
        {
            self.unqueue_last();
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
            "submit" => self.submit_editor(),
            "paste_image" => self.paste_image(),
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
                let mut s = String::from("commands:");
                for (n, d, _) in COMMANDS {
                    s.push_str(&format!("\n  /{n:<10} {d}"));
                }
                for (p, c) in &self.plugin_commands {
                    s.push_str(&format!("\n  /{:<10} {} ({p})", c.name, c.description));
                }
                s.push_str("\nkeys: Enter send · Shift/Alt-Enter newline · Esc cancel · Up/Down prompt history · PgUp/PgDn scroll · Shift-Tab next favorite · Ctrl-V paste image · Ctrl-T tool output · Ctrl-R thinking · Ctrl-C quit");
                self.push(Block::Notice(s));
            }
            "model" | "models" => {
                if args == "refresh" {
                    self.open_model_picker("", None, true);
                } else if args.is_empty() {
                    self.open_model_picker("", None, false);
                } else if self.settings().model.favorites.contains_key(args) {
                    self.use_favorite(args);
                } else if self.catalogue_has(args) {
                    self.pick_model(args, None);
                } else {
                    self.open_model_picker(args, None, false);
                }
            }
            "effort" => {
                if args.is_empty() {
                    self.open_effort_picker(None, None);
                } else {
                    self.set_effort(args);
                }
            }
            "favorite" | "fav" => {
                if args.is_empty() {
                    self.open_favorites_picker();
                } else if self.settings().model.favorites.contains_key(args) {
                    self.use_favorite(args);
                } else {
                    self.open_model_picker("", Some(args.to_string()), false);
                }
            }
            "init" => self.init_instructions(),
            "usage" => {
                self.usage_pane = Some(usage::Pane::new());
                self.fetch_usage();
            }
            "skills" | "skill" => {
                let (name, rest) = args
                    .split_once(' ')
                    .map(|(n, a)| (n, a.trim()))
                    .unwrap_or((args, ""));
                if name.is_empty() {
                    self.open_skills_picker("");
                } else {
                    self.run_skill(name, rest, true);
                }
            }
            "rename" => {
                if args.is_empty() {
                    self.open_session_name_picker();
                } else {
                    self.rename_session(args);
                }
            }
            "resume" => {
                if args.is_empty() {
                    self.open_session_picker("");
                } else if let Some(id) = ah_core::session::find(args) {
                    self.resume(&id);
                } else {
                    self.open_session_picker(args);
                }
            }
            "compact" => {
                if self.busy {
                    self.push(Block::Notice("busy; wait or press Esc to cancel".into()));
                } else {
                    self.follow = true;
                    self.busy = true;
                    self.set_state(State::Thinking);
                    let _ = self.tx.send(EngineCmd::Compact(args.to_string()));
                }
            }
            "clear" => {
                self.entries.clear();
                self.usage = Usage::default();
                self.stats = Stats::default();
                self.context_tokens = 0;
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
                    "submit {:?}\nnewline {:?}\ncancel {:?}\nquit {:?}\nscroll {:?}/{:?} page {:?}/{:?}\ntools {:?} reasoning {:?} favorites model {:?}",
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
                    k.cycle_model
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
            "session" => {
                let s = format!(
                    "session {}{} · {}",
                    self.session_id,
                    self.session_name
                        .as_deref()
                        .map(|n| format!(" \u{201c}{n}\u{201d}"))
                        .unwrap_or_default(),
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
        let side = if pal.has_side_borders() { 2 } else { 0 };
        let prefix_w = unicode_width::UnicodeWidthStr::width(pal.input_prefix.as_str()) as u16;
        let input_width = area.width.saturating_sub(side + prefix_w) as usize;
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
        let queue_rows: u16 = if self.queue.is_empty() {
            0
        } else {
            self.queue.len() as u16 + border
        };

        let [
            transcript_area,
            perm_area,
            queue_area,
            input_area,
            status_area,
        ] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(perm_rows),
            Constraint::Length(queue_rows),
            Constraint::Length(input_rows + border),
            Constraint::Length(status_rows),
        ])
        .areas(area);

        self.draw_transcript(f, transcript_area, &pal, layout.transcript_max_width);
        if !self.queue.is_empty() {
            self.draw_queue(f, queue_area, &pal, layout.paste_collapse_lines);
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
        let placeholder = if self.busy && self.settings().layout.queue_max > 0 {
            "Type the next message; Enter queues it"
        } else {
            "Type a message, /help for commands"
        };
        let block = pal
            .input_block(!self.busy)
            .title(title)
            .style(Style::default().fg(pal.input_fg).bg(pal.input_bg));
        let inner = block.inner(input_area);
        f.render_widget(block, input_area);
        let first_row = cursor_rc
            .0
            .saturating_sub(inner.height.saturating_sub(1) as usize);
        let prefix_style = pal.bold(pal.accent);
        let pad = " ".repeat(prefix_w as usize);
        let mut visible: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(first_row)
            .take(inner.height as usize)
            .map(|(i, r)| {
                let lead = if i == 0 {
                    pal.input_prefix.clone()
                } else {
                    pad.clone()
                };
                Line::from(vec![Span::styled(lead, prefix_style), Span::raw(r.clone())])
            })
            .collect();
        if self.editor.is_empty() {
            visible = vec![Line::from(vec![
                Span::styled(pal.input_prefix.clone(), prefix_style),
                Span::styled(placeholder, pal.dim()),
            ])];
        }
        f.render_widget(Paragraph::new(visible), inner);
        if self.pending_perm.is_none() && self.picker.is_none() && self.usage_pane.is_none() {
            f.set_cursor_position((
                inner.x + prefix_w + cursor_rc.1 as u16,
                inner.y + (cursor_rc.0 - first_row) as u16,
            ));
        }

        if layout.show_status {
            self.draw_status(f, status_area, &pal);
        }
        if let Some(c) = &self.completion {
            self.draw_completion(f, c, input_area, &pal);
        }
        if let Some(p) = &self.picker {
            p.draw(f, area, &pal);
        }
        if let Some(u) = &self.usage_pane {
            let icons = |id: &str| self.icons_for(id);
            let models = usage::Models {
                current: &self.settings().model.id,
                icons: &icons,
            };
            u.draw(f, area, &pal, &self.usage, &self.stats, &models);
        }
        if let Some(sel) = self.sel {
            let text = self.highlight_selection(f, area, sel.anchor, sel.cur);
            if self.copy_pending {
                self.copy_pending = false;
                copy_to_clipboard(&text);
                let n = text.chars().count();
                self.push(Block::Notice(format!("copied {n} chars")));
            }
        }
    }

    /// Reverse the cells between `a` and `b` in reading order and return
    /// their text, rows trimmed and joined with newlines.
    fn highlight_selection(
        &self,
        f: &mut Frame,
        area: Rect,
        a: (u16, u16),
        b: (u16, u16),
    ) -> String {
        let (start, end) = if (a.1, a.0) <= (b.1, b.0) {
            (a, b)
        } else {
            (b, a)
        };
        let buf = f.buffer_mut();
        let mut out = String::new();
        for y in start.1..=end.1.min(area.height.saturating_sub(1)) {
            let x0 = if y == start.1 { start.0 } else { 0 };
            let x1 = if y == end.1 {
                end.0.min(area.width.saturating_sub(1))
            } else {
                area.width.saturating_sub(1)
            };
            let mut row = String::new();
            for x in x0..=x1 {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    row.push_str(cell.symbol());
                    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                }
            }
            if y > start.1 {
                out.push('\n');
            }
            out.push_str(row.trim_end());
        }
        out
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
                let shown = match alias_of(name) {
                    Some(a) => format!("{name} ({a})"),
                    None => name.clone(),
                };
                Line::from(vec![
                    Span::styled(format!(" /{shown:<14}"), name_style),
                    Span::styled(format!(" {desc}"), pal.dim()),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// Queued messages, one per row; long ones collapse like paste chips.
    fn draw_queue(&self, f: &mut Frame, area: Rect, pal: &Palette, collapse_lines: usize) {
        let n = self.queue.len();
        let block = pal
            .input_block(false)
            .title(format!(" queued {n} · Up edits the last one "));
        let inner = block.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        let width = inner.width.saturating_sub(4) as usize;
        let lines: Vec<Line> = self
            .queue
            .iter()
            .enumerate()
            .map(|(i, (t, images))| {
                let lines = t.lines().count();
                let first = t.lines().next().unwrap_or("").trim();
                let mut spans = vec![Span::styled(format!(" {}. ", i + 1), pal.dim())];
                if !images.is_empty() {
                    spans.push(Span::styled(
                        format!(
                            "[{} image{}] ",
                            images.len(),
                            if images.len() == 1 { "" } else { "s" }
                        ),
                        pal.bold(pal.accent),
                    ));
                }
                if lines > collapse_lines.max(1) {
                    spans.push(Span::styled(
                        format!("[{lines} lines] "),
                        pal.bold(pal.accent),
                    ));
                }
                let shown: String = first.chars().take(width).collect();
                spans.push(Span::styled(shown, Style::default().fg(pal.user)));
                Line::from(spans)
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
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

    fn draw_status(&self, f: &mut Frame, area: Rect, pal: &Palette) {
        let ctx = self.status_ctx();
        let mut text = self.plugin_status.clone().unwrap_or(ctx.rendered);
        if self.busy {
            text = format!(" {}{}", self.spinner(), text);
        }
        let style = Style::default().fg(pal.status_fg).bg(pal.status_bg);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(text, style))).style(style),
            area,
        );
    }
}
