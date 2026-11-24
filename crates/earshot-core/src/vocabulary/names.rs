//! Which saved people's names a summary request carries (T12.91, docs/summary-names.md).
//!
//! Every saved person's name is spelling help for a summary, like a Dictionary word, but the
//! list can hold a whole organisation's people, so each request carries only the names its
//! own text calls for. A name matches a request's text when, over the text's white-space
//! tokens:
//!
//! 1. [`NameMatch::Exact`]: the name's words appear as consecutive tokens (case ignored,
//!    punctuation around a token and a trailing possessive `'s` dropped, as
//!    [`token_key`] does);
//! 2. [`NameMatch::Word`]: one of the name's words appears as a token, compared the same
//!    way; a name word that is a common word ([`super::suggest::is_common`], the matcher's
//!    own list) counts only where the token as written starts with an upper-case letter
//!    (anywhere in a sentence: "Mark said" matches a saved Mark, "mark the date" does not,
//!    and "Will you" does, a known cost);
//! 3. [`NameMatch::Sound`]: the shipped matcher ([`super::suggest::Suggester`], unchanged)
//!    offers one of the name's words for a token, with the same list the transcript's word
//!    suggestions use ([`listed_words`]: the Dictionary's words, then the names' words), so a
//!    name is chosen by sound exactly where the transcript would suggest it.
//!
//! A name's match is its strongest; matches rank by strength (exact first), then by the
//! order of the names as given. [`NameFilter::choose`] keeps every name a request is told
//! to carry (the names the parts of a merge carried, so a name chosen in a map step is never
//! lost at the merge) and adds matched names in rank order until the request holds `cap`
//! names; carried names are never dropped for the cap. The chosen names come back in the
//! order of the names as given, so a block is stable however the names were found.

use std::collections::HashSet;

use super::suggest::{Suggester, is_common, split_punctuation};

/// How a name matched a text, weakest first (so `Ord` ranks the strongest highest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NameMatch {
    /// The shipped matcher offers one of the name's words for a token of the text.
    Sound,
    /// One of the name's words is a token of the text.
    Word,
    /// The whole name is consecutive tokens of the text.
    Exact,
}

/// The word list the transcript's suggestions use (T12.15ec): the white-space words of the
/// Dictionary's entries, then those of `names`, each kept in its first form and dropped when
/// it repeats an earlier one in another case (the matcher reads a word listed twice as two
/// candidates and never suggests it). `earshot_runtime::word_choices::list_of` builds the
/// transcript's list through it, so the two lists are one rule.
#[must_use]
pub fn listed_words<D: AsRef<str>, N: AsRef<str>>(dictionary: &[D], names: &[N]) -> Vec<String> {
    let mut seen = HashSet::new();
    dictionary
        .iter()
        .flat_map(|e| e.as_ref().split_whitespace())
        .chain(names.iter().flat_map(|n| n.as_ref().split_whitespace()))
        .filter(|w| seen.insert(w.to_lowercase()))
        .map(str::to_owned)
        .collect()
}

/// A token as names are compared with it: its core ([`split_punctuation`]) in lower case,
/// without a trailing possessive (`'s` or a lone `'`). Empty for a token with no core.
#[must_use]
pub fn token_key(token: &str) -> String {
    let (_, core, _) = split_punctuation(token);
    let lower = core.to_lowercase();
    let lower = lower.strip_suffix("'s").unwrap_or(&lower);
    let lower = lower.strip_suffix('\'').unwrap_or(lower);
    lower.to_owned()
}

/// One name as the filter compares it.
#[derive(Debug, Clone)]
struct Name {
    form: String,
    /// The name's words as [`token_key`] reads them.
    keys: Vec<String>,
    /// The name's words in lower case, as the matcher's listed words are compared.
    lower: Vec<String>,
    /// Per word of `keys`: whether it is a common word, which counts as a word match only
    /// when the token is written with an upper-case first letter.
    common: Vec<bool>,
}

/// Saved people's names ready to match request texts against (module doc).
#[derive(Debug, Clone)]
pub struct NameFilter {
    names: Vec<Name>,
    suggester: Suggester,
}

impl NameFilter {
    /// A filter over `names` (in their given order, which breaks ties and orders the chosen
    /// names), with the matcher's list built from `dictionary` and `names` ([`listed_words`]).
    #[must_use]
    pub fn new<N: AsRef<str>, D: AsRef<str>>(names: &[N], dictionary: &[D]) -> Self {
        NameFilter {
            names: names
                .iter()
                .map(|n| {
                    let form = n.as_ref().to_owned();
                    let keys: Vec<String> = form
                        .split_whitespace()
                        .map(token_key)
                        .filter(|k| !k.is_empty())
                        .collect();
                    Name {
                        common: keys.iter().map(|k| is_common(k)).collect(),
                        keys,
                        lower: form.split_whitespace().map(str::to_lowercase).collect(),
                        form,
                    }
                })
                .collect(),
            suggester: Suggester::new(&listed_words(dictionary, names)),
        }
    }

