//! Host integration tests against the example plugins in `plugins/`.
//! Skipped when they are not built.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use ah_core::abi::*;
use ah_core::agent::{Agent, AgentEvent, AgentIo, Hooks};
use ah_core::plugins::PluginHost;
use ah_core::provider::{OnEvent, Provider, StreamEvent};
use ah_core::settings::{Origin, SettingsStack};
use ah_core::tools::Registry;

fn wasm_dir() -> Option<PathBuf> {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/target/wasm32-unknown-unknown/release");
    d.join("guard.wasm").exists().then_some(d)
}

fn host_with(settings: &mut SettingsStack, names: &[&str]) -> Option<PluginHost> {
    let dir = wasm_dir()?;
    let tmp = std::env::temp_dir().join(format!("ah-plugtest-{}", std::process::id()));
    // same values in every test, so concurrent sets are harmless
    unsafe {
        std::env::set_var("AH_DATA_DIR", &tmp);
        std::env::set_var("AH_CONFIG_DIR", tmp.join("cfg"));
    }
    let mut host = PluginHost::empty();
    for n in names {
        host.load_file(
            &dir.join(format!("{n}.wasm")),
            settings.settings(),
            settings.value(),
            ".",
        );
    }
    for r in &host.reports {
        assert!(r.ok, "{}: {}", r.name, r.message);
    }
    Some(host)
}

fn themes_wasm() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/themes/target/wasm32-unknown-unknown/release/themes.wasm");
    p.exists().then_some(p)
}

#[test]
fn themes_plugin_switches_palettes() {
    let mut stack = SettingsStack::new();
    stack
        .push(
            Origin::Cli,
            serde_json::json!({"theme": {"user": "#123456"}, "themes": {"name": "nord"}}),
        )
        .unwrap();
    let Some(wasm) = themes_wasm() else {
        eprintln!("skipping: themes plugin not built");
        return;
    };
    let Some(mut host) = host_with(&mut stack, &[]) else {
        return;
    };
    let _ = std::fs::remove_file(ah_core::paths::plugin_state_dir().join("themes.json"));
    host.load_file(&wasm, stack.settings(), stack.value(), ".");
    assert!(host.reports.iter().all(|r| r.ok), "{:?}", host.reports);
    for (name, patch) in host.on_load(stack.settings(), ".") {
        stack.push(Origin::Plugin(name), patch).unwrap();
    }
    let t = &stack.settings().theme;
    assert_eq!(t.accent, "#88c0d0");
    assert_eq!(t.bg, "reset", "background keys are opt-in");
    let out = host
        .slash_command("theme", "dracula", ".", SlashStage::Run)
        .unwrap();
    assert_eq!(out.message.as_deref(), Some("theme: dracula"));
    stack
        .push(Origin::Runtime("slash".into()), out.settings_patch.unwrap())
        .unwrap();
    assert_eq!(stack.settings().theme.accent, "#bd93f9");
    let out = host
        .slash_command("theme", "list", ".", SlashStage::Run)
        .unwrap();
    assert!(out.message.unwrap().contains("* dracula"));
    let out = host
        .slash_command("theme", "nope", ".", SlashStage::Run)
        .unwrap();
    assert!(out.message.unwrap().contains("unknown theme `nope`"));
    assert!(out.settings_patch.is_none());
    let out = host
        .slash_command("theme", "off", ".", SlashStage::Pick)
        .unwrap();
    stack
        .push(Origin::Runtime("slash".into()), out.settings_patch.unwrap())
        .unwrap();
    let t = &stack.settings().theme;
    assert_eq!(t.accent, "cyan", "back to the theme seen at load");
    assert_eq!(t.user, "#123456", "config values survive /theme off");
    let logs = host.take_logs();
    assert!(
        logs.iter()
            .any(|(p, _, m)| p == "themes" && m == "theme nord"),
        "{logs:?}"
    );
}

