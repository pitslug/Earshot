//! The nudge on meeting speech (T12.15c, docs/vocabulary.md "The nudge on meeting speech"):
//! AMI dev meetings, their named entities as the lists, and the non-speech clips of G4.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};

use crate::lists::{self, List};
use crate::refs;

/// The meetings of fixture set `ami-dev` (`ami-dev-ihm` pins their headsets), in rule order.
pub const MEETINGS: [&str; 5] = ["ES2003a", "ES2011a", "IS1008a", "TS3004a", "TS3006a"];
/// Meeting `k`'s draws and shuffle are seeded `lists::LIST_SEED + SEED_OFFSET + k`.
pub const SEED_OFFSET: u64 = 100_000;
/// The one variant: boosting open with this bonus.
pub const BETA: f32 = 2.0;
/// The verdict list and the informational ones, in report order.
pub const LISTS: [&str; 3] = ["ne", "ne-all", "rare"];
/// The named-entity types left out: everything under these (times and numbers).
pub const LEFT_OUT_TYPES: [&str; 2] = ["TIMEX", "NUMEX"];
pub const FETCH_AMI_DEV: &str = "run `cargo xtask fetch-fixtures --set ami-dev-ihm`";

/// Ids of the entity types under [`LEFT_OUT_TYPES`] in `ne-types.xml`.
pub fn left_out_type_ids(types_xml: &str) -> Result<HashSet<String>, String> {
    let mut out = HashSet::new();
    // The stack of open types: (id, name).
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut rest = types_xml;
    loop {
        let open = rest.find("<ne-type ");
        let close = rest.find("</ne-type>");
        match (open, close) {
            (Some(o), c) if c.is_none_or(|c| o < c) => {
                let tag_end = rest[o..].find('>').ok_or("unterminated <ne-type>")? + o;
                let tag = &rest[o..tag_end];
                let id = attr(tag, "nite:id").ok_or_else(|| format!("type without id: {tag}"))?;
                let name = attr(tag, "name").ok_or_else(|| format!("type without name: {tag}"))?;
                // stack[0] is the root; stack[1] (or this type) the top-level type.
                let top = stack.get(1).map_or(name, |(_, n)| n.as_str());
                if !stack.is_empty() && LEFT_OUT_TYPES.contains(&top) {
                    out.insert(id.to_owned());
                }
                if !tag.ends_with('/') {
                    stack.push((id.to_owned(), name.to_owned()));
                }
                rest = &rest[tag_end + 1..];
            }
            (_, Some(c)) => {
                stack.pop().ok_or("</ne-type> without an open type")?;
                rest = &rest[c + "</ne-type>".len()..];
            }
            (None, None) => break,
            (Some(_), None) => unreachable!("matched by the first arm"),
        }
    }
    if !stack.is_empty() {
        return Err("unclosed <ne-type>".to_owned());
    }
    Ok(out)
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let rest = &tag[at..];
    Some(&rest[..rest.find('"')?])
}

/// One speaker's words by their number in the words file (`<meeting>.<X>.wordsN`), with the
/// text `refs::parse_ami_words` keeps (punctuation and unfinished fragments map to nothing).
pub fn words_by_number(xml: &str) -> Result<BTreeMap<u64, String>, String> {
    let mut out = BTreeMap::new();
    let mut rest = xml;
    while let Some(at) = rest.find("<w ") {
        rest = &rest[at..];
        let tag = &rest[..rest.find('>').ok_or("unterminated <w>")?];
        if tag.ends_with('/') {
            rest = &rest[tag.len() + 1..];
            continue;
        }
        let end = rest.find("</w>").map(|e| e + 4).ok_or("<w> without </w>")?;
        let element = &rest[..end];
        rest = &rest[end..];
        let id = attr(tag, "nite:id").ok_or_else(|| format!("word without id: {tag}"))?;
        let n = word_number(id)?;
        // The reference reader's own rule, one word at a time.
        let kept = refs::parse_ami_words(&refs::with_start_times(element))?;
        if let Some(w) = kept.into_iter().next() {
            out.insert(n, w.text);
        }
    }
    Ok(out)
}

