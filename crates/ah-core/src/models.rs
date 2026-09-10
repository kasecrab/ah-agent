//! Cached OpenRouter model catalogue and a fuzzy matcher.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Result;

/// Modalities in display order with their one-letter tags, and the words
/// OpenRouter uses for them. Plain ASCII so every terminal font renders the
/// tags at full size.
pub const MODALITIES: &[(&str, &str)] = &[
    ("text", "T"),
    ("image", "I"),
    ("audio", "A"),
    ("video", "V"),
    ("file", "F"),
    ("speech", "S"),
    ("transcription", "X"),
    ("embeddings", "E"),
    ("rerank", "R"),
];

/// Bit for anything the table above does not name, so a modality OpenRouter
/// adds later is still counted rather than silently dropped.
const OTHER: u16 = 1 << 15;

/// A set of modalities, one bit each.
///
/// The catalogue is nearly six hundred models and every one of them used to
/// carry two `Vec<String>` — some twelve hundred allocations to answer
/// questions like "does this model draw?". A `u16` answers them with an `and`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Modalities(u16);

impl Modalities {
    pub const EMPTY: Self = Self(0);

    /// The bit for one modality name, or [`OTHER`] for a name not in the table.
    pub fn bit(name: &str) -> u16 {
        MODALITIES
            .iter()
            .position(|(m, _)| *m == name)
            .map(|i| 1 << i)
            .unwrap_or(OTHER)
    }

    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        Self(names.into_iter().fold(0, |acc, n| acc | Self::bit(n)))
    }

    pub fn has(self, name: &str) -> bool {
        self.0 & Self::bit(name) != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn bits(self) -> u16 {
        self.0
    }

    /// The names in table order. `other` is not named: there is no word for it.
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        MODALITIES
            .iter()
            .enumerate()
            .filter(move |(i, _)| self.0 & (1 << i) != 0)
            .map(|(_, (m, _))| *m)
    }

    /// `TIF`, in table order.
    pub fn icons(self) -> String {
        MODALITIES
            .iter()
            .enumerate()
            .filter(|(i, _)| self.0 & (1 << i) != 0)
            .map(|(_, (_, t))| *t)
            .collect()
    }
}

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
    /// USD per million audio input tokens. Dictation is billed here, and the
    /// spread between models is wide enough that picking blind is expensive.
    #[serde(default)]
    pub audio_per_m: f64,
    /// USD per million image output tokens. A model that only draws prices
    /// nothing under `completion`, so without this it reads as free.
    #[serde(default)]
    pub image_out_per_m: f64,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub reasoning: bool,
    /// What the model takes and what it gives back, as OpenRouter reports it.
    #[serde(default)]
    pub input: Modalities,
    #[serde(default)]
    pub output: Modalities,
}

impl ModelInfo {
    pub fn accepts(&self, modality: &str) -> bool {
        self.input.has(modality)
    }

    pub fn produces(&self, modality: &str) -> bool {
        self.output.has(modality)
    }

    /// A speech-to-text model: it answers on `/audio/transcriptions`, not
    /// `/chat/completions`, and takes nothing but the audio.
    pub fn transcribes(&self) -> bool {
        self.produces("transcription")
    }

    /// True for a model there is any point talking to: one that answers with
    /// words or with a picture. Everything else in the catalogue — embeddings,
    /// rerank, video, speech — is a different kind of thing entirely.
    pub fn chats(&self) -> bool {
        (self.produces("text") || self.produces("image")) && !self.transcribes()
    }

    /// `TIF→T`: input tags, arrow, output tags.
    pub fn modality_icons(&self) -> String {
        modality_icons(self.input, self.output)
    }
}

/// Separates input from output icons.
pub const MODALITY_ARROW: &str = "→";

/// `TIF→T`; only the input half when outputs are unknown, empty when both are.
pub fn modality_icons(input: Modalities, output: Modalities) -> String {
    let (i, o) = (input.icons(), output.icons());
    match (i.is_empty(), o.is_empty()) {
        (true, true) => String::new(),
        (_, true) => i,
        _ => format!("{i}{MODALITY_ARROW}{o}"),
    }
}

/// Bumped when `ModelInfo` gains fields; older caches are refetched.
const CACHE_VERSION: u32 = 6;

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

