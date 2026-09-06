//! Cached OpenRouter model catalogue and a fuzzy matcher.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context_length: u64,
    /// USD per million tokens.
    #[serde(default)]
    pub prompt_per_m: f64,
    #[serde(default)]
    pub completion_per_m: f64,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub reasoning: bool,
    /// `text`, `image`, `audio`, `video`, `file` as OpenRouter reports them.
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
}

impl ModelInfo {
    pub fn accepts(&self, modality: &str) -> bool {
        self.input_modalities.iter().any(|m| m == modality)
    }

    pub fn produces(&self, modality: &str) -> bool {
        self.output_modalities.iter().any(|m| m == modality)
    }

    /// `TIF→T`: input tags, arrow, output tags.
    pub fn modality_icons(&self) -> String {
        modality_icons(&self.input_modalities, &self.output_modalities)
    }
}

/// Modalities in display order with their one-letter tags. Plain ASCII so
/// every terminal font renders them at full size.
pub const MODALITIES: &[(&str, &str)] = &[
    ("text", "T"),
    ("image", "I"),
    ("audio", "A"),
    ("video", "V"),
    ("file", "F"),
];

/// Separates input from output icons.
pub const MODALITY_ARROW: &str = "→";

fn icons(mods: &[String]) -> String {
    MODALITIES
        .iter()
        .filter(|(m, _)| mods.iter().any(|x| x == m))
        .map(|(_, i)| *i)
        .collect()
}

/// `TIF→T`; only the input half when outputs are unknown, empty when both
/// are.
pub fn modality_icons(input: &[String], output: &[String]) -> String {
    let (i, o) = (icons(input), icons(output));
    match (i.is_empty(), o.is_empty()) {
        (true, true) => String::new(),
        (_, true) => i,
        _ => format!("{i}{MODALITY_ARROW}{o}"),
    }
}

/// Bumped when `ModelInfo` gains fields; older caches are refetched.
const CACHE_VERSION: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    version: u32,
    fetched_ms: u64,
    models: Vec<ModelInfo>,
}

pub fn cache_path() -> PathBuf {
    crate::paths::data_dir().join("models.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Cached catalogue and its age, if a cache exists.
pub fn load_cached() -> Option<(Vec<ModelInfo>, Duration)> {
    let text = std::fs::read_to_string(cache_path()).ok()?;
    let c: Cache = serde_json::from_str(&text).ok()?;
    if c.version != CACHE_VERSION {
        return None;
    }
    let age = Duration::from_millis(now_ms().saturating_sub(c.fetched_ms));
    Some((c.models, age))
}

pub fn save_cache(models: &[ModelInfo]) -> Result<()> {
    let p = cache_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let c = Cache {
        version: CACHE_VERSION,
        fetched_ms: now_ms(),
        models: models.to_vec(),
    };
    std::fs::write(&p, serde_json::to_vec(&c)?)?;
    Ok(())
}

/// Fetch `/models` from the API and refresh the cache.
pub fn fetch(base_url: &str, api_key: Option<&str>) -> Result<Vec<ModelInfo>> {
    let client = crate::provider::openrouter::OpenRouter::new(base_url, api_key.unwrap_or(""));
    let v = client.get_json("/models")?;
    let mut models: Vec<ModelInfo> = v["data"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            let price = |k: &str| {
                m["pricing"][k]
                    .as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|p| p * 1e6)
                    .unwrap_or(0.0)
            };
            let params = m["supported_parameters"].as_array();
            let has = |p: &str| params.is_some_and(|a| a.iter().any(|x| x == p));
            Some(ModelInfo {
                id,
                name: m["name"].as_str().unwrap_or("").to_string(),
                context_length: m["context_length"].as_u64().unwrap_or(0),
                prompt_per_m: price("prompt"),
                completion_per_m: price("completion"),
                tools: has("tools"),
                reasoning: has("reasoning") || has("include_reasoning"),
                input_modalities: strings(&m["architecture"]["input_modalities"]),
                output_modalities: strings(&m["architecture"]["output_modalities"]),
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    save_cache(&models)?;
    Ok(models)
}

fn strings(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Cached if younger than `max_age`, otherwise fetched (falling back to a
/// stale cache when offline).
pub fn load(base_url: &str, api_key: Option<&str>, max_age: Duration) -> Result<Vec<ModelInfo>> {
    match load_cached() {
        Some((m, age)) if age <= max_age => Ok(m),
        cached => match fetch(base_url, api_key) {
            Ok(m) => Ok(m),
            Err(e) => cached.map(|(m, _)| m).ok_or(e),
        },
    }
}

/// Context window of `id` from the cached catalogue, if known.
pub fn context_window(id: &str) -> Option<u64> {
    let (models, _) = load_cached()?;
    models
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.context_length)
        .filter(|&n| n > 0)
}

/// Subsequence fuzzy score: higher is better, `None` when `query` chars do not
/// all appear in order. Rewards prefix, word-boundary and contiguous matches.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(1);
    }
    let q: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    let c: Vec<char> = candidate.chars().flat_map(|c| c.to_lowercase()).collect();
    // all space-separated terms must match
    if query.contains(' ') {
        let mut total = 0;
        for term in query.split_whitespace() {
            total += fuzzy_score(term, candidate)?;
        }
        return Some(total);
    }
    let mut score: u32 = 0;
    let mut qi = 0;
    let mut prev: Option<usize> = None;
    for (i, ch) in c.iter().enumerate() {
        if qi < q.len() && *ch == q[qi] {
            score += 10;
            if i == 0 {
                score += 30;
            } else if !c[i - 1].is_alphanumeric() {
                score += 20; // word boundary (after '/', '-', '.', ':')
            }
            if prev == Some(i.wrapping_sub(1)) {
                score += 15; // contiguous
            }
            prev = Some(i);
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    Some(score.saturating_sub((c.len() as u32).min(200) / 4))
}

/// Rank `items` by fuzzy score of `key(item)` against `query`.
pub fn rank<'a, T>(query: &str, items: &'a [T], key: impl Fn(&T) -> String) -> Vec<&'a T> {
    let mut scored: Vec<(u32, &T)> = items
        .iter()
        .filter_map(|it| fuzzy_score(query, &key(it)).map(|s| (s, it)))
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, t)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_prefix_and_boundaries() {
        assert!(
            fuzzy_score("son", "anthropic/claude-sonnet-4.5")
                > fuzzy_score("son", "openai/gpt-5-personal")
        );
        assert!(fuzzy_score("q", "quit") > fuzzy_score("q", "sequence"));
        assert_eq!(fuzzy_score("xyz", "abc"), None);
        assert!(fuzzy_score("claude 4.5", "anthropic/claude-sonnet-4.5").is_some());
        assert_eq!(
            fuzzy_score("claude gpt", "anthropic/claude-sonnet-4.5"),
            None
        );
    }

    #[test]
    fn rank_orders() {
        let items = vec![
            "quit".to_string(),
            "reload".to_string(),
            "sequence".to_string(),
        ];
        let r = rank("q", &items, |s| s.clone());
        assert_eq!(r[0], "quit");
    }
}