fn word_number(id: &str) -> Result<u64, String> {
    let at = id
        .rfind("words")
        .ok_or_else(|| format!("not a word id: {id}"))?;
    id[at + 5..]
        .parse()
        .map_err(|e| format!("bad word id {id}: {e}"))
}

/// The word numbers of every entity of a named-entity file not of a left-out type, in file
/// order (each entity's child ranges expanded).
pub fn entity_words(ne_xml: &str, left_out: &HashSet<String>) -> Result<Vec<u64>, String> {
    let mut out = Vec::new();
    let mut rest = ne_xml;
    while let Some(at) = rest.find("<named-entity ") {
        rest = &rest[at..];
        let end = rest
            .find("</named-entity>")
            .ok_or("<named-entity> without its end")?;
        let body = &rest[..end];
        rest = &rest[end..];
        let ty = body
            .split("href=\"ne-types.xml#id(")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .ok_or_else(|| format!("entity without a type: {body}"))?;
        if left_out.contains(ty) {
            continue;
        }
        for child in body.split("<nite:child ").skip(1) {
            let href = attr(&format!(" {child}"), "href")
                .ok_or_else(|| format!("child without href: {child}"))?
                .to_owned();
            let ids: Vec<&str> = href
                .split("#id(")
                .skip(1)
                .flat_map(|s| s.split("id("))
                .filter_map(|s| s.split(')').next())
                .filter(|s| !s.is_empty())
                .collect();
            let nums = ids
                .iter()
                .map(|i| word_number(i))
                .collect::<Result<Vec<_>, _>>()?;
            match nums.as_slice() {
                [one] => out.push(*one),
                [from, to] if from <= to => out.extend(*from..=*to),
                _ => return Err(format!("bad child range {href}")),
            }
        }
    }
    Ok(out)
}

/// Listed spoken tokens from entity words: `common` filtered when given, word-like only,
/// distinct in order, at most [`lists::LIST_LEN`].
pub fn entity_tokens(words: &[String], common: Option<&HashSet<String>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in words {
        for t in lists::tokens(w) {
            if lists::word_like(&t)
                && common.is_none_or(|c| !c.contains(&t))
                && !out.contains(&t)
                && out.len() < lists::LIST_LEN
            {
                out.push(t);
            }
        }
    }
    out
}

/// The `rare` list's spoken tokens: the meeting's tokens in order, word-like, not common, in
/// none of `others`, distinct, at most [`lists::LIST_LEN`].
pub fn rare_tokens(
    meeting_text: &str,
    common: &HashSet<String>,
    others: &HashSet<String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in lists::tokens(meeting_text) {
        if lists::word_like(&t)
            && !common.contains(&t)
            && !others.contains(&t)
            && !out.contains(&t)
            && out.len() < lists::LIST_LEN
        {
            out.push(t);
        }
    }
    out
}

/// The synthetic non-speech clips of model_eval's `silence` corpus (same names, levels, length,
/// seed and generator): uniform white noise at each RMS in dBFS.
pub fn silence_clips() -> Vec<(&'static str, Vec<f32>)> {
    const CLIPS: [(&str, f64); 3] = [
        ("dither-90dBFS", -90.0),
        ("noise-60dBFS", -60.0),
        ("noise-40dBFS", -40.0),
    ];
    const SECONDS: usize = 30;
    const SEED: u64 = 0x6e0_15e;
    let mut rng = ChaCha8Rng::seed_from_u64(SEED);
    let n = SECONDS * 16_000;
    CLIPS
        .iter()
        .map(|&(name, dbfs)| {
            // Uniform on [-a, a] has RMS a / sqrt(3).
            let a = 10f64.powf(dbfs / 20.0) * 3f64.sqrt();
            let clip = (0..n)
                .map(|_| {
                    let u = f64::from(rng.next_u32()) / f64::from(u32::MAX);
                    ((2.0 * u - 1.0) * a) as f32
                })
                .collect();
            (name, clip)
        })
        .collect()
}

