//! The correction pass of docs/vocabulary.md (method c): a decoded word close to exactly one
//! listed word is replaced by it, and every replacement keeps the original word. `sound-rare`
//! is the shipped matcher, `earshot_core::vocabulary::suggest` (T12.15eb); the other two
//! variants stay evaluation only and share its sound key, punctuation split and common words.

use earshot_core::vocabulary::suggest::{Suggester, is_common, sound_key, split_punctuation};

/// The correction variants, in the doc's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Equal sound keys, any decoded word.
    Sound,
    /// Equal sound keys, never a common word: core's matcher.
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

/// One listed word as the pass compares it.
struct Listed<'a> {
    form: &'a str,
    lower: String,
    key: String,
}

/// The decoded `words` (white-space tokens) after the pass, and every replacement made.
/// Common words are core's (`earshot_core::vocabulary::suggest::common_words`).
pub fn correct(
    words: &[String],
    listed_forms: &[String],
    variant: Variant,
) -> (Vec<String>, Vec<Change>) {
    if variant == Variant::SoundRare {
        return sound_rare(words, listed_forms);
    }
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
                if key.chars().count() < 2 {
                    Vec::new()
                } else {
                    listed.iter().filter(|l| l.key == key).collect()
                }
            }
            Variant::SpellingRare => {
                if is_common(core) {
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

/// `sound-rare`: core's matcher on the segment text the tokens `words` make, each suggestion
/// written in its token's place with the token's punctuation kept.
fn sound_rare(words: &[String], listed_forms: &[String]) -> (Vec<String>, Vec<Change>) {
    // Tokens split from a hypothesis on white space join and split back to themselves, so a
    // suggestion's token index is the position in `words`.
    assert!(
        words
            .iter()
            .all(|w| !w.is_empty() && !w.contains(char::is_whitespace)),
        "sound-rare takes white-space tokens"
    );
    let mut out = words.to_vec();
    let mut changes = Vec::new();
    for s in Suggester::new(listed_forms).suggest(&words.join(" ")) {
        let (lead, _, trail) = split_punctuation(&s.decoded);
        out[s.token] = format!("{lead}{}{trail}", s.listed);
        changes.push(Change {
            position: s.token,
            original: s.decoded,
            listed: s.listed,
        });
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
    fn vocab_levenshtein_counts_edits() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("same", "same"), 0);
    }

    #[test]
    fn vocab_correction_keeps_the_original_beside_every_replacement() {
        let listed = s(&["Kam", "Kat", "Bozzle"]);
        let words = s(&["Cam,", "will", "come", "the", "cat", "Bozle", "Kam"]);
        let (out, changes) = correct(&words, &listed, Variant::Sound);
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
        // The rare forms never touch a common word ("come" and "cat" are in core's list).
        let (out, changes) = correct(&words, &listed, Variant::SoundRare);
        assert_eq!(
            out,
            s(&["Kam,", "will", "come", "the", "cat", "Bozzle", "Kam"])
        );
        assert_eq!(
            changes,
            [
                Change {
                    position: 0,
                    original: "Cam,".into(),
                    listed: "Kam".into()
                },
                Change {
                    position: 5,
                    original: "Bozle".into(),
                    listed: "Bozzle".into()
                }
            ]
        );
        let (out, _) = correct(&words, &listed, Variant::SpellingRare);
        // "Cam" is three letters from a three-letter word: too short for spelling.
        assert_eq!(
            out,
            s(&["Cam,", "will", "come", "the", "cat", "Bozzle", "Kam"])
        );
    }

    /// `Variant::SoundRare` calls core's matcher, so this is equal by construction apart from
    /// the field mapping and the punctuation kept around a replaced word, which is all it
    /// pins. It is not evidence that core's matcher behaves as the evaluated one did: that is
    /// the T12.15ea reproduction (docs/vocabulary.md, AGENTS.md "re-runs the T12.15ea
    /// reproduction").
    #[test]
    fn vocab_correction_sound_rare_is_cores_matcher() {
        let listed = s(&["Kam", "Bozzle", "Quint"]);
        let text = "so (Cam) said Bozle! and kwint, Kam.";
        let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
        let (out, changes) = correct(&words, &listed, Variant::SoundRare);
        let core = earshot_core::vocabulary::suggest::suggest(text, &listed);
        assert_eq!(changes.len(), core.len());
        for (c, g) in changes.iter().zip(&core) {
            assert_eq!(
                (c.position, c.original.as_str(), c.listed.as_str()),
                (g.token, g.decoded.as_str(), g.listed.as_str())
            );
        }
        assert_eq!(out[1], "(Kam)");
    }

    #[test]
    fn vocab_correction_leaves_ambiguous_words() {
        let listed = s(&["Kam", "Cahm"]);
        let (out, changes) = correct(&s(&["Cam"]), &listed, Variant::Sound);
        assert_eq!(out, s(&["Cam"]));
        assert!(changes.is_empty());
    }
}