    /// Whether there are no names (no request carries any).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Every name that matches one of `texts` (each matched on its own, so no name spans two
    /// texts), with its strongest match, ranked: strongest first, then in the names' order.
    /// Indices are into the names as given.
    #[must_use]
    pub fn matches<T: AsRef<str>>(&self, texts: &[T]) -> Vec<(usize, NameMatch)> {
        if self.names.is_empty() {
            return Vec::new();
        }
        let mut best: Vec<Option<NameMatch>> = vec![None; self.names.len()];
        let mut raise = |i: usize, m: NameMatch| {
            if best[i].is_none_or(|b| m > b) {
                best[i] = Some(m);
            }
        };
        for text in texts {
            let tokens: Vec<&str> = text.as_ref().split_whitespace().collect();
            let keys: Vec<String> = tokens.iter().map(|t| token_key(t)).collect();
            let present: HashSet<&str> = keys.iter().map(String::as_str).collect();
            // The keys of tokens written with an upper-case first letter.
            let capitalised: HashSet<&str> = tokens
                .iter()
                .zip(&keys)
                .filter(|(t, _)| {
                    split_punctuation(t)
                        .1
                        .chars()
                        .next()
                        .is_some_and(char::is_uppercase)
                })
                .map(|(_, k)| k.as_str())
                .collect();
            for (i, name) in self.names.iter().enumerate() {
                if name.keys.is_empty() {
                    continue;
                }
                if keys
                    .windows(name.keys.len())
                    .any(|w| w.iter().zip(&name.keys).all(|(a, b)| a == b))
                {
                    raise(i, NameMatch::Exact);
                } else if name.keys.iter().zip(&name.common).any(|(k, &common)| {
                    if common {
                        capitalised.contains(k.as_str())
                    } else {
                        present.contains(k.as_str())
                    }
                }) {
                    raise(i, NameMatch::Word);
                }
            }
            for token in &tokens {
                let Some(listed) = self.suggester.suggest_word(token) else {
                    continue;
                };
                let listed = listed.to_lowercase();
                for (i, name) in self.names.iter().enumerate() {
                    if name.lower.contains(&listed) {
                        raise(i, NameMatch::Sound);
                    }
                }
            }
        }
        let mut ranked: Vec<(usize, NameMatch)> = best
            .into_iter()
            .enumerate()
            .filter_map(|(i, m)| m.map(|m| (i, m)))
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked
    }

    /// The names a request carries (module doc): every name of `carried` that is one of the
    /// filter's names (compared exactly), then the names matching `texts` in rank order while
    /// fewer than `cap` are held; in the names' order.
    #[must_use]
    pub fn choose<T: AsRef<str>, C: AsRef<str>>(
        &self,
        texts: &[T],
        carried: &[C],
        cap: usize,
    ) -> Vec<String> {
        let mut chosen = vec![false; self.names.len()];
        let mut held = 0;
        for c in carried {
            if let Some(i) = self.names.iter().position(|n| n.form == c.as_ref())
                && !chosen[i]
            {
                chosen[i] = true;
                held += 1;
            }
        }
        for (i, _) in self.matches(texts) {
            if held >= cap {
                break;
            }
            if !chosen[i] {
                chosen[i] = true;
                held += 1;
            }
        }
        self.names
            .iter()
            .zip(chosen)
            .filter(|&(_, c)| c)
            .map(|(n, _)| n.form.clone())
            .collect()
    }
}