/// One dev meeting: its speakers (oracle segments, references, audio), every token its words
/// hold, and its three lists in [`LISTS`] order.
pub struct Meeting {
    pub name: String,
    pub headsets: Vec<refs::AmiHeadset>,
    pub said: HashSet<String>,
    pub lists: Vec<List>,
}

fn read(fixtures: &Path, rel: &str) -> Result<String, String> {
    let p = fixtures.join(rel);
    std::fs::read(&p)
        .map(|b| refs::latin1(&b))
        .map_err(|e| format!("{}: {e}; {FETCH_AMI_DEV}", p.display()))
}

/// Every meeting with its lists (docs/vocabulary.md, T12.15c "Lists").
pub fn meetings(
    fixtures: &Path,
    common: &HashSet<String>,
    pool: &[String],
) -> Result<Vec<Meeting>, String> {
    let left_out = left_out_type_ids(&read(fixtures, "ami/annotations/ontologies/ne-types.xml")?)?;
    let headsets = refs::load_ami(fixtures, &MEETINGS, refs::AmiMic::Ihm)
        .map_err(|e| format!("{e} ({FETCH_AMI_DEV})"))?;
    // Per meeting: entity words in speaker then file order, all tokens, the UEM text.
    let mut entity: Vec<Vec<String>> = Vec::new();
    let mut said: Vec<HashSet<String>> = Vec::new();
    let mut text: Vec<String> = Vec::new();
    for meeting in MEETINGS {
        let mut words_of_entities = Vec::new();
        let mut all = HashSet::new();
        let mut speakers = Vec::new();
        for spk in refs::AMI_SPEAKERS {
            let words_xml = read(
                fixtures,
                &format!("ami/annotations/words/{meeting}.{spk}.words.xml"),
            )?;
            let ne_xml = read(
                fixtures,
                &format!("ami/annotations/namedEntities/{meeting}.{spk}.ne.xml"),
            )?;
            let by_number = words_by_number(&words_xml)?;
            for n in entity_words(&ne_xml, &left_out)? {
                if let Some(w) = by_number.get(&n) {
                    words_of_entities.push(w.clone());
                }
            }
            for w in by_number.values() {
                all.extend(lists::tokens(w));
            }
            let h = headsets
                .iter()
                .find(|h| h.meeting == meeting && h.speaker == spk)
                .ok_or_else(|| format!("{meeting} speaker {spk} not loaded"))?;
            speakers.push(h.words.clone());
        }
        let uem = headsets
            .iter()
            .find(|h| h.meeting == meeting)
            .map(|h| h.uem)
            .ok_or_else(|| format!("{meeting} not loaded"))?;
        text.push(refs::ami_reference(&speakers, uem.0, uem.1));
        entity.push(words_of_entities);
        said.push(all);
    }
    let mut out = Vec::new();
    for (k, meeting) in MEETINGS.iter().enumerate() {
        let others: HashSet<String> = said
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != k)
            .flat_map(|(_, s)| s.iter().cloned())
            .collect();
        let spoken = [
            entity_tokens(&entity[k], Some(common)),
            entity_tokens(&entity[k], None),
            rare_tokens(&text[k], common, &others),
        ];
        let said_set: HashSet<&str> = said[k].iter().map(String::as_str).collect();
        let seed = lists::LIST_SEED + SEED_OFFSET + k as u64;
        out.push(Meeting {
            name: (*meeting).to_owned(),
            headsets: headsets
                .iter()
                .filter(|h| h.meeting == *meeting)
                .cloned()
                .collect(),
            said: said[k].clone(),
            lists: spoken
                .into_iter()
                .map(|s| lists::fill(seed, s, &said_set, pool))
                .collect(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPES: &str = r#"<ne-type nite:id="ne_0" name="ne-root">
   <ne-type nite:id="ne_1" name="ENAMEX">
      <ne-type nite:id="ne_11" name="PERSON">
         <ne-type nite:id="ne_111" name="PARTICIPANT"/>
      </ne-type>
      <ne-type nite:id="ne_13" name="LOCATION"/>
   </ne-type>
   <ne-type nite:id="ne_2" name="TIMEX">
      <ne-type nite:id="ne_21" name="TIME"/>
   </ne-type>
   <ne-type nite:id="ne_3" name="NUMEX">
      <ne-type nite:id="ne_31" name="MONEY"/>
   </ne-type>
   <ne-type nite:id="ne_50" name="COLOUR"/>
</ne-type>"#;

    #[test]
    fn vocab_ami_left_out_types_are_everything_under_timex_and_numex() {
        let ids = left_out_type_ids(TYPES).unwrap();
        let mut ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["ne_2", "ne_21", "ne_3", "ne_31"]);
        assert!(left_out_type_ids("<ne-type nite:id=\"a\" name=\"x\">").is_err());
    }

    #[test]
    fn vocab_ami_entity_words_expand_ranges_and_skip_left_out_types() {
        let left_out: HashSet<String> = ["ne_21".to_owned()].into();
        let ne = r#"<named-entity nite:id="M.A.ne.1">
      <nite:pointer role="type"  href="ne-types.xml#id(ne_111)"/>
      <nite:child href="M.A.words.xml#id(M.A.words2)..id(M.A.words4)"/>
   </named-entity>
   <named-entity nite:id="M.A.ne.2">
      <nite:pointer role="type"  href="ne-types.xml#id(ne_21)"/>
      <nite:child href="M.A.words.xml#id(M.A.words7)"/>
   </named-entity>
   <named-entity nite:id="M.A.ne.3">
      <nite:pointer role="type"  href="ne-types.xml#id(ne_50)"/>
      <nite:child href="M.A.words.xml#id(M.A.words9)"/>
   </named-entity>"#;
        assert_eq!(entity_words(ne, &left_out).unwrap(), [2, 3, 4, 9]);
    }

    #[test]
    fn vocab_ami_words_by_number_keep_what_the_reference_keeps() {
        let xml = r#"<w nite:id="M.A.words0" starttime="1" endtime="2">Hello</w>
   <w nite:id="M.A.words1" starttime="2" endtime="2" punc="true">,</w>
   <w nite:id="M.A.words2" starttime="2" endtime="3">Zorbla</w>
   <w nite:id="M.A.words3" starttime="3" endtime="3" trunc="true">Qua</w>
   <w nite:id="M.A.words4">L_C_D_</w>"#;
        let w = words_by_number(xml).unwrap();
        let got: Vec<(u64, &str)> = w.iter().map(|(n, t)| (*n, t.as_str())).collect();
        assert_eq!(got, [(0, "Hello"), (2, "Zorbla"), (4, "LCD")]);
    }

    #[test]
    fn vocab_ami_list_tokens_follow_the_rule() {
        let common: HashSet<String> = ["red", "the"].map(str::to_owned).into();
        let words: Vec<String> = ["Zorbla", "red", "Zorbla", "Vantrix", "42"]
            .map(str::to_owned)
            .into();
        assert_eq!(entity_tokens(&words, Some(&common)), ["zorbla", "vantrix"]);
        assert_eq!(entity_tokens(&words, None), ["zorbla", "red", "vantrix"]);
        let others: HashSet<String> = ["quellit".to_owned()].into();
        assert_eq!(
            rare_tokens("the Quellit and Brannow the Brannow", &common, &others),
            ["and", "brannow"]
        );
    }

    #[test]
    fn vocab_ami_silence_clips_have_their_levels() {
        let clips = silence_clips();
        assert_eq!(clips.len(), 3);
        for ((_, clip), dbfs) in clips.iter().zip([-90.0f64, -60.0, -40.0]) {
            assert_eq!(clip.len(), 480_000);
            let rms = (clip.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / clip.len() as f64)
                .sqrt();
            assert!((20.0 * rms.log10() - dbfs).abs() < 0.1, "{dbfs}");
        }
    }
}
