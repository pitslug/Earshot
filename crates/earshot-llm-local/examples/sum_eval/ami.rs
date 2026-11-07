//! AMI manual annotations v1.6.2 for the summary evaluation: pure functions, unit-tested.
//!
//! Reference transcript rules (fixed before measuring):
//! - words from `words/<meeting>.<speaker>.words.xml`, `<w>` elements only (no vocal sounds,
//!   disfluency markers or gaps); `trunc="true"` fragments are left out (an ASR transcript
//!   has none); spelled acronyms lose their underscores (`L_C_D_` -> `LCD`); XML character
//!   references are decoded; files are ISO-8859-1;
//! - `punc="true"` entries are attached to the previous word without a space, so the
//!   transcript reads as the annotators punctuated it;
//! - one speaker's words form a turn while each word starts at most [`TURN_GAP_S`] after
//!   the previous word ended and the turn spans at most [`TURN_MAX_S`] (Earshot's own
//!   stretches are cut at pauses and at 30 s); all turns are ordered by start time;
//! - the speaker of a turn is the participant's role in `corpusResources/meetings.xml`
//!   (`role="PM"` -> "Project Manager", and so on): the human summaries name people by role.

/// Longest pause inside one turn (s).
pub const TURN_GAP_S: f64 = 1.0;
/// Longest turn (s), as Earshot's 30 s stretches.
pub const TURN_MAX_S: f64 = 30.0;

/// Decodes ISO-8859-1 bytes.
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let rest = &tag[at..];
    Some(&rest[..rest.find('"')?])
}

/// Decodes the XML entities and numeric character references AMI files use.
pub fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// One transcript token: a word, or punctuation attached to the word before it.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    /// Start (s).
    pub start: f64,
    /// End (s).
    pub end: f64,
    /// Text.
    pub text: String,
    /// Punctuation (`punc="true"`).
    pub punc: bool,
}

/// Parses a words file (rules in the module docs).
///
/// # Errors
/// A `<w>` without a closing tag or a parseable time.
pub fn parse_words(xml: &str) -> Result<Vec<Word>, String> {
    let mut words = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find("<w ") {
        rest = &rest[at..];
        let close = rest.find('>').ok_or("unterminated <w> tag")?;
        let tag = &rest[..close];
        if tag.ends_with('/') {
            rest = &rest[close + 1..];
            continue;
        }
        let body_end = rest.find("</w>").ok_or("<w> without </w>")?;
        let body = &rest[close + 1..body_end];
        rest = &rest[body_end + 4..];
        if attr(tag, "trunc") == Some("true") {
            continue;
        }
        let time = |n: &str| -> Result<f64, String> {
            attr(tag, n)
                .ok_or_else(|| format!("word without {n}: {tag}"))?
                .parse()
                .map_err(|e| format!("bad {n} in {tag}: {e}"))
        };
        let punc = attr(tag, "punc") == Some("true");
        let text = unescape(body.trim());
        let text = if punc { text } else { text.replace('_', "") };
        if !text.is_empty() {
            words.push(Word {
                start: time("starttime")?,
                end: time("endtime")?,
                text,
                punc,
            });
        }
    }
    Ok(words)
}

/// A speaker turn of the reference transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// Start (s).
    pub start: f64,
    /// End of its last word (s).
    pub end: f64,
    /// Speaker name.
    pub speaker: String,
    /// What was said.
    pub text: String,
}

/// One speaker's words as turns (rules in the module docs). Punctuation before any word
/// is dropped.
#[must_use]
pub fn turns(words: &[Word], speaker: &str) -> Vec<Turn> {
    let mut out: Vec<Turn> = Vec::new();
    let mut turn_start = 0.0;
    let mut last_end = f64::NEG_INFINITY;
    for w in words {
        if w.punc {
            if let Some(t) = out.last_mut() {
                t.text.push_str(&w.text);
            }
            continue;
        }
        let new =
            out.is_empty() || w.start - last_end > TURN_GAP_S || w.end - turn_start > TURN_MAX_S;
        if new {
            turn_start = w.start;
            out.push(Turn {
                start: w.start,
                end: w.end,
                speaker: speaker.to_owned(),
                text: w.text.clone(),
            });
        } else if let Some(t) = out.last_mut() {
            t.text.push(' ');
            t.text.push_str(&w.text);
            t.end = w.end;
        }
        last_end = w.end;
    }
    out
}

/// The role name of an AMI scenario role code.
#[must_use]
pub fn role_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "PM" => "Project Manager",
        "ME" => "Marketing Expert",
        "UI" => "User Interface Designer",
        "ID" => "Industrial Designer",
        _ => return None,
    })
}

