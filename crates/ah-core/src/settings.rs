//! Settings layers: defaults, user TOML, project TOML, plugin patches, CLI.

use std::path::{Path, PathBuf};

use ah_abi::{Settings, merge_patch};
use serde_json::Value;

use crate::{Error, Result};

/// Where a layer came from, for `ah config show --origins` style output.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    Defaults,
    File(PathBuf),
    Plugin(String),
    Cli,
    Runtime(String),
}

#[derive(Debug, Clone)]
pub struct Layer {
    pub origin: Origin,
    pub patch: Value,
}

/// Ordered stack of merge patches.
#[derive(Debug, Clone)]
pub struct SettingsStack {
    layers: Vec<Layer>,
    resolved: Settings,
    resolved_value: Value,
}

impl Default for SettingsStack {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsStack {
    pub fn new() -> Self {
        let defaults = serde_json::to_value(Settings::default()).expect("defaults serialise");
        let mut s = Self {
            layers: Vec::new(),
            resolved: Settings::default(),
            resolved_value: defaults.clone(),
        };
        s.layers.push(Layer {
            origin: Origin::Defaults,
            patch: defaults,
        });
        s
    }

    /// Defaults + user config + project config.
    pub fn from_files() -> Result<Self> {
        let mut s = Self::new();
        for path in [
            crate::paths::user_config_file(),
            crate::paths::project_config_file(),
        ] {
            if path.is_file() {
                s.push_file(&path)?;
            }
        }
        Ok(s)
    }

    pub fn push_file(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path)?;
        let patch = parse_toml_patch(&text)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        self.push(Origin::File(path.to_path_buf()), patch)
    }

    pub fn push(&mut self, origin: Origin, patch: Value) -> Result<()> {
        self.layers.push(Layer { origin, patch });
        self.resolve()
    }

    /// Remove all layers with a matching origin kind (used by `/reload` to drop
    /// stale plugin patches before re-running `on_load`).
    pub fn retain(&mut self, keep: impl Fn(&Origin) -> bool) -> Result<()> {
        self.layers
            .retain(|l| matches!(l.origin, Origin::Defaults) || keep(&l.origin));
        self.resolve()
    }

    fn resolve(&mut self) -> Result<()> {
        let mut v = Value::Null;
        for l in &self.layers {
            merge_patch(&mut v, &l.patch);
        }
        let s: Settings =
            serde_json::from_value(v.clone()).map_err(|e| Error::Config(e.to_string()))?;
        self.resolved = s;
        self.resolved_value = v;
        Ok(())
    }

    pub fn settings(&self) -> &Settings {
        &self.resolved
    }

    pub fn value(&self) -> &Value {
        &self.resolved_value
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Render the merged settings as TOML.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(&self.resolved)
            .unwrap_or_else(|e| format!("# failed to render: {e}"))
    }
}

/// TOML → JSON value. TOML has no null so removal is spelled `key = "__unset__"`.
pub fn parse_toml_patch(text: &str) -> std::result::Result<Value, toml::de::Error> {
    let t: toml::Value = toml::from_str(text)?;
    let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
    unset_markers(&mut v);
    Ok(v)
}

fn unset_markers(v: &mut Value) {
    match v {
        Value::Object(m) => m.values_mut().for_each(unset_markers),
        Value::Array(a) => a.iter_mut().for_each(unset_markers),
        Value::String(s) if s == "__unset__" => *v = Value::Null,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_merge_in_order() {
        let mut s = SettingsStack::new();
        s.push(
            Origin::File("a".into()),
            parse_toml_patch("[theme]\naccent = \"red\"\n[layout]\ninput_height = 5").unwrap(),
        )
        .unwrap();
        s.push(
            Origin::Plugin("p".into()),
            serde_json::json!({"theme": {"accent": "blue"}}),
        )
        .unwrap();
        assert_eq!(s.settings().theme.accent, "blue");
        assert_eq!(s.settings().layout.input_height, 5);
        s.retain(|o| !matches!(o, Origin::Plugin(_))).unwrap();
        assert_eq!(s.settings().theme.accent, "red");
        assert!(s.to_toml().contains("accent = \"red\""));
    }

    #[test]
    fn bad_type_reports_config_error() {
        let mut s = SettingsStack::new();
        let e = s
            .push(
                Origin::Cli,
                serde_json::json!({"layout": {"input_height": "tall"}}),
            )
            .unwrap_err();
        assert!(matches!(e, Error::Config(_)));
    }
}
