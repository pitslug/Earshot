//! English text normaliser for WER scoring.
//!
//! Port of the rules that matter from OpenAI Whisper's `EnglishTextNormalizer`
//! (`whisper/normalizers/english.py` and `basic.py`, MIT licence,
//! <https://github.com/openai/whisper>). Steps, in Whisper's order:
//!
//! 1. lowercase;
//! 2. remove `[...]` / `<...>` spans and non-empty `(...)` spans;
//! 3. remove the fillers `hmm mm mhm mmm uh um` (whole words);
//! 4. join a space before an apostrophe (`\s+'` -> `'`);
//! 5. the replacer table: common contractions, titles, perfect tenses, general
//!    contractions (same patterns and order as Whisper);
//! 6. remove commas between digits;
//! 7. periods not followed by a digit become spaces;
//! 8. remove symbols, punctuation and diacritics, keeping `. % $ ¢ € £`;
//! 9. `EnglishNumberNormalizer`, ported in full: spelled-out and digit numbers share
//!    one running value, so suffix words attach to both (`25 percent` and
//!    `twenty five percent` -> `25%`, `per cent` too); currency words and symbols
//!    (`five dollars` and `$5` -> `$5`, `two dollars and seven cents` -> `$2.07`,
//!    `$0.50` -> `¢50`); plurals (`the twenties` -> `the 20s`); ordinals; `oh`/`o` as
//!    zero (`one oh one` -> `101`); `double`/`triple`; `point` decimals; `and a half`;
//!    signs; a lone `1` / `1s` written back as `one` / `ones`;
//! 10. British -> American spellings from Whisper's full `english.json` (1739 entries,
//!     embedded unmodified, see `data/english.json.NOTICE`);
//! 11. remove `. $ ¢ € £` not followed by a digit and `%` not preceded by one;
//! 12. collapse whitespace (and trim).
//!
//! Every regex substitution is reproduced with upstream's left-to-right,
//! non-overlapping semantics. Reference cases checked against the upstream Python
//! normaliser are in `metrics/whisper_reference.rs`.
//!
//! Deviations, all applied identically to reference and hypothesis:
//! - No Unicode NFKD tables in `core`: diacritics are folded with a Latin table
//!   ([`fold_diacritic`]); other non-alphanumeric characters become spaces.
//! - Regex `\d` is ASCII here (Python's matches every Unicode decimal digit).
//! - Numbers that do not fit in `u128` (about 3.4e38) may be grouped differently, since
//!   Python integers are unbounded.
//! - The result is trimmed (Whisper can leave one leading or trailing space).

use std::collections::HashMap;
use std::sync::OnceLock;