/// [`NameFilter::choose`] with a filter built for one call.
#[must_use]
pub fn choose_names<T: AsRef<str>, N: AsRef<str>, D: AsRef<str>, C: AsRef<str>>(
    texts: &[T],
    names: &[N],
    dictionary: &[D],
    carried: &[C],
    cap: usize,
) -> Vec<String> {
    NameFilter::new(names, dictionary).choose(texts, carried, cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: &[&str] = &[];

    #[test]
    fn vocabulary_names_listed_words_are_dictionary_then_names_without_repeats() {
        let got = listed_words(
            &["QXR-7", "Quillmarsh Ridge"],
            &["Vantrel Oskarine", "ridge Kam"],
        );
        assert_eq!(
            got,
            ["QXR-7", "Quillmarsh", "Ridge", "Vantrel", "Oskarine", "Kam"]
        );
    }

    #[test]
    fn vocabulary_names_token_keys_drop_punctuation_case_and_possessives() {
        assert_eq!(token_key("\"Oskarine's,"), "oskarine");
        assert_eq!(token_key("Vantrels'"), "vantrels");
        assert_eq!(token_key("(KAM)"), "kam");
        assert_eq!(token_key("--"), "");
    }

    #[test]
    fn vocabulary_names_match_exact_word_and_sound_and_rank_them() {
        let names = [
            "Bozzle Fenwright",
            "Vantrel Oskarine",
            "Kam Pellory",
            "Idris Quell",
        ];
        let f = NameFilter::new(&names, NONE);
        let texts = [
            "We asked Vantrel Oskarine's team.",
            "Then Pellory said it, and Bozle agreed.",
        ];
        // Exact for the whole name, word for one of its words, sound for a sound-alike.
        assert_eq!(
            f.matches(&texts),
            [
                (1, NameMatch::Exact),
                (2, NameMatch::Word),
                (0, NameMatch::Sound)
            ]
        );
        // A name split over two texts is two word matches, never exact.
        assert_eq!(f.matches(&["Idris", "Quell"]), [(3, NameMatch::Word)]);
        assert!(f.matches(&["Nobody of that name spoke."]).is_empty());
        assert!(NameFilter::new(NONE, NONE).matches(&texts).is_empty());
    }

    #[test]
    fn vocabulary_names_a_common_name_word_matches_only_written_with_a_capital() {
        let f = NameFilter::new(&["Mark Oskarine", "Will Ranford"], NONE);
        assert!(is_common("mark") && is_common("will"));
        // A capital anywhere in the sentence counts; lower case does not.
        assert_eq!(f.matches(&["so Mark said it"]), [(0, NameMatch::Word)]);
        assert_eq!(f.matches(&["\"Mark,\" she said"]), [(0, NameMatch::Word)]);
        assert!(f.matches(&["mark the date"]).is_empty());
        assert!(f.matches(&["we will see"]).is_empty());
        // The documented cost: a common word starting a sentence matches.
        assert_eq!(f.matches(&["Will you send it?"]), [(1, NameMatch::Word)]);
        // A whole name matches in any case; an uncommon word of it in lower case too.
        assert_eq!(f.matches(&["ask will ranford"]), [(1, NameMatch::Exact)]);
        assert_eq!(f.matches(&["ask ranford"]), [(1, NameMatch::Word)]);
        assert_eq!(
            f.matches(&["mark oskarine's plan"]),
            [(0, NameMatch::Exact)]
        );
    }

    #[test]
    fn vocabulary_names_sound_matches_follow_the_transcripts_suggestions() {
        // "come" is common, so the matcher never offers "Kam" for it.
        let f = NameFilter::new(&["Kam Pellory"], NONE);
        assert!(f.matches(&["they will come"]).is_empty());
        assert_eq!(f.matches(&["Cam, hello"]), [(0, NameMatch::Sound)]);
        // A Dictionary word of the same sound makes it ambiguous, as in the transcript.
        let g = NameFilter::new(&["Kam Pellory"], &["Cahm"]);
        assert!(g.matches(&["Cam, hello"]).is_empty());
    }

    #[test]
    fn vocabulary_names_choose_keeps_carried_names_and_caps_new_ones_by_rank() {
        let names = [
            "Ada Brisk",
            "Bozzle Fenwright",
            "Kam Pellory",
            "Oskarine Vale",
        ];
        let f = NameFilter::new(&names, NONE);
        let text = ["Kam spoke to Oskarine Vale about Bozle"];
        // Ranked: Oskarine Vale (exact), Kam Pellory (word), Bozzle Fenwright (sound).
        assert_eq!(f.choose(&text, NONE, 2), ["Kam Pellory", "Oskarine Vale"]);
        assert_eq!(f.choose(&text, NONE, 1), ["Oskarine Vale"]);
        assert!(f.choose(&text, NONE, 0).is_empty());
        // Carried names are kept beyond the cap and count toward it.
        assert_eq!(
            f.choose(&text, &["Ada Brisk"], 2),
            ["Ada Brisk", "Oskarine Vale"]
        );
        assert_eq!(
            f.choose(&text, &["Ada Brisk", "Bozzle Fenwright", "Kam Pellory"], 1),
            ["Ada Brisk", "Bozzle Fenwright", "Kam Pellory"]
        );
        // A carried name that is not one of the names is ignored.
        assert!(f.choose(NONE, &["Somebody Else"], 5).is_empty());
        assert_eq!(
            choose_names(&text, &names, NONE, NONE, usize::MAX),
            ["Bozzle Fenwright", "Kam Pellory", "Oskarine Vale"]
        );
    }
}