/// A meeting's entry in `corpusResources/meetings.xml`.
#[derive(Debug, Clone, PartialEq)]
pub struct MeetingMeta {
    /// Length in seconds (`duration`; may be fractional).
    pub duration_s: f64,
    /// `(speaker letter, headset channel, role code)`, sorted by letter.
    pub speakers: Vec<(String, u8, String)>,
}

/// Parses `meeting`'s entry of `corpusResources/meetings.xml`.
///
/// # Errors
/// The meeting is missing or an attribute is missing or unparseable.
pub fn meeting_meta(xml: &str, meeting: &str) -> Result<MeetingMeta, String> {
    let key = format!(" observation=\"{meeting}\"");
    let at = xml
        .find(&key)
        .ok_or_else(|| format!("{meeting} not in meetings.xml"))?;
    let open_start = xml[..at].rfind('<').ok_or("no <meeting tag")?;
    let open_end = at + xml[at..].find('>').ok_or("unterminated <meeting>")?;
    let open = &xml[open_start..open_end];
    let duration_s = attr(open, "duration")
        .ok_or("meeting without duration")?
        .parse()
        .map_err(|e| format!("bad duration: {e}"))?;
    let body = &xml[open_end..];
    let body = &body[..body.find("</meeting>").ok_or("unterminated <meeting>")?];
    let mut speakers = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("<speaker ") {
        rest = &rest[i..];
        let end = rest.find('>').ok_or("unterminated <speaker>")?;
        let tag = &rest[..end];
        rest = &rest[end..];
        let letter = attr(tag, "nxt_agent").ok_or("speaker without nxt_agent")?;
        let channel = attr(tag, "channel")
            .ok_or("speaker without channel")?
            .parse()
            .map_err(|e| format!("bad channel: {e}"))?;
        let role = attr(tag, "role").ok_or("speaker without role")?;
        speakers.push((letter.to_owned(), channel, role.to_owned()));
    }
    speakers.sort();
    if speakers.is_empty() {
        return Err(format!("{meeting} has no speakers"));
    }
    Ok(MeetingMeta {
        duration_s,
        speakers,
    })
}

/// A human abstractive summary (`abstractive/<meeting>.abssumm.xml`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AbsSumm {
    /// The abstract's sentences.
    pub abstract_: Vec<String>,
    /// Action sentences (may be only "NA.").
    pub actions: Vec<String>,
    /// Decision sentences (may be only "NA.").
    pub decisions: Vec<String>,
    /// Problem sentences.
    pub problems: Vec<String>,
}

fn section_sentences(xml: &str, name: &str) -> Option<Vec<String>> {
    let open = format!("<{name} ");
    let close = format!("</{name}>");
    let a = xml.find(&open)?;
    let b = a + xml[a..].find(&close)?;
    let mut rest = &xml[a..b];
    let mut out = Vec::new();
    while let Some(i) = rest.find("<sentence") {
        rest = &rest[i..];
        let gt = rest.find('>')?;
        if rest[..gt].ends_with('/') {
            rest = &rest[gt + 1..];
            continue;
        }
        let end = rest.find("</sentence>")?;
        let text = unescape(rest[gt + 1..end].trim());
        if !text.is_empty() {
            out.push(text);
        }
        rest = &rest[end + "</sentence>".len()..];
    }
    Some(out)
}