/// Normalises English text for WER scoring (see the module documentation).
pub fn normalize_english(text: &str) -> String {
    let mut s = text.to_lowercase();
    s = remove_bracketed(&s);
    for filler in ["hmm", "mm", "mhm", "mmm", "uh", "um"] {
        s = replace_bounded(&s, filler, "", true, true);
    }
    s = join_space_before_apostrophe(&s);
    for (pat, rep, lb, rb) in REPLACERS {
        s = replace_bounded(&s, pat, rep, *lb, *rb);
    }
    s = remove_commas_between_digits(&s);
    s = periods_not_before_digits(&s);
    s = remove_symbols_and_diacritics(&s, &['.', '%', '$', '¢', '€', '£']);
    s = standardize_numbers(&s);
    s = s
        .split_whitespace()
        .map(|w| american_spelling(w).unwrap_or(w))
        .collect::<Vec<_>>()
        .join(" ");
    s = remove_unattached_numeric_symbols(&s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Regex `\w` (Unicode): alphanumeric or underscore.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Replaces non-overlapping occurrences of `pat` left to right. `lb` / `rb` require a
/// word boundary (`\b`) just before / after the match; every pattern used here starts
/// and ends with a word character where a boundary is requested.
fn replace_bounded(s: &str, pat: &str, rep: &str, lb: bool, rb: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut prev: Option<char> = None;
    while i < s.len() {
        let rest = &s[i..];
        if let Some(after) = rest.strip_prefix(pat) {
            let next = after.chars().next();
            let left_ok = !lb || !prev.is_some_and(is_word);
            let right_ok = !rb || !next.is_some_and(is_word);
            if left_ok && right_ok {
                out.push_str(rep);
                prev = pat.chars().last();
                i += pat.len();
                continue;
            }
        }
        let c = rest.chars().next().expect("i is a char boundary below len");
        out.push(c);
        prev = Some(c);
        i += c.len_utf8();
    }
    out
}

/// Whisper: `[<\[][^>\]]*[>\]]` -> "" then `\(([^)]+?)\)` -> "".
fn remove_bracketed(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if (c == '[' || c == '<')
            && let Some(off) = chars[i + 1..].iter().position(|&d| d == ']' || d == '>')
        {
            i += off + 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    let chars: Vec<char> = out.chars().collect();
    let mut out2 = String::with_capacity(out.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '('
            && let Some(off) = chars[i + 1..].iter().position(|&d| d == ')')
            && off > 0
        {
            i += off + 2;
            continue;
        }
        out2.push(c);
        i += 1;
    }
    out2
}

/// Whisper: `\s+'` -> `'`.
fn join_space_before_apostrophe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\'' {
            while out.ends_with(char::is_whitespace) {
                out.pop();
            }
        }
        out.push(c);
    }
    out
}

/// Whisper's replacer table, in order: (pattern, replacement, `\b` before, `\b` after).
const REPLACERS: &[(&str, &str, bool, bool)] = &[
    // common contractions
    ("won't", "will not", true, true),
    ("can't", "can not", true, true),
    ("let's", "let us", true, true),
    ("ain't", "aint", true, true),
    ("y'all", "you all", true, true),
    ("wanna", "want to", true, true),
    ("gotta", "got to", true, true),
    ("gonna", "going to", true, true),
    ("i'ma", "i am going to", true, true),
    ("imma", "i am going to", true, true),
    ("woulda", "would have", true, true),
    ("coulda", "could have", true, true),
    ("shoulda", "should have", true, true),
    ("ma'am", "madam", true, true),
    // titles and prefixes
    ("mr", "mister ", true, true),
    ("mrs", "missus ", true, true),
    ("st", "saint ", true, true),
    ("dr", "doctor ", true, true),
    ("prof", "professor ", true, true),
    ("capt", "captain ", true, true),
    ("gov", "governor ", true, true),
    ("ald", "alderman ", true, true),
    ("gen", "general ", true, true),
    ("sen", "senator ", true, true),
    ("rep", "representative ", true, true),
    ("pres", "president ", true, true),
    ("rev", "reverend ", true, true),
    ("hon", "honorable ", true, true),
    ("asst", "assistant ", true, true),
    ("assoc", "associate ", true, true),
    ("lt", "lieutenant ", true, true),
    ("col", "colonel ", true, true),
    ("jr", "junior ", true, true),
    ("sr", "senior ", true, true),
    ("esq", "esquire ", true, true),
    // perfect tenses
    ("'d been", " had been", false, true),
    ("'s been", " has been", false, true),
    ("'d gone", " had gone", false, true),
    ("'s gone", " has gone", false, true),
    ("'d done", " had done", false, true),
    ("'s got", " has got", false, true),
    // general contractions
    ("n't", " not", false, true),
    ("'re", " are", false, true),
    ("'s", " is", false, true),
    ("'d", " would", false, true),
    ("'ll", " will", false, true),
    ("'t", " not", false, true),
    ("'ve", " have", false, true),
    ("'m", " am", false, true),
];

/// Whisper: `(\d),(\d)` -> `\1\2` (non-overlapping, so `1,2,3` -> `12,3`).
fn remove_commas_between_digits(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    replace_scan(&chars, |i| {
        let d = |k: usize| chars.get(k).is_some_and(char::is_ascii_digit);
        (d(i) && chars.get(i + 1) == Some(&',') && d(i + 2))
            .then(|| (i + 3, [chars[i], chars[i + 2]].iter().collect()))
    })
}

/// Whisper: `\.([^0-9]|$)` -> ` \1` (the following character is consumed).
fn periods_not_before_digits(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    replace_scan(&chars, |i| {
        if chars[i] != '.' {
            return None;
        }
        match chars.get(i + 1) {
            None => Some((i + 1, " ".into())),
            Some(n) if !n.is_ascii_digit() => Some((i + 2, format!(" {n}"))),
            Some(_) => None,
        }
    })
}

/// Folds a lowercase Latin letter with a diacritic to its base letters (the effect of
/// NFKD plus dropping combining marks, and Whisper's `ADDITIONAL_DIACRITICS`).
pub fn fold_diacritic(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' => "i",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' => "s",
        'ţ' | 'ť' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'æ' => "ae",
        'œ' => "oe",
        'ß' => "ss",
        'þ' => "th",
        _ => return None,
    })
}

fn remove_symbols_and_diacritics(s: &str, keep: &[char]) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if keep.contains(&c) {
            out.push(c);
        } else if let Some(base) = fold_diacritic(c) {
            out.push_str(base);
        } else if c.is_alphanumeric() || c.is_whitespace() {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    out
}

