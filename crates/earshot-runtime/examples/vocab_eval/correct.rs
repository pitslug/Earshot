//! The correction pass of docs/vocabulary.md (method c): a decoded word close to exactly one
//! listed word is replaced by it, and every replacement keeps the original word.

use std::collections::HashSet;

use crate::lists::tokens;

/// The correction variants, in the doc's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Equal sound keys, any decoded word.
    Sound,
    /// Equal sound keys, never a common word.
    SoundRare,
    /// Close spelling, never a common word.
    SpellingRare,
}

impl Variant {
    pub const ALL: [Variant; 3] = [Variant::Sound, Variant::SoundRare, Variant::SpellingRare];

    pub fn name(self) -> &'static str {
        match self {
            Variant::Sound => "sound",
            Variant::SoundRare => "sound-rare",
            Variant::SpellingRare => "spelling-rare",
        }
    }
}

/// One replacement: where, the decoded word kept, and the listed word written instead.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Change {
    pub position: usize,
    pub original: String,
    pub listed: String,
}

/// The reduced sound key of docs/vocabulary.md (after Philips' Metaphone, 1990).
pub fn sound_key(word: &str) -> String {
    let mut s: String = word
        .chars()
        .flat_map(char::to_lowercase)
        .filter(char::is_ascii_lowercase)
        .collect();
    for (from, to) in [("kn", "n"), ("wr", "r"), ("gh", "g")] {
        if let Some(rest) = s.strip_prefix(from) {
            s = format!("{to}{rest}");
        }
    }
    for (from, to) in [
        ("sch", "sk"),
        ("ph", "f"),
        ("ck", "k"),
        ("qu", "kw"),
        ("wh", "w"),
        ("gh", ""),
    ] {
        s = s.replace(from, to);
    }
    let chars: Vec<char> = s.chars().collect();
    let mut mapped = String::new();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            'c' => {
                let soft = matches!(chars.get(i + 1), Some('e' | 'i' | 'y'));
                mapped.push(if soft { 's' } else { 'k' });
            }
            'q' => mapped.push('k'),
            'x' => mapped.push_str("ks"),
            'z' => mapped.push('s'),
            'v' => mapped.push('f'),
            _ => mapped.push(c),
        }
    }
    let mut key = String::new();
    for (i, c) in mapped.chars().enumerate() {
        let c = if i == 0 {
            if matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y') {
                'a'
            } else {
                c
            }
        } else if matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'h' | 'w') {
            continue;
        } else {
            c
        };
        if !key.ends_with(c) {
            key.push(c);
        }
    }
    key
}

/// Levenshtein distance between two words, by characters.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            row[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(row[j] + 1);
        }
        prev = row;
    }
    prev[b.len()]
}

/// `word` split into leading punctuation, its core (from the first to the last letter, digit
/// or apostrophe) and trailing punctuation.
pub fn split_punctuation(word: &str) -> (&str, &str, &str) {
    let keep = |c: char| c.is_alphanumeric() || c == '\'';
    let Some(start) = word.find(keep) else {
        return (word, "", "");
    };
    let end = word.rfind(keep).map_or(word.len(), |i| {
        i + word[i..].chars().next().map_or(1, char::len_utf8)
    });
    (&word[..start], &word[start..end], &word[end..])
}

/// Whether every normalised token of `word` is a common word (and it has one).
pub fn is_common(word: &str, common: &HashSet<String>) -> bool {
    let t = tokens(word);
    !t.is_empty() && t.iter().all(|t| common.contains(t))
}

/// One listed word as the pass compares it.
struct Listed<'a> {
    form: &'a str,
    lower: String,
    key: String,
}