/// Fetch the catalogue from the API and refresh the cache.
///
/// `/models` on its own answers with the text category alone — four hundred
/// odd models — and everything that draws, speaks, transcribes, embeds or
/// reranks is missing from it. `output_modalities=all` asks for the lot.
pub fn fetch(base_url: &str, api_key: Option<&str>) -> Result<Vec<ModelInfo>> {
    let client = crate::provider::openrouter::OpenRouter::new(base_url, api_key.unwrap_or(""));
    let v = match client.get_json("/models?output_modalities=all") {
        Ok(v) => v,
        // A proxy that does not know the parameter still owes us the models it
        // does list, which is what ah had before it asked for the rest.
        Err(e) => {
            crate::debug!("full catalogue unavailable ({e}); asking for the default one");
            client.get_json("/models")?
        }
    };
    let mut models: Vec<ModelInfo> = parse(&v);
    models.sort_by(|a, b| a.id.cmp(&b.id));
    save_cache(&models)?;
    Ok(models)
}

fn parse(v: &serde_json::Value) -> Vec<ModelInfo> {
    v["data"]
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
                audio_per_m: {
                    let a = price("input_audio");
                    if a > 0.0 { a } else { price("audio") }
                },
                image_out_per_m: {
                    let i = price("image_output");
                    if i > 0.0 { i } else { price("image_token") }
                },
                tools: has("tools"),
                reasoning: has("reasoning") || has("include_reasoning"),
                input: modalities(&m["architecture"]["input_modalities"]),
                output: modalities(&m["architecture"]["output_modalities"]),
            })
        })
        .collect()
}

/// The modality names in a JSON array, as a bit set.
fn modalities(v: &serde_json::Value) -> Modalities {
    match v.as_array() {
        Some(a) => Modalities::from_names(a.iter().filter_map(|x| x.as_str())),
        None => Modalities::EMPTY,
    }
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

/// Catalogue entry for `id`, if a usable cache exists and lists it.
pub fn info(id: &str) -> Option<ModelInfo> {
    let (models, _) = load_cached()?;
    models.into_iter().find(|m| m.id == id)
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
    fn a_modality_set_is_a_bit_per_name() {
        let m = Modalities::from_names(["text", "image"]);
        assert!(m.has("text") && m.has("image"));
        assert!(!m.has("audio") && !m.has("rerank"));
        assert!(!m.is_empty());
        assert!(Modalities::EMPTY.is_empty());
        assert_eq!(m.names().collect::<Vec<_>>(), vec!["text", "image"]);
        assert_eq!(m.icons(), "TI");
        // A name the table does not know is still counted, so a category
        // OpenRouter adds later does not read as "no modalities at all".
        let odd = Modalities::from_names(["hologram"]);
        assert!(!odd.is_empty());
        assert!(!odd.has("text"));
        assert_eq!(odd.icons(), "");
        assert_eq!(odd.names().count(), 0);
    }

    #[test]
    fn icons_show_what_goes_in_and_what_comes_out() {
        let i = Modalities::from_names(["text", "image", "file"]);
        let o = Modalities::from_names(["text"]);
        assert_eq!(modality_icons(i, o), "TIF→T");
        // Outputs unknown: the input half alone.
        assert_eq!(modality_icons(i, Modalities::EMPTY), "TIF");
        assert_eq!(
            modality_icons(Modalities::EMPTY, Modalities::EMPTY),
            String::new()
        );
    }

    fn model(json: serde_json::Value) -> ModelInfo {
        parse(&serde_json::json!({"data": [json]})).remove(0)
    }

    #[test]
    fn a_model_that_only_draws_is_parsed_with_its_own_price() {
        let m = model(serde_json::json!({
            "id": "meta/muse-image",
            "context_length": 65536,
            "pricing": {"prompt": "0", "completion": "0", "image_output": "0.0000024"},
            "architecture": {
                "input_modalities": ["text", "image"],
                "output_modalities": ["image"]
            }
        }));
        assert!(m.produces("image") && !m.produces("text"));
        assert!(m.accepts("text") && m.accepts("image"));
        assert!(m.chats(), "a model that draws is one you can talk to");
        assert!(!m.transcribes());
        // Priced per image token, not under completion: without this it reads
        // as free.
        assert_eq!(m.completion_per_m, 0.0);
        assert!((m.image_out_per_m - 2.4).abs() < 1e-9);
        assert_eq!(m.modality_icons(), "TI→I");
    }

    #[test]
    fn the_kinds_of_model_you_cannot_talk_to() {
        let embed = model(serde_json::json!({
            "id": "openai/text-embedding-3-large",
            "architecture": {"input_modalities": ["text"], "output_modalities": ["embeddings"]}
        }));
        assert!(!embed.chats());
        let stt = model(serde_json::json!({
            "id": "openai/whisper",
            "architecture": {"input_modalities": ["audio"], "output_modalities": ["transcription"]}
        }));
        assert!(stt.transcribes() && !stt.chats());
        let chat = model(serde_json::json!({
            "id": "anthropic/claude",
            "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]}
        }));
        assert!(chat.chats());
    }

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