/// Whisper: `[.$¢€£]([^0-9])` -> ` \1`, then `([^0-9])%` -> `\1 ` (both
/// non-overlapping, consuming the neighbouring character).
fn remove_unattached_numeric_symbols(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let pass1 = replace_scan(&chars, |i| {
        let sym = matches!(chars[i], '.' | '$' | '¢' | '€' | '£');
        match chars.get(i + 1) {
            Some(n) if sym && !n.is_ascii_digit() => Some((i + 2, format!(" {n}"))),
            _ => None,
        }
    });
    let chars: Vec<char> = pass1.chars().collect();
    replace_scan(&chars, |i| {
        (!chars[i].is_ascii_digit() && chars.get(i + 1) == Some(&'%'))
            .then(|| (i + 2, format!("{} ", chars[i])))
    })
}

// ------------------------------------------------------------------ numbers
//
// Port of Whisper's `EnglishNumberNormalizer` (`whisper/normalizers/english.py` at
// openai/whisper 86098128c0b4f24f0e2aa2994de830614b474227, MIT): `preprocess`,
// `process_words` and `postprocess` keep upstream's structure and branch order so the
// two can be read side by side. Python's `int | str` running value is [`Value`].

const ONES: [&str; 19] = [
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];

const TENS: [(&str, u128); 8] = [
    ("twenty", 20),
    ("thirty", 30),
    ("forty", 40),
    ("fifty", 50),
    ("sixty", 60),
    ("seventy", 70),
    ("eighty", 80),
    ("ninety", 90),
];

const MULTIPLIERS: [(&str, u128); 12] = [
    ("hundred", 100),
    ("thousand", 1_000),
    ("million", 1_000_000),
    ("billion", 1_000_000_000),
    ("trillion", 1_000_000_000_000),
    ("quadrillion", 1_000_000_000_000_000),
    ("quintillion", 1_000_000_000_000_000_000),
    ("sextillion", 1_000_000_000_000_000_000_000),
    ("septillion", 1_000_000_000_000_000_000_000_000),
    ("octillion", 1_000_000_000_000_000_000_000_000_000),
    ("nonillion", 1_000_000_000_000_000_000_000_000_000_000),
    ("decillion", 1_000_000_000_000_000_000_000_000_000_000_000),
];

/// Characters that may prefix a number token (`self.prefixes` upstream).
const PREFIXES: [char; 6] = ['-', '+', '£', '€', '$', '¢'];

/// `self.zeros`.
fn is_zero(w: &str) -> bool {
    matches!(w, "o" | "oh" | "zero")
}

/// `self.ones`: one..nineteen.
fn ones(w: &str) -> Option<u128> {
    ONES.iter().position(|&n| n == w).map(|i| i as u128 + 1)
}

/// `self.ones_suffixed`: plurals (`sixes`, `twos`, ...) and ordinals. Upstream builds
/// ordinals as `name + ("h" if name ends with "t" else "th")` except for a fixed few, so
/// "ninth" is absent (it generates "nineth").
fn ones_suffixed(w: &str) -> Option<(u128, &'static str)> {
    if w == "sixes" {
        return Some((6, "s"));
    }
    if let Some(stem) = w.strip_suffix('s')
        && stem != "six"
        && let Some(v) = ones(stem)
    {
        return Some((v, "s"));
    }
    match w {
        "zeroth" => return Some((0, "th")),
        "first" => return Some((1, "st")),
        "second" => return Some((2, "nd")),
        "third" => return Some((3, "rd")),
        "fifth" => return Some((5, "th")),
        "twelfth" => return Some((12, "th")),
        _ => {}
    }
    ONES.iter().enumerate().find_map(|(i, name)| {
        let v = i as u128 + 1;
        let ext = if name.ends_with('t') { "h" } else { "th" };
        let generated = v > 3 && v != 5 && v != 12;
        (generated && w.strip_suffix(ext) == Some(*name)).then_some((v, "th"))
    })
}

/// `self.tens`.
fn tens(w: &str) -> Option<u128> {
    TENS.iter().find(|(n, _)| *n == w).map(|&(_, v)| v)
}

/// `self.tens_suffixed`: `y` -> `ies` (plural) and `y` -> `ieth` (ordinal).
fn tens_suffixed(w: &str) -> Option<(u128, &'static str)> {
    TENS.iter().find_map(|&(name, v)| {
        if w == name.replace('y', "ies") {
            Some((v, "s"))
        } else if w == name.replace('y', "ieth") {
            Some((v, "th"))
        } else {
            None
        }
    })
}