/// The decoded `words` after the pass, and every replacement made.
pub fn correct(
    words: &[String],
    listed_forms: &[String],
    variant: Variant,
    common: &HashSet<String>,
) -> (Vec<String>, Vec<Change>) {
    let listed: Vec<Listed> = listed_forms
        .iter()
        .map(|f| Listed {
            form: f,
            lower: f.to_lowercase(),
            key: sound_key(f),
        })
        .collect();
    let mut out = Vec::with_capacity(words.len());
    let mut changes = Vec::new();
    for (position, word) in words.iter().enumerate() {
        let (lead, core, trail) = split_punctuation(word);
        let lower = core.to_lowercase();
        if core.is_empty() || listed.iter().any(|l| l.lower == lower) {
            out.push(word.clone());
            continue;
        }
        let candidates: Vec<&Listed> = match variant {
            Variant::Sound | Variant::SoundRare => {
                let key = sound_key(core);
                if key.chars().count() < 2
                    || (variant == Variant::SoundRare && is_common(core, common))
                {
                    Vec::new()
                } else {
                    listed.iter().filter(|l| l.key == key).collect()
                }
            }
            Variant::SpellingRare => {
                if is_common(core, common) {
                    Vec::new()
                } else {
                    listed
                        .iter()
                        .filter(|l| {
                            let n = l.lower.chars().count();
                            n >= 4 && levenshtein(&lower, &l.lower) <= (n / 4).max(1)
                        })
                        .collect()
                }
            }
        };
        match candidates.as_slice() {
            [only] => {
                changes.push(Change {
                    position,
                    original: word.clone(),
                    listed: only.form.to_owned(),
                });
                out.push(format!("{lead}{}{trail}", only.form));
            }
            _ => out.push(word.clone()),
        }
    }
    (out, changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn vocab_sound_keys_join_spellings_of_one_sound() {
        assert_eq!(sound_key("Kam"), sound_key("Cam"));
        assert_eq!(sound_key("Kat"), sound_key("cat"));
        assert_eq!(sound_key("Stephen"), sound_key("Steven"));
        assert_eq!(sound_key("Philips"), sound_key("Filips"));
        assert_eq!(sound_key("Knight"), "nt");
        // Letters that meet once the vowels are gone collapse too.
        assert_eq!(sound_key("Cecil"), "sl");
        assert_eq!(sound_key("Xavier"), "ksfr");
        assert_eq!(sound_key("Ollie"), "al");
        assert_ne!(sound_key("Kam"), sound_key("Pam"));
        // The cost the doc names: a common word can share a name's key.
        assert_eq!(sound_key("come"), sound_key("Kam"));
    }

    #[test]
    fn vocab_levenshtein_counts_edits() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("same", "same"), 0);
    }

    #[test]
    fn vocab_punctuation_is_split_off_and_put_back() {
        assert_eq!(split_punctuation("\"Cam,"), ("\"", "Cam", ","));
        assert_eq!(split_punctuation("don't."), ("", "don't", "."));
        assert_eq!(split_punctuation("--"), ("--", "", ""));
        assert_eq!(split_punctuation("café!"), ("", "café", "!"));
    }

    #[test]
    fn vocab_correction_keeps_the_original_beside_every_replacement() {
        let common: HashSet<String> = ["come", "cat", "the"].map(str::to_owned).into();
        let listed = s(&["Kam", "Kat", "Bozzle"]);
        let words = s(&["Cam,", "will", "come", "the", "cat", "Bozle", "Kam"]);
        let (out, changes) = correct(&words, &listed, Variant::Sound, &common);
        assert_eq!(
            out,
            s(&["Kam,", "will", "Kam", "the", "Kat", "Bozzle", "Kam"])
        );
        assert_eq!(changes.len(), 4);
        assert_eq!(
            changes[0],
            Change {
                position: 0,
                original: "Cam,".into(),
                listed: "Kam".into()
            }
        );
        // The rare forms never touch a common word.
        let (out, changes) = correct(&words, &listed, Variant::SoundRare, &common);
        assert_eq!(
            out,
            s(&["Kam,", "will", "come", "the", "cat", "Bozzle", "Kam"])
        );
        assert_eq!(changes.len(), 2);
        let (out, _) = correct(&words, &listed, Variant::SpellingRare, &common);
        // "Cam" is three letters from a three-letter word: too short for spelling.
        assert_eq!(
            out,
            s(&["Cam,", "will", "come", "the", "cat", "Bozzle", "Kam"])
        );
    }

    #[test]
    fn vocab_correction_leaves_ambiguous_words() {
        let listed = s(&["Kam", "Cahm"]);
        let (out, changes) = correct(&s(&["Cam"]), &listed, Variant::Sound, &HashSet::new());
        assert_eq!(out, s(&["Cam"]));
        assert!(changes.is_empty());
    }
}