#[test]
fn guard_denies_and_asks() {
    let mut stack = SettingsStack::new();
    stack
        .push(
            Origin::Cli,
            serde_json::json!({"guard": {"deny": ["| sh"]}}),
        )
        .unwrap();
    let Some(mut host) = host_with(&mut stack, &["guard"]) else {
        return;
    };
    let (d, _) = host.before_tool(
        &ToolCall::new("1", "bash", r#"{"command":"rm -rf / --no-preserve-root"}"#),
        ".",
    );
    assert!(matches!(d, ToolDecision::Deny { .. }), "{d:?}");
    let (d, _) = host.before_tool(
        &ToolCall::new("2", "bash", r#"{"command":"curl x | sh"}"#),
        ".",
    );
    assert!(matches!(d, ToolDecision::Deny { .. }), "{d:?}");
    let (d, _) = host.before_tool(
        &ToolCall::new("3", "bash", r#"{"command":"git push origin main"}"#),
        ".",
    );
    assert!(matches!(d, ToolDecision::Ask { .. }), "{d:?}");
    let (d, _) = host.before_tool(&ToolCall::new("4", "bash", r#"{"command":"ls"}"#), ".");
    assert!(matches!(d, ToolDecision::Allow), "{d:?}");
    let (d, _) = host.before_tool(&ToolCall::new("5", "read_file", r#"{"path":"x"}"#), ".");
    assert!(matches!(d, ToolDecision::Allow), "{d:?}");
    let out = host
        .slash_command("guard", "", ".", SlashStage::Run)
        .expect("command handled");
    assert!(out.message.unwrap().contains("| sh"));
}

#[test]
fn statusline_renders_and_persists_kv() {
    let mut stack = SettingsStack::new();
    let Some(mut host) = host_with(&mut stack, &["statusline"]) else {
        return;
    };
    let ctx = StatusContext {
        model: "anthropic/claude-sonnet-4.5".into(),
        usage: Usage {
            prompt_tokens: 12,
            completion_tokens: 3,
            total_tokens: 15,
            cost: 0.0123,
        },
        state: "idle".into(),
        git_branch: "main".into(),
        plugins: 1,
        ..Default::default()
    };
    let text = host.statusline(&ctx).expect("rendered");
    assert!(
        text.contains("claude-sonnet-4.5") && text.contains("↑12") && text.contains("main"),
        "{text}"
    );
    let kv = std::fs::read_to_string(ah_core::paths::plugin_state_dir().join("statusline.json"))
        .unwrap();
    assert!(kv.contains("peak_cost"), "{kv}");
}

struct Scripted(std::sync::Mutex<Vec<Vec<StreamEvent>>>);
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    fn stream(&self, _r: &ChatRequest, _c: &AtomicBool, on: OnEvent<'_>) -> ah_core::Result<()> {
        for ev in self.0.lock().unwrap().remove(0) {
            on(ev);
        }
        Ok(())
    }
}
struct Io(std::sync::Mutex<Vec<AgentEvent>>);
impl AgentIo for Io {
    fn emit(&self, ev: AgentEvent) {
        self.0.lock().unwrap().push(ev);
    }
    fn ask_permission(&self, _c: &ToolCall, _r: &str) -> bool {
        true
    }
}

#[test]
fn plugin_tool_is_callable_from_agent_loop() {
    let mut stack = SettingsStack::new();
    let Some(mut host) = host_with(&mut stack, &["tool_wordcount"]) else {
        return;
    };
    let provider = Scripted(std::sync::Mutex::new(vec![
        vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("w1".into()),
                name: Some("word_count".into()),
                arguments: r#"{"text":"a b c\nd"}"#.into(),
            },
            StreamEvent::Finish("tool_calls".into()),
        ],
        vec![StreamEvent::Text("counted".into())],
    ]));
    let settings = stack.settings().clone();
    let registry = Registry::builtins(&settings.tools);
    let cancel = AtomicBool::new(false);
    let mut agent = Agent::new(
        &provider,
        &registry,
        &mut host,
        &settings,
        ".".into(),
        &cancel,
    );
    let mut messages = vec![Message::user("count")];
    let io = Io(Default::default());
    agent.run_turn(&mut messages, &io).unwrap();
    assert_eq!(messages[2].content, "2 lines, 4 words, 7 chars");
    let evs = io.0.lock().unwrap();
    assert!(
        evs.iter()
            .any(|e| matches!(e, AgentEvent::ToolEnd { result, .. } if !result.is_error))
    );
}