/// `self.multipliers`.
fn multiplier(w: &str) -> Option<u128> {
    MULTIPLIERS.iter().find(|(n, _)| *n == w).map(|&(_, v)| v)
}

/// `self.multipliers_suffixed`: `name + "s"` and `name + "th"`.
fn multiplier_suffixed(w: &str) -> Option<(u128, &'static str)> {
    MULTIPLIERS.iter().find_map(|&(name, v)| {
        if w.strip_suffix('s') == Some(name) {
            Some((v, "s"))
        } else if w.strip_suffix("th") == Some(name) {
            Some((v, "th"))
        } else {
            None
        }
    })
}

/// `self.preceding_prefixers`.
fn preceding_prefixer(w: &str) -> Option<char> {
    match w {
        "minus" | "negative" => Some('-'),
        "plus" | "positive" => Some('+'),
        _ => None,
    }
}

/// `self.following_prefixers`.
fn following_prefixer(w: &str) -> Option<char> {
    match w {
        "pound" | "pounds" => Some('£'),
        "euro" | "euros" => Some('€'),
        "dollar" | "dollars" => Some('$'),
        "cent" | "cents" => Some('¢'),
        _ => None,
    }
}

/// `self.suffixers` keys (`per` only completes with a following `cent`).
fn is_suffixer(w: &str) -> bool {
    matches!(w, "per" | "percent")
}

/// `self.specials`.
fn is_special(w: &str) -> bool {
    matches!(w, "and" | "double" | "triple" | "point")
}

/// `self.words`: every word the number normaliser treats specially.
fn is_number_word(w: &str) -> bool {
    is_zero(w)
        || ones(w).is_some()
        || ones_suffixed(w).is_some()
        || tens(w).is_some()
        || tens_suffixed(w).is_some()
        || multiplier(w).is_some()
        || multiplier_suffixed(w).is_some()
        || preceding_prefixer(w).is_some()
        || following_prefixer(w).is_some()
        || is_suffixer(w)
        || is_special(w)
}

/// `self.decimals`: words that may follow `point`.
fn is_decimal_word(w: &str) -> bool {
    ones(w).is_some() || tens(w).is_some() || is_zero(w)
}

/// `^\d+(\.\d+)?$` (ASCII digits).
fn is_arabic(s: &str) -> bool {
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    digits(int) && frac.is_none_or(digits)
}

/// The running value: Python keeps an `int` or a `str`.
#[derive(Debug, Clone)]
enum Value {
    Int(u128),
    Str(String),
}

impl Value {
    /// `str(value)`.
    fn text(&self) -> String {
        match self {
            Value::Int(n) => n.to_string(),
            Value::Str(s) => s.clone(),
        }
    }
}

/// `str(value or "")`: `None`, `0` and `""` are falsy in Python.
fn text_or_empty(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Int(0)) => String::new(),
        Some(v) => v.text(),
    }
}

/// `Fraction(s)` for a token matching [`is_arabic`]: an integral value is stored as an
/// int (`"007"` -> 7, `"2.00"` -> 2), anything else keeps its text (`"1.50"`).
fn parse_arabic(s: &str) -> Value {
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if frac.bytes().all(|b| b == b'0') {
        match int.parse::<u128>() {
            Ok(n) => Value::Int(n),
            // Deviation: Python ints are unbounded; keep the digits as text.
            Err(_) => {
                let t = int.trim_start_matches('0');
                Value::Str(if t.is_empty() { "0".into() } else { t.into() })
            }
        }
    } else {
        Value::Str(s.to_string())
    }
}