/// Parses an abstractive summary.
///
/// # Errors
/// No abstract, or no actions or decisions section.
pub fn parse_abssumm(xml: &str) -> Result<AbsSumm, String> {
    let s = AbsSumm {
        abstract_: section_sentences(xml, "abstract").ok_or("no <abstract>")?,
        actions: section_sentences(xml, "actions").ok_or("no <actions>")?,
        decisions: section_sentences(xml, "decisions").ok_or("no <decisions>")?,
        problems: section_sentences(xml, "problems").unwrap_or_default(),
    };
    if s.abstract_.is_empty() {
        return Err("empty <abstract>".into());
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ami_words_keep_punctuation_and_drop_fragments() {
        let xml = concat!(
            r#"<w nite:id="a" starttime="0.37" endtime="0.95">Hmm</w>"#,
            r#"<w nite:id="b" starttime="0.95" endtime="0.95" punc="true">,</w>"#,
            r#"<vocalsound nite:id="c" starttime="1.0" endtime="1.1" type="cough"/>"#,
            r#"<w nite:id="d" starttime="1.2" endtime="1.3" trunc="true">Tu</w>"#,
            r#"<w nite:id="e" starttime="1.4" endtime="1.9">L_C_D_</w>"#,
            r#"<w nite:id="f" starttime="2.0" endtime="2.2">caf&#233;&amp;co</w>"#,
        );
        let words = parse_words(&latin1(xml.as_bytes())).unwrap();
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Hmm", ",", "LCD", "caf\u{e9}&co"]);
        assert!(words[1].punc && !words[0].punc);
        assert!(parse_words(r#"<w starttime="x" endtime="1">a</w>"#).is_err());
    }

    fn w(start: f64, end: f64, text: &str) -> Word {
        Word {
            start,
            end,
            text: text.into(),
            punc: matches!(text, "." | "," | "?"),
        }
    }

    #[test]
    fn ami_turns_split_at_pauses_and_at_thirty_seconds() {
        let words = [
            w(0.0, 0.5, "Okay"),
            w(0.5, 0.5, "."),
            w(1.2, 1.5, "so"),
            w(3.0, 3.5, "next"),
            w(3.5, 3.5, "?"),
        ];
        let t = turns(&words, "Project Manager");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].text, "Okay. so");
        assert_eq!(t[1].text, "next?");
        assert_eq!(t[1].start, 3.0);
        // A continuous 40 s stretch is cut once it would pass 30 s.
        let long: Vec<Word> = (0..80)
            .map(|i| w(f64::from(i) * 0.5, f64::from(i) * 0.5 + 0.5, "la"))
            .collect();
        let t = turns(&long, "X");
        assert_eq!(t.len(), 2);
        assert!(t[0].text.split(' ').count() <= 60);
        // Leading punctuation has no word to attach to.
        assert!(turns(&[w(0.0, 0.0, ".")], "X").is_empty());
    }

    #[test]
    fn ami_meeting_meta_reads_roles_channels_and_duration() {
        let xml = r#"<meetings>
   <meeting nite:id="meet_49" type="scenario" observation="ES2004a" duration="1186" k5="1">
      <speaker nite:id="ES2004a_2" channel="1" nxt_agent="B" role="PM"/>
      <speaker nite:id="ES2004a_1" channel="0" nxt_agent="A" role="UI"/>
   </meeting>
   <meeting nite:id="meet_50" observation="ES2004b" duration="2333">
      <speaker nite:id="x" channel="0" nxt_agent="A" role="ME"/>
   </meeting></meetings>"#;
        let m = meeting_meta(xml, "ES2004a").unwrap();
        assert_eq!(m.duration_s, 1186.0);
        // Some meetings give fractional seconds (TS3004a: 1345.324).
        let frac = xml.replace("duration=\"1186\"", "duration=\"1345.324\"");
        assert_eq!(meeting_meta(&frac, "ES2004a").unwrap().duration_s, 1345.324);
        assert_eq!(
            m.speakers,
            [
                ("A".to_string(), 0, "UI".to_string()),
                ("B".to_string(), 1, "PM".to_string())
            ]
        );
        assert_eq!(meeting_meta(xml, "ES2004b").unwrap().speakers.len(), 1);
        assert!(meeting_meta(xml, "IS1009a").is_err());
        assert_eq!(role_name("PM"), Some("Project Manager"));
        assert_eq!(role_name("XX"), None);
    }

    #[test]
    fn ami_abssumm_sections_parse_with_na_markers() {
        let xml = r#"<nite:root>
<abstract nite:id="a"><sentence nite:id="s1">The PM opened.</sentence> <sentence nite:id="s2">They drew &amp; talked.</sentence></abstract>
<actions nite:id="b"><sentence nite:id="s3">NA.</sentence></actions>
<decisions nite:id="c">
<sentence nite:id="s4">The remote will be yellow.</sentence>
</decisions>
</nite:root>"#;
        let s = parse_abssumm(xml).unwrap();
        assert_eq!(s.abstract_, ["The PM opened.", "They drew & talked."]);
        assert_eq!(s.actions, ["NA."]);
        assert_eq!(s.decisions, ["The remote will be yellow."]);
        assert!(s.problems.is_empty());
        assert!(
            parse_abssumm("<abstract x></abstract><actions x></actions><decisions x></decisions>")
                .is_err()
        );
        assert!(parse_abssumm(&xml.replace("actions", "other")).is_err());
    }

    #[test]
    fn ami_unescape_handles_named_numeric_and_stray_ampersands() {
        assert_eq!(
            unescape("a&amp;b &#39;c&#x41; & d&bogus;"),
            "a&b 'cA & d&bogus;"
        );
    }
}
