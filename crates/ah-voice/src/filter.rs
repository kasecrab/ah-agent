//! What models say when there was nothing to hear.
//!
//! Transcribers trained on captioned video fill silence with the furniture of
//! that video: a sign-off, a credit, a stock phrase. It arrives looking like a
//! perfectly ordinary transcript, so it has to be recognised by name.

/// Lower-case, punctuation stripped. Kept short on purpose: anything long
/// enough to be a real sentence a user might dictate does not belong here.
const STOCK: &[&str] = &[
    "thank you",
    "thanks",
    "thank you for watching",
    "thanks for watching",
    "thank you very much",
    "please subscribe",
    "like and subscribe",
    "subtitles by the amaraorg community",
    "subtitles by the amara org community",
    "transcription by castingwordscom",
    "music",
    "applause",
    "laughter",
    "silence",
    "you",
    "bye",
    "okay",
    "so",
    "gracias",
    "merci",
    "danke",
    "spasibo",
];

fn normalise(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// True for text that should be dropped rather than shown.
pub fn is_stock(text: &str) -> bool {
    let n = normalise(text);
    if n.is_empty() {
        return true;
    }
    STOCK.contains(&n.as_str())
}

/// A model that loses its place repeats one phrase until it runs out of
/// tokens. Collapse a run of the same word or short phrase back to one.
pub fn collapse_repeats(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 6 {
        return text.trim().to_string();
    }
    for span in 1..=4usize {
        if words.len() < span * 4 {
            continue;
        }
        let mut out: Vec<&str> = Vec::with_capacity(words.len());
        let mut i = 0;
        let mut collapsed = false;
        while i < words.len() {
            let end = i + span;
            if end > words.len() {
                out.extend_from_slice(&words[i..]);
                break;
            }
            let unit = &words[i..end];
            let mut reps = 1;
            let mut j = end;
            while j + span <= words.len() && eq_ci(&words[j..j + span], unit) {
                reps += 1;
                j += span;
            }
            out.extend_from_slice(unit);
            if reps >= 4 {
                collapsed = true;
            }
            i = j;
        }
        if collapsed {
            return out.join(" ");
        }
    }
    text.trim().to_string()
}

fn eq_ci(a: &[&str], b: &[&str]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Run a transcript through both. `None` means there is nothing to show.
pub fn clean(text: &str, enabled: bool) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if !enabled {
        return Some(t.to_string());
    }
    if is_stock(t) {
        return None;
    }
    let c = collapse_repeats(t);
    if c.trim().is_empty() { None } else { Some(c) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_phrases_are_recognised_through_punctuation_and_case() {
        assert!(is_stock("Thanks for watching!"));
        assert!(is_stock("  [Music] "));
        assert!(is_stock("You."));
        assert!(is_stock(""));
    }

    #[test]
    fn real_speech_is_left_alone() {
        assert!(!is_stock("thank you for the review, it helped"));
        assert!(!is_stock("fix the auth middleware"));
        assert_eq!(clean("fix the auth middleware", true).unwrap(), "fix the auth middleware");
    }

    #[test]
    fn a_repeat_loop_collapses_to_one() {
        let looped = "and then and then and then and then and then and then";
        assert_eq!(collapse_repeats(looped), "and then");
    }

    #[test]
    fn a_single_word_loop_collapses() {
        assert_eq!(collapse_repeats("no no no no no no no"), "no");
    }

    #[test]
    fn ordinary_repetition_is_not_a_loop() {
        let s = "the test is the test that we run when the build is done";
        assert_eq!(collapse_repeats(s), s);
    }

    #[test]
    fn filtering_off_keeps_everything_but_blanks() {
        assert_eq!(clean("Thanks for watching", false).unwrap(), "Thanks for watching");
        assert!(clean("   ", false).is_none());
    }
}