/// `to_fraction(value) * multiplier` when the product is an integer, else `None`
/// (also `None` where upstream's `Fraction` raises, e.g. `"1.2.3"`). Accepts the
/// `Fraction` decimal forms `"12"`, `"1."`, `".5"`, `"1.5"`.
fn fraction_times(v: &Value, m: u128) -> Option<u128> {
    let s = match v {
        Value::Int(n) => return n.checked_mul(m),
        Value::Str(s) => s.as_str(),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || !digits(frac) || (int.is_empty() && frac.is_empty()) {
        return None;
    }
    let mut num: u128 = 0;
    for b in int.bytes().chain(frac.bytes()) {
        num = num.checked_mul(10)?.checked_add(u128::from(b - b'0'))?;
    }
    let den = 10u128.checked_pow(u32::try_from(frac.len()).ok()?)?;
    let p = num.checked_mul(m)?;
    p.is_multiple_of(den).then_some(p / den)
}

/// `before + residual * multiplier` with `before = v // 1000 * 1000`.
fn scale_int(v: u128, m: u128) -> Option<u128> {
    (v / 1000 * 1000).checked_add((v % 1000).checked_mul(m)?)
}

/// `value + x` when the low digits are zero, else the digits side by side (upstream's
/// `value += x` / `str(value) + str(x)` choice).
fn add_or_concat(v: u128, x: u128, modulus: u128) -> Value {
    if v.is_multiple_of(modulus)
        && let Some(sum) = v.checked_add(x)
    {
        Value::Int(sum)
    } else {
        Value::Str(format!("{v}{x}"))
    }
}

/// State of `process_words`: the pending `prefix` and `value`, plus the output tokens.
#[derive(Default)]
struct NumberState {
    prefix: Option<char>,
    value: Option<Value>,
    out: Vec<String>,
}

impl NumberState {
    /// Upstream `output(result)`: prepend the pending prefix, clear prefix and value.
    fn emit(&mut self, result: String) {
        let r = match self.prefix.take() {
            Some(p) => format!("{p}{result}"),
            None => result,
        };
        self.value = None;
        self.out.push(r);
    }

    /// `if value is not None: yield output(value)`.
    fn emit_value(&mut self) {
        if let Some(v) = self.value.take() {
            self.emit(v.text());
        }
    }
}

/// Upstream `EnglishNumberNormalizer.process_words`.
fn process_words(words: &[&str]) -> Vec<String> {
    let mut st = NumberState::default();
    let mut skip = false;
    let is_tens = |w: Option<&str>| w.is_some_and(|w| tens(w).is_some());
    let is_ones = |w: Option<&str>| w.is_some_and(|w| ones(w).is_some());
    for (i, &current) in words.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| words[j]);
        let next = words.get(i + 1).copied();
        if skip {
            skip = false;
            continue;
        }
        let next_is_numeric = next.is_some_and(is_arabic);
        let next_is_word = next.is_some_and(is_number_word);
        let first = current.chars().next().unwrap_or(' ');
        let has_prefix = PREFIXES.contains(&first);
        let without_prefix = if has_prefix {
            &current[first.len_utf8()..]
        } else {
            current
        };

        if is_arabic(without_prefix) {
            // arabic numbers (potentially with signs and fractions)
            if let Some(v) = &st.value {
                if let Value::Str(s) = v
                    && s.ends_with('.')
                {
                    // concatenate decimals / ip address components
                    st.value = Some(Value::Str(format!("{s}{current}")));
                    continue;
                }
                st.emit_value();
            }
            if has_prefix {
                st.prefix = Some(first);
            }
            st.value = Some(parse_arabic(without_prefix));
        } else if !is_number_word(current) {
            // non-numeric words
            st.emit_value();
            st.emit(current.to_string());
        } else if is_zero(current) {
            st.value = Some(Value::Str(text_or_empty(st.value.as_ref()) + "0"));
        } else if let Some(o) = ones(current) {
            let v = st.value.take();
            st.value = Some(match v {
                None => Value::Int(o),
                Some(Value::Int(v)) if !is_ones(prev) => {
                    add_or_concat(v, o, if o < 10 { 10 } else { 100 })
                }
                Some(v) => {
                    let t = v.text();
                    if is_tens(prev) && o < 10 {
                        // replace the last zero with the digit
                        Value::Str(format!("{}{o}", &t[..t.len() - 1]))
                    } else {
                        Value::Str(format!("{t}{o}"))
                    }
                }
            });
        } else if let Some((o, suffix)) = ones_suffixed(current) {
            // ordinal or cardinal; yield the number right away
            let text = match st.value.take() {
                None => format!("{o}{suffix}"),
                Some(Value::Int(v)) if !is_ones(prev) => {
                    let n = add_or_concat(v, o, if o < 10 { 10 } else { 100 });
                    format!("{}{suffix}", n.text())
                }
                Some(v) => {
                    let t = v.text();
                    if is_tens(prev) && o < 10 {
                        format!("{}{o}{suffix}", &t[..t.len() - 1])
                    } else {
                        format!("{t}{o}{suffix}")
                    }
                }
            };
            st.emit(text);
        } else if let Some(t) = tens(current) {
            st.value = Some(match st.value.take() {
                None => Value::Int(t),
                Some(Value::Str(s)) => Value::Str(format!("{s}{t}")),
                Some(Value::Int(v)) => add_or_concat(v, t, 100),
            });
        } else if let Some((t, suffix)) = tens_suffixed(current) {
            // ordinal or cardinal; yield the number right away
            let text = match st.value.take() {
                None => format!("{t}{suffix}"),
                Some(Value::Str(s)) => format!("{s}{t}{suffix}"),
                Some(Value::Int(v)) => format!("{}{suffix}", add_or_concat(v, t, 100).text()),
            };
            st.emit(text);
        } else if let Some(m) = multiplier(current) {
            match st.value.take() {
                None => st.value = Some(Value::Int(m)),
                Some(Value::Int(v)) if v != 0 => match scale_int(v, m) {
                    Some(n) => st.value = Some(Value::Int(n)),
                    // Deviation: overflow beyond u128 (Python ints are unbounded).
                    None => {
                        st.emit(v.to_string());
                        st.value = Some(Value::Int(m));
                    }
                },
                // a string or the int 0
                Some(v) => match fraction_times(&v, m) {
                    Some(p) => st.value = Some(Value::Int(p)),
                    None => {
                        st.emit(v.text());
                        st.value = Some(Value::Int(m));
                    }
                },
            }
        } else if let Some((m, suffix)) = multiplier_suffixed(current) {
            match st.value.take() {
                None => st.emit(format!("{m}{suffix}")),
                Some(Value::Str(s)) => match fraction_times(&Value::Str(s.clone()), m) {
                    Some(p) => st.emit(format!("{p}{suffix}")),
                    None => {
                        st.emit(s);
                        st.emit(format!("{m}{suffix}"));
                    }
                },
                Some(Value::Int(v)) => match scale_int(v, m) {
                    Some(n) => st.emit(format!("{n}{suffix}")),
                    // Deviation: overflow beyond u128 (Python ints are unbounded).
                    None => {
                        st.emit(v.to_string());
                        st.emit(format!("{m}{suffix}"));
                    }
                },
            }
        } else if let Some(sym) = preceding_prefixer(current) {
            // apply prefix (positive, minus, etc.) if it precedes a number
            st.emit_value();
            if next_is_word || next_is_numeric {
                st.prefix = Some(sym);
            } else {
                st.emit(current.to_string());
            }
        } else if let Some(sym) = following_prefixer(current) {
            // apply prefix (dollars, cents, etc.) only after a number
            if st.value.is_some() {
                st.prefix = Some(sym);
                st.emit_value();
            } else {
                st.emit(current.to_string());
            }
        } else if is_suffixer(current) {
            // apply suffix symbols (percent -> '%')
            match st.value.take() {
                Some(v) if current == "per" => {
                    if next == Some("cent") {
                        st.emit(v.text() + "%");
                        skip = true;
                    } else {
                        st.emit(v.text());
                        st.emit(current.to_string());
                    }
                }
                Some(v) => st.emit(v.text() + "%"),
                None => st.emit(current.to_string()),
            }
        } else if !next_is_word && !next_is_numeric {
            // a special word; apply special handling only if the next word can be numeric
            st.emit_value();
            st.emit(current.to_string());
        } else if current == "and" {
            // ignore "and" after hundreds, thousands, etc.
            if !prev.is_some_and(|p| multiplier(p).is_some()) {
                st.emit_value();
                st.emit(current.to_string());
            }
        } else if current == "double" || current == "triple" {
            if next.is_some_and(|n| ones(n).is_some() || is_zero(n)) {
                let repeats = if current == "double" { 2 } else { 3 };
                let digit = next.and_then(ones).unwrap_or(0).to_string();
                st.value = Some(Value::Str(
                    text_or_empty(st.value.as_ref()) + &digit.repeat(repeats),
                ));
                skip = true;
            } else {
                st.emit_value();
                st.emit(current.to_string());
            }
        } else if next.is_some_and(is_decimal_word) || next_is_numeric {
            // "point" before a decimal digit
            st.value = Some(Value::Str(text_or_empty(st.value.as_ref()) + "."));
        }
        // else: "point" before another number word is dropped, as upstream.
    }
    st.emit_value();
    st.out
}

