//! Whisper's non-speech annotations (T6.7).
//!
//! Whisper was trained on captions that mark sounds in brackets or parentheses, and it
//! reproduces them: whisper.cpp prints `[BLANK_AUDIO]` for silence, and the models write
//! `[MUSIC PLAYING]`, `(gentle music)`, `*laughs*` or `♪`. openai/whisper treats the
//! characters that build them as non-speech (`tokenizer.py` `non_speech_tokens`: among
//! others `[ ] ( ) *` and the musical symbols `♪♪♪`), and whisper.cpp's `suppress_nst`
//! suppresses the same set. They are not words anyone said, so they never reach a
//! transcript, a summary prompt or an export: [`drop_non_speech`] removes them from both the
//! live chunker's words and the final pass's segments.
//!
//! Rule: a word that opens with `[` or `(` (after leading punctuation) starts an
//! annotation, which runs to the first word that closes it with `]` or `)` (trailing
//! punctuation allowed) within [`MAX_ANNOTATION_WORDS`]; an opener with no close in that
//! range is kept as speech. A word wrapped in `*` and a word made only of musical symbols
//! are annotations on their own.

use super::types::Word;

/// Longest bracketed annotation removed, in words (`[MUSIC PLAYING]` is 2).
pub const MAX_ANNOTATION_WORDS: usize = 6;

const MUSIC: &[char] = &['♪', '♫', '♬', '♩', '♭', '♯'];

fn trim_punct(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, '-' | '.' | ',' | '!' | '?' | ';' | ':' | '"' | '\''))
}

fn opens(word: &str) -> Option<char> {
    match trim_punct(word).chars().next() {
        Some('[') => Some(']'),
        Some('(') => Some(')'),
        _ => None,
    }
}

fn closes(word: &str, close: char) -> bool {
    trim_punct(word).ends_with(close)
}

fn lone_annotation(word: &str) -> bool {
    let w = trim_punct(word);
    let starred = w.len() >= 3 && w.starts_with('*') && w.ends_with('*');
    let music = !w.is_empty() && w.chars().all(|c| MUSIC.contains(&c) || c.is_whitespace());
    starred || music
}

/// Removes Whisper's non-speech annotations from `words` (module docs).
#[must_use]
pub fn drop_non_speech(words: Vec<Word>) -> Vec<Word> {
    let mut keep = vec![true; words.len()];
    let mut i = 0;
    while i < words.len() {
        if lone_annotation(&words[i].text) {
            keep[i] = false;
            i += 1;
            continue;
        }
        if let Some(close) = opens(&words[i].text) {
            let end = (i..words.len().min(i + MAX_ANNOTATION_WORDS))
                .find(|&j| closes(&words[j].text, close));
            if let Some(end) = end {
                keep[i..=end].fill(false);
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    words
        .into_iter()
        .zip(keep)
        .filter_map(|(w, k)| k.then_some(w))
        .collect()
}

/// Whether `text` is nothing but non-speech annotations (or empty).
#[must_use]
pub fn is_non_speech(text: &str) -> bool {
    let words: Vec<Word> = text
        .split_whitespace()
        .map(|t| Word::new(t, 0, 0, 1.0))
        .collect();
    drop_non_speech(words).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, t)| Word::new(t, 100 * i as u64, 100 * i as u64 + 90, 0.9))
            .collect()
    }

    fn kept(text: &str) -> String {
        drop_non_speech(words(text))
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn asr_nonspeech_annotations_are_removed() {
        assert_eq!(kept("[BLANK_AUDIO]"), "");
        assert_eq!(kept("[MUSIC PLAYING]"), "");
        assert_eq!(kept("(gentle music)"), "");
        assert_eq!(kept("*laughs*"), "");
        assert_eq!(kept("♪ ♪"), "");
        assert_eq!(kept("-(applause)."), "");
        assert_eq!(
            kept("Thanks everyone. [MUSIC PLAYING] See you."),
            "Thanks everyone. See you."
        );
        assert_eq!(kept("Okay (sighs) let's go"), "Okay let's go");
    }

    #[test]
    fn asr_nonspeech_speech_is_kept() {
        assert_eq!(
            kept("The quarterly numbers came in."),
            "The quarterly numbers came in."
        );
        // An opener without a close in range is speech.
        assert_eq!(
            kept("(and then we waited for the very long reply to come"),
            "(and then we waited for the very long reply to come"
        );
        assert_eq!(kept("a * b"), "a * b");
        // Word times are untouched.
        let w = drop_non_speech(words("hello [BLANK_AUDIO] there"));
        assert_eq!(w[1].start.as_millis(), 200);
    }

    #[test]
    fn asr_nonspeech_whole_text_check() {
        assert!(is_non_speech("[BLANK_AUDIO]"));
        assert!(is_non_speech(" "));
        assert!(!is_non_speech("Yes. [MUSIC]"));
    }
}