/// Upstream `preprocess`: "<number> and a half" -> "<number> point five", spaces at
/// letter/digit boundaries, then re-attach `st nd rd th s` suffixes to digits.
fn preprocess_numbers(s: &str) -> String {
    let segments = split_and_a_half(s);
    let last = segments.len() - 1;
    let mut results: Vec<&str> = Vec::with_capacity(segments.len() * 2);
    for (i, segment) in segments.iter().enumerate() {
        if segment.trim().is_empty() {
            continue;
        }
        results.push(segment);
        if i != last {
            let last_word = segment.split_whitespace().last().unwrap_or("");
            if is_decimal_word(last_word) || multiplier(last_word).is_some() {
                results.push("point five");
            } else {
                results.push("and a half");
            }
        }
    }
    let joined = results.join(" ");

    // `([a-z])([0-9])` -> `\1 \2` and `([0-9])([a-z])` -> `\1 \2` (the two passes cannot
    // interact, so one scan is equivalent).
    let chars: Vec<char> = joined.chars().collect();
    let mut spaced: Vec<char> = Vec::with_capacity(chars.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        spaced.push(c);
        if let Some(&n) = chars.get(i + 1)
            && ((c.is_ascii_lowercase() && n.is_ascii_digit())
                || (c.is_ascii_digit() && n.is_ascii_lowercase()))
        {
            spaced.push(' ');
        }
    }

    // `([0-9])\s+(st|nd|rd|th|s)\b` -> `\1\2`
    let attach = |i: usize| -> Option<(usize, String)> {
        let c = spaced[i];
        if !c.is_ascii_digit() {
            return None;
        }
        let mut k = i + 1;
        while k < spaced.len() && spaced[k].is_whitespace() {
            k += 1;
        }
        if k == i + 1 {
            return None;
        }
        ["st", "nd", "rd", "th", "s"].iter().find_map(|suf| {
            let end = k + suf.len();
            let matched = end <= spaced.len()
                && spaced[k..end].iter().copied().eq(suf.chars())
                && !spaced.get(end).is_some_and(|&n| is_word(n));
            matched.then(|| (end, format!("{c}{suf}")))
        })
    };
    replace_scan(&spaced, attach)
}

/// `re.split(r"\band\s+a\s+half\b", s)`.
fn split_and_a_half(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let lit = |at: usize, w: &str| -> Option<usize> {
        let end = at + w.chars().count();
        (end <= n && chars[at..end].iter().copied().eq(w.chars())).then_some(end)
    };
    let spaces = |at: usize| -> Option<usize> {
        let mut k = at;
        while k < n && chars[k].is_whitespace() {
            k += 1;
        }
        (k > at).then_some(k)
    };
    let mut segments = Vec::new();
    let mut seg_start = 0;
    let mut i = 0;
    while i < n {
        let left_ok = i == 0 || !is_word(chars[i - 1]);
        let m = left_ok
            .then_some(i)
            .and_then(|k| lit(k, "and"))
            .and_then(spaces)
            .and_then(|k| lit(k, "a"))
            .and_then(spaces)
            .and_then(|k| lit(k, "half"))
            .filter(|&end| !chars.get(end).is_some_and(|&c| is_word(c)));
        if let Some(end) = m {
            segments.push(chars[seg_start..i].iter().collect());
            seg_start = end;
            i = end;
        } else {
            i += 1;
        }
    }
    segments.push(chars[seg_start..].iter().collect());
    segments
}

/// A run of 1-2 ASCII digits at `at` followed by a regex `\b` (`[0-9]{1,2}\b`); returns
/// the end index.
fn one_or_two_digits(chars: &[char], at: usize) -> Option<usize> {
    let mut end = at;
    while end < chars.len() && chars[end].is_ascii_digit() {
        end += 1;
    }
    let len = end - at;
    ((1..=2).contains(&len) && !chars.get(end).is_some_and(|&c| is_word(c))).then_some(end)
}

/// Upstream `postprocess`: `$2 and ¢7` -> `$2.07`, `$0.07` -> `¢7`, lone `1`/`1s` ->
/// `one`/`ones`.
fn postprocess_numbers(s: &str) -> String {
    let is_currency = |c: char| matches!(c, '€' | '£' | '$');

    // `([€£$])([0-9]+) (?:and )?¢([0-9]{1,2})\b` -> `{currency}{integer}.{cents:02d}`
    let chars: Vec<char> = s.chars().collect();
    let combine = |i: usize| -> Option<(usize, String)> {
        if !is_currency(chars[i]) {
            return None;
        }
        let mut j = i + 1;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j == i + 1 || chars.get(j) != Some(&' ') {
            return None;
        }
        let integer: String = chars[i + 1..j].iter().collect();
        let after_space = j + 1;
        let with_and = chars
            .get(after_space..after_space + 4)
            .is_some_and(|w| w.iter().copied().eq("and ".chars()));
        let starts = if with_and {
            vec![after_space + 4, after_space]
        } else {
            vec![after_space]
        };
        starts.into_iter().find_map(|k| {
            if chars.get(k) != Some(&'¢') {
                return None;
            }
            let end = one_or_two_digits(&chars, k + 1)?;
            let cents: u32 = chars[k + 1..end].iter().collect::<String>().parse().ok()?;
            Some((end, format!("{}{integer}.{cents:02}", chars[i])))
        })
    };
    let s = replace_scan(&chars, combine);

    // `[€£$]0.([0-9]{1,2})\b` -> `¢{int}` (the `.` matches any character but newline)
    let chars: Vec<char> = s.chars().collect();
    let extract = |i: usize| -> Option<(usize, String)> {
        if !is_currency(chars[i]) || chars.get(i + 1) != Some(&'0') {
            return None;
        }
        if !chars.get(i + 2).is_some_and(|&c| c != '\n') {
            return None;
        }
        let end = one_or_two_digits(&chars, i + 3)?;
        let cents: u32 = chars[i + 3..end].iter().collect::<String>().parse().ok()?;
        Some((end, format!("¢{cents}")))
    };
    let s = replace_scan(&chars, extract);

    // `(?<!\S)1(s?)(?!\S)` -> `one\1`
    let chars: Vec<char> = s.chars().collect();
    let free = |k: usize| chars.get(k).is_none_or(|c| c.is_whitespace());
    let one = |i: usize| -> Option<(usize, String)> {
        if chars[i] != '1' || !(i == 0 || chars[i - 1].is_whitespace()) {
            return None;
        }
        if chars.get(i + 1) == Some(&'s') && free(i + 2) {
            Some((i + 2, "ones".into()))
        } else if free(i + 1) {
            Some((i + 1, "one".into()))
        } else {
            None
        }
    };
    replace_scan(&chars, one)
}

/// Left-to-right, non-overlapping replacement (like `re.sub`): `at(i)` returns the end
/// of a match starting at `i` and its replacement.
fn replace_scan(chars: &[char], at: impl Fn(usize) -> Option<(usize, String)>) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some((end, rep)) = at(i) {
            out.push_str(&rep);
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Upstream `EnglishNumberNormalizer.__call__`.
fn standardize_numbers(s: &str) -> String {
    let pre = preprocess_numbers(s);
    let words: Vec<&str> = pre.split_whitespace().collect();
    postprocess_numbers(&process_words(&words).join(" "))
}

// ------------------------------------------------------------------ spellings

/// Whisper's British -> American spelling table, embedded unmodified from
/// `whisper/normalizers/english.json` (openai/whisper commit
/// 86098128c0b4f24f0e2aa2994de830614b474227, MIT licence, 1739 entries; see
/// `data/english.json.NOTICE`).
const ENGLISH_JSON: &str = include_str!("data/english.json");

static SPELLINGS: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

fn spellings() -> &'static HashMap<&'static str, &'static str> {
    SPELLINGS.get_or_init(|| {
        parse_flat_string_object(ENGLISH_JSON)
            .expect("embedded english.json is a flat object of plain strings")
    })
}

/// Parses a JSON object whose keys and values are strings without escapes (the shape of
/// `english.json`). Returns `None` for anything else.
fn parse_flat_string_object(src: &'static str) -> Option<HashMap<&'static str, &'static str>> {
    let b = src.as_bytes();
    let ws = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    // '"' and '\\' never occur inside multi-byte UTF-8 sequences, so byte slicing at
    // them stays on char boundaries.
    let string = |i: &mut usize| -> Option<&'static str> {
        if b.get(*i) != Some(&b'"') {
            return None;
        }
        let start = *i + 1;
        let len = b[start..].iter().position(|&c| c == b'"')?;
        let s = &src[start..start + len];
        if s.contains('\\') {
            return None;
        }
        *i = start + len + 1;
        Some(s)
    };
    let mut map = HashMap::new();
    let mut i = 0;
    ws(&mut i);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    ws(&mut i);
    if b.get(i) == Some(&b'}') {
        i += 1;
    } else {
        loop {
            ws(&mut i);
            let k = string(&mut i)?;
            ws(&mut i);
            if b.get(i) != Some(&b':') {
                return None;
            }
            i += 1;
            ws(&mut i);
            let v = string(&mut i)?;
            map.insert(k, v);
            ws(&mut i);
            match b.get(i) {
                Some(b',') => i += 1,
                Some(b'}') => {
                    i += 1;
                    break;
                }
                _ => return None,
            }
        }
    }
    ws(&mut i);
    (i == b.len()).then_some(map)
}

/// American spelling for a British-spelled word, from Whisper's full `english.json`
/// table (exact, lowercase word match, as upstream's `EnglishSpellingNormalizer`).
pub fn american_spelling(word: &str) -> Option<&'static str> {
    spellings().get(word).copied()
}

/// Number of entries in the embedded spelling table.
#[cfg(test)]
pub(crate) fn spelling_table_len() -> usize {
    spellings().len()
}
