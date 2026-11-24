//! `summary_names`: the T12.91 measurement of which saved people's names summary requests
//! carry (docs/summary-names.md, whose rule was written before this tool ran). Evaluation
//! only: it runs the production filter and prompt builder (`earshot_core::prompt` with
//! `SummaryInput::people`) on AMI dev reference transcripts with an invented name list and
//! injected mentions, and writes counts only: no transcript text, no name.
//!
//! ```text
//! cargo run --release -p earshot-core --example summary_names -- \
//!     --annotations <EARSHOT_FIXTURES>/ami/annotations --out docs/summary-names/ami-dev.json
//! cargo run --release -p earshot-core --example summary_names -- --dry-run
//! ```
//!
//! `--dry-run` measures a small synthetic meeting built here instead of AMI, to show the tool
//! works; its numbers mean nothing.

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use earshot_core::asr::MeetingTime;
use earshot_core::identity::EntityId;
use earshot_core::prompt::{
    ContextBudget, MAX_PEOPLE_PER_REQUEST, MAX_PERSON_NAME_CHARS, MeetingInfo, MergeLevel, People,
    PromptPlan, PromptSegment, SummaryInput, Template, Vocabulary, carried_names, plan,
    plan_chunks, reduce_prompt, reduce_prompt_carrying, time_chunks,
};
use earshot_core::summary::SummaryV1;
use earshot_core::vocabulary::names::{NameFilter, listed_words};
use earshot_core::vocabulary::suggest::{Suggester, is_common, sound_key};
use serde::Serialize;

#[allow(dead_code)]
#[path = "../../../earshot-llm-local/examples/sum_eval/ami.rs"]
mod ami;

/// The AMI dev meetings of fixture set `ami-sum` (docs/summary-eval.md: the dev split).
const DEV_MEETINGS: [&str; 20] = [
    "ES2003a", "ES2003b", "ES2003c", "ES2003d", "ES2011a", "ES2011b", "ES2011c", "ES2011d",
    "IS1008a", "IS1008b", "IS1008c", "IS1008d", "TS3004a", "TS3004b", "TS3004c", "TS3004d",
    "TS3006a", "TS3006b", "TS3006c", "TS3006d",
];

/// How many invented names the list holds.
const INVENTED: usize = 400;

/// Invented names mentioned per meeting (each once whole, once with its surname misheard).
const MENTIONED: usize = 8;

/// The local writer's map window (`prompt::time_chunks`, `SummaryService::local`).
const MAP_SECONDS: u32 = 600;

/// The cap values the rule chooses from (docs/summary-names.md).
const CAPS: [usize; 10] = [5, 10, 15, 20, 25, 30, 40, 50, 75, 100];

/// The rule's bound on the median map request's growth, in estimated tokens (raised from 200
/// before any AMI measurement: the block's fixed text alone costs 191; docs/summary-names.md).
const MAX_MEDIAN_MAP_GROWTH: usize = 400;

/// A budget large enough that every request is planned as asked: only the names are
/// measured here, never whether a model's context holds them.
const MAP_BUDGET: ContextBudget = ContextBudget {
    context_window: 1_000_000,
    max_output_tokens: 4_096,
};

#[derive(serde::Deserialize)]
struct Seeds {
    first_start: Vec<String>,
    first_end: Vec<String>,
    first_common: Vec<String>,
    last_start: Vec<String>,
    last_end: Vec<String>,
}

/// Every `start` + `end`, in order.
fn joined(start: &[String], end: &[String]) -> Vec<String> {
    start
        .iter()
        .flat_map(|a| end.iter().map(move |b| format!("{a}{b}")))
        .collect()
}

/// The invented first names and surnames (seeds.toml).
fn seed_names() -> (Vec<String>, Vec<String>) {
    let seeds: Seeds = toml::from_str(include_str!("seeds.toml")).expect("seeds.toml");
    let mut first = joined(&seeds.first_start, &seeds.first_end);
    first.extend(seeds.first_common);
    (first, joined(&seeds.last_start, &seeds.last_end))
}

/// The invented names (seeds.toml): name i is `first[i % F]` and `last[(7 i) % L]`, so with
/// 200 first names and 400 surnames every surname is used once and every first name twice.
fn invented_names() -> Vec<String> {
    let (first, last) = seed_names();
    (0..INVENTED)
        .map(|i| format!("{} {}", first[i % first.len()], last[(7 * i) % last.len()]))
        .collect()
}

/// `word` misheard: its first vowel after the first letter replaced by another vowel (tried
/// in the order a, e, i, o, u), the first spelling that keeps the sound key, differs in lower
/// case and is not a common word. `None` when no such spelling exists.
fn mishear(word: &str) -> Option<String> {
    let chars: Vec<char> = word.chars().collect();
    let key = sound_key(word);
    let at = chars
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, c)| "aeiou".contains(c.to_ascii_lowercase()))?
        .0;
    for v in ['a', 'e', 'i', 'o', 'u'] {
        if v == chars[at].to_ascii_lowercase() {
            continue;
        }
        let mut c = chars.clone();
        c[at] = v;
        let heard: String = c.into_iter().collect();
        if sound_key(&heard) == key
            && heard.to_lowercase() != word.to_lowercase()
            && !is_common(&heard)
        {
            return Some(heard);
        }
    }
    None
}

/// FNV-1a of `s`: a fixed, platform-independent choice of names per meeting.
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// One injected mention.
#[derive(Debug, Clone)]
struct Mention {
    /// The segment it was appended to.
    segment: usize,
    /// The name it mentions (an index into the meeting's list).
    name: usize,
    /// Whether it is the whole name (else the misheard surname).
    exact: bool,
    /// The misheard spelling, for a misheard mention.
    heard: Option<String>,
}

/// `input` with `MENTIONED` invented names mentioned (docs/summary-names.md, "Injection"),
/// and the mentions. Names whose surname cannot be misheard get their whole mention only.
fn inject(input: &mut SummaryInput, meeting: &str, names: &[String]) -> Vec<Mention> {
    let n = input.segments.len();
    let h = usize::try_from(fnv(meeting) % INVENTED as u64).unwrap();
    let mut out = Vec::new();
    for j in 0..MENTIONED {
        let name = (h + j * 53) % INVENTED;
        let at = |k: usize| ((k * n) / (2 * MENTIONED)).min(n - 1);
        let exact_at = at(2 * j + 1);
        input.segments[exact_at].text.push(' ');
        input.segments[exact_at].text.push_str(&names[name]);
        out.push(Mention {
            segment: exact_at,
            name,
            exact: true,
            heard: None,
        });
        let surname = names[name].split(' ').nth(1).expect("two words");
        if let Some(heard) = mishear(surname) {
            let heard_at = at(2 * j + 2);
            input.segments[heard_at].text.push(' ');
            input.segments[heard_at].text.push_str(&heard);
            out.push(Mention {
                segment: heard_at,
                name,
                exact: false,
                heard: Some(heard),
            });
        }
    }
    out
}

/// The texts a map or single request over `range` is matched against, as the builder reads
/// them: the meeting's title and attendees, each segment's speaker and text (the AMI inputs
/// have no notes). Checked against the builder's own choice for every request.
fn request_texts(input: &SummaryInput, range: Range<usize>) -> Vec<&str> {
    let mut texts: Vec<&str> = input.meeting.title.as_deref().into_iter().collect();
    texts.extend(input.meeting.attendees.iter().map(String::as_str));
    for s in &input.segments[range] {
        texts.push(&s.speaker);
        texts.push(&s.text);
    }
    texts
}

/// One request's counts.
#[derive(Debug, Clone, Default, Serialize)]
struct RequestCounts {
    /// Names the production builder chose.
    chosen: usize,
    /// Names the filter matches with no cap.
    matched: usize,
    /// Chosen names that were not spoken (neither mentioned in the request nor a speaker of
    /// it) but share a word with a spoken name, or hold the word the matcher heard in a
    /// misheard mention: everyone who shares a spoken first name or surname.
    namesakes: usize,
    /// Every other chosen name that was not spoken.
    spurious: usize,
    /// Estimated tokens the names block adds.
    growth: usize,
}

/// One meeting's measurement.
#[derive(Debug, Default)]
struct MeetingCounts {
    maps: Vec<RequestCounts>,
    single: RequestCounts,
    merge: RequestCounts,
    /// Per mention: (exact, position of its name in the uncapped ranking of the map request
    /// holding it, and of the single request); `None` when not matched.
    mentions: Vec<(bool, Option<usize>, Option<usize>)>,
}

/// The namesakes and spurious names among `chosen` (see [`RequestCounts`]): `spoken` are the
/// names said in the request (mentioned, or its speakers), `mentions` the mentions in it.
fn unspoken(
    chosen: &[String],
    spoken: &[&str],
    mentions: &[&Mention],
    suggester: &Suggester,
) -> (usize, usize) {
    let mut words: HashSet<String> = spoken
        .iter()
        .flat_map(|n| n.split_whitespace().map(str::to_lowercase))
        .collect();
    words.extend(
        mentions
            .iter()
            .filter_map(|m| m.heard.as_deref())
            .filter_map(|h| suggester.suggest_word(h))
            .map(str::to_lowercase),
    );
    let mut namesakes = 0;
    let mut spurious = 0;
    for c in chosen.iter().filter(|c| !spoken.contains(&c.as_str())) {
        if c.split_whitespace()
            .any(|w| words.contains(&w.to_lowercase()))
        {
            namesakes += 1;
        } else {
            spurious += 1;
        }
    }
    (namesakes, spurious)
}

/// The counts of one request over `range` whose builder chose `chosen` and grew by `growth`.
#[allow(clippy::too_many_arguments)]
fn counts(
    filter: &NameFilter,
    suggester: &Suggester,
    list: &[String],
    input: &SummaryInput,
    range: Range<usize>,
    mentions: &[Mention],
    chosen: &[String],
    growth: usize,
) -> Result<(RequestCounts, Vec<usize>), String> {
    let texts = request_texts(input, range.clone());
    let ranked: Vec<usize> = filter.matches(&texts).into_iter().map(|(i, _)| i).collect();
    let mut expected: Vec<usize> = ranked
        .iter()
        .copied()
        .take(MAX_PEOPLE_PER_REQUEST)
        .collect();
    expected.sort_unstable();
    let built: Vec<usize> = chosen
        .iter()
        .map(|c| list.iter().position(|n| n == c).expect("a listed name"))
        .collect();
    if built != expected {
        return Err("the tool's request texts disagree with the prompt builder".into());
    }
    let here: Vec<&Mention> = mentions
        .iter()
        .filter(|m| range.contains(&m.segment))
        .collect();
    let mut spoken: Vec<&str> = input.segments[range]
        .iter()
        .map(|s| s.speaker.as_str())
        .collect();
    spoken.extend(here.iter().map(|m| list[m.name].as_str()));
    let (namesakes, spurious) = unspoken(chosen, &spoken, &here, suggester);
    Ok((
        RequestCounts {
            chosen: chosen.len(),
            matched: ranked.len(),
            namesakes,
            spurious,
            growth,
        },
        ranked,
    ))
}

/// Measures one meeting's `input` (speakers named as listed), with `invented` names and its
/// speakers listed.
fn measure(
    meeting: &str,
    plain: SummaryInput,
    invented: &[String],
) -> Result<MeetingCounts, String> {
    let mut input = plain.clone();
    let mentions = inject(&mut input, meeting, invented);
    let mut plain = plain;
    plain.segments.clone_from(&input.segments);
    let mut list: Vec<String> = invented.to_vec();
    list.extend(input.meeting.attendees.iter().cloned());
    input.people = People::new(&list, &Vocabulary::default());
    if input.people.names().len() != list.len() {
        return Err(format!("{meeting}: the list lost names"));
    }
    let list = input.people.names().to_vec();
    let filter = NameFilter::new(&list, &[] as &[&str]);
    // The matcher with the filter's own list (no Dictionary here).
    let suggester = Suggester::new(&listed_words(&[] as &[&str], &list));
    let mut out = MeetingCounts::default();

    let ranges = time_chunks(&input, MAP_SECONDS).map_err(|e| e.to_string())?;
    let maps = plan_chunks(&input, MAP_BUDGET, &ranges).map_err(|e| e.to_string())?;
    let plain_maps = plan_chunks(&plain, MAP_BUDGET, &ranges).map_err(|e| e.to_string())?;
    let mut map_ranked = Vec::new();
    for (m, p) in maps.iter().zip(&plain_maps) {
        let growth = m.prompt.estimated_tokens - p.prompt.estimated_tokens;
        let (c, ranked) = counts(
            &filter,
            &suggester,
            &list,
            &input,
            m.segments.clone(),
            &mentions,
            &m.prompt.people,
            growth,
        )?;
        out.maps.push(c);
        map_ranked.push((m.segments.clone(), ranked));
    }
    let single_budget = ContextBudget {
        context_window: 10_000_000,
        max_output_tokens: 16_384,
    };
    let (PromptPlan::Single(single), PromptPlan::Single(plain_single)) = (
        plan(&input, single_budget).map_err(|e| e.to_string())?,
        plan(&plain, single_budget).map_err(|e| e.to_string())?,
    ) else {
        return Err(format!("{meeting}: not one request"));
    };
    let (c, single_ranked) = counts(
        &filter,
        &suggester,
        &list,
        &input,
        0..input.segments.len(),
        &mentions,
        &single.people,
        single.estimated_tokens - plain_single.estimated_tokens,
    )?;
    out.single = c;
    // The last merge over the map parts, with partials that name nobody: what the parts
    // carried is all it holds.
    let parts: Vec<Vec<String>> = maps.iter().map(|m| m.prompt.people.clone()).collect();
    let partials = vec![SummaryV1::default(); maps.len()];
    let merge = reduce_prompt_carrying(
        &input,
        input.rules,
        &partials,
        &carried_names(&parts),
        MAP_BUDGET,
        MergeLevel::Last,
    );
    let plain_merge = reduce_prompt(&plain, &partials, MAP_BUDGET, MergeLevel::Last);
    let mut spoken: Vec<&str> = input.meeting.attendees.iter().map(String::as_str).collect();
    spoken.extend(mentions.iter().map(|m| list[m.name].as_str()));
    let all: Vec<&Mention> = mentions.iter().collect();
    let (namesakes, spurious) = unspoken(&merge.people, &spoken, &all, &suggester);
    out.merge = RequestCounts {
        chosen: merge.people.len(),
        matched: merge.people.len(),
        namesakes,
        spurious,
        growth: merge.estimated_tokens - plain_merge.estimated_tokens,
    };
    for m in &mentions {
        let in_map = map_ranked
            .iter()
            .find(|(r, _)| r.contains(&m.segment))
            .and_then(|(_, ranked)| ranked.iter().position(|&i| i == m.name));
        let in_single = single_ranked.iter().position(|&i| i == m.name);
        out.mentions.push((m.exact, in_map, in_single));
    }
    Ok(out)
}

/// Median and maximum.
#[derive(Debug, Serialize)]
struct Spread {
    median: usize,
    max: usize,
}

fn spread(values: &[usize]) -> Spread {
    let mut v = values.to_vec();
    v.sort_unstable();
    Spread {
        median: v.get(v.len() / 2).copied().unwrap_or(0),
        max: v.last().copied().unwrap_or(0),
    }
}

#[derive(Debug, Serialize)]
struct KindReport {
    requests: usize,
    chosen: Spread,
    matched: Spread,
    namesakes: Spread,
    spurious: Spread,
    growth_tokens: Spread,
}

fn kind(requests: &[&RequestCounts]) -> KindReport {
    let field =
        |f: fn(&RequestCounts) -> usize| -> Vec<usize> { requests.iter().map(|r| f(r)).collect() };
    KindReport {
        requests: requests.len(),
        chosen: spread(&field(|r| r.chosen)),
        matched: spread(&field(|r| r.matched)),
        namesakes: spread(&field(|r| r.namesakes)),
        spurious: spread(&field(|r| r.spurious)),
        growth_tokens: spread(&field(|r| r.growth)),
    }
}

#[derive(Debug, Serialize)]
struct Recall {
    mentions: usize,
    /// Mentions whose request chose the name with no cap (map requests).
    map_uncapped: usize,
    /// The same, single requests.
    single_uncapped: usize,
}

/// Which commit the run measured: `git rev-parse HEAD`, with `-dirty` when `git status
/// --porcelain` lists anything (a tracked change or an untracked file), read before the run
/// so the report it writes is not counted. `unknown` when git cannot say.
fn commit() -> String {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    match (git(&["rev-parse", "HEAD"]), git(&["status", "--porcelain"])) {
        (Some(head), Some(status)) if !head.is_empty() => {
            if status.is_empty() {
                head
            } else {
                format!("{head}-dirty")
            }
        }
        _ => "unknown".to_owned(),
    }
}

/// `at` (seconds since the Unix epoch) as a UTC time, `YYYY-MM-DDTHH:MM:SSZ` (the civil
/// date from days, after Howard Hinnant's `civil_from_days`).
fn utc(at: u64) -> String {
    let (days, secs) = (at / 86_400, at % 86_400);
    let z = i64::try_from(days).expect("a date") + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        secs / 60 % 60,
        secs % 60
    )
}

/// The run's provenance: what it measured and when.
#[derive(Debug, Clone, Serialize)]
struct Provenance {
    /// [`commit`] when the run started.
    commit: String,
    /// When the run started, UTC.
    started_utc: String,
}

impl Provenance {
    fn now() -> Self {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        Provenance {
            commit: commit(),
            started_utc: utc(at),
        }
    }
}

/// Every shipped constant the run read, as it read them.
#[derive(Debug, Serialize)]
struct Shipped {
    /// `earshot_core::prompt::MAX_PEOPLE_PER_REQUEST`.
    max_people_per_request: usize,
    /// `earshot_core::prompt::MAX_PERSON_NAME_CHARS`.
    max_person_name_chars: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    tool: &'static str,
    /// What the run measured and when (T12.91a).
    provenance: Provenance,
    /// The shipped constants it read (T12.91a).
    shipped: Shipped,
    source: String,
    meetings: usize,
    listed_invented: usize,
    shipped_cap: usize,
    map_seconds: u32,
    map: KindReport,
    single: KindReport,
    merge: KindReport,
    exact: Recall,
    misheard: Recall,
    /// Per candidate cap: injected mentions matched with no cap but dropped at that cap, in
    /// map and single requests together.
    lost_at_cap: BTreeMap<usize, usize>,
    /// The rule's outcome (docs/summary-names.md): the cap it picks and every failing bound.
    rule_cap: Option<usize>,
    rule_failures: Vec<String>,
}

fn report(
    provenance: Provenance,
    source: String,
    all: &[MeetingCounts],
    invented: usize,
) -> Report {
    let maps: Vec<&RequestCounts> = all.iter().flat_map(|m| &m.maps).collect();
    let singles: Vec<&RequestCounts> = all.iter().map(|m| &m.single).collect();
    let merges: Vec<&RequestCounts> = all.iter().map(|m| &m.merge).collect();
    let mentions: Vec<&(bool, Option<usize>, Option<usize>)> =
        all.iter().flat_map(|m| &m.mentions).collect();
    let recall = |exact: bool| {
        let of: Vec<_> = mentions.iter().filter(|m| m.0 == exact).collect();
        Recall {
            mentions: of.len(),
            map_uncapped: of.iter().filter(|m| m.1.is_some()).count(),
            single_uncapped: of.iter().filter(|m| m.2.is_some()).count(),
        }
    };
    let lost_at_cap: BTreeMap<usize, usize> = CAPS
        .iter()
        .map(|&c| {
            let lost = mentions
                .iter()
                .map(|m| {
                    usize::from(m.1.is_some_and(|p| p >= c))
                        + usize::from(m.2.is_some_and(|p| p >= c))
                })
                .sum();
            (c, lost)
        })
        .collect();
    let rule_cap = CAPS.iter().copied().find(|c| lost_at_cap[c] == 0);
    let (exact, misheard) = (recall(true), recall(false));
    let map = kind(&maps);
    let all_requests: Vec<&RequestCounts> = maps
        .iter()
        .chain(&singles)
        .chain(&merges)
        .copied()
        .collect();
    let mut failures = Vec::new();
    if exact.map_uncapped < exact.mentions || exact.single_uncapped < exact.mentions {
        failures.push("whole-name recall below 100%".to_owned());
    }
    if 2 * misheard.map_uncapped < misheard.mentions {
        failures.push("misheard recall below 50% (map requests)".to_owned());
    }
    if map.growth_tokens.median > MAX_MEDIAN_MAP_GROWTH {
        failures.push("median map request growth above 400 estimated tokens".to_owned());
    }
    if all_requests.iter().any(|r| r.growth > 1_500) {
        failures.push("a request grew by more than 1,500 estimated tokens".to_owned());
    }
    if map.spurious.median > 3 {
        failures.push("median spurious names per map request above 3".to_owned());
    }
    if rule_cap.is_none() {
        failures.push("no candidate cap keeps every matched mention".to_owned());
    }
    Report {
        tool: "summary_names (T12.91)",
        provenance,
        shipped: Shipped {
            max_people_per_request: MAX_PEOPLE_PER_REQUEST,
            max_person_name_chars: MAX_PERSON_NAME_CHARS,
        },
        source,
        meetings: all.len(),
        listed_invented: invented,
        shipped_cap: MAX_PEOPLE_PER_REQUEST,
        map_seconds: MAP_SECONDS,
        map,
        single: kind(&singles),
        merge: kind(&merges),
        exact,
        misheard,
        lost_at_cap,
        rule_cap,
        rule_failures: failures,
    }
}

fn read_latin1(p: &Path) -> Result<String, String> {
    std::fs::read(p).map(|b| ami::latin1(&b)).map_err(|e| {
        format!(
            "{}: {e}; run `cargo xtask fetch-fixtures --set ami-sum`",
            p.display()
        )
    })
}

/// The `global_name` of each speaker letter of `meeting` in `corpusResources/meetings.xml`.
fn global_names(xml: &str, meeting: &str) -> Result<BTreeMap<String, String>, String> {
    let key = format!(" observation=\"{meeting}\"");
    let at = xml
        .find(&key)
        .ok_or_else(|| format!("{meeting} not in meetings.xml"))?;
    let body = &xml[at..];
    let body = &body[..body.find("</meeting>").ok_or("unterminated <meeting>")?];
    let value = |tag: &str, name: &str| -> Option<String> {
        let k = format!(" {name}=\"");
        let i = tag.find(&k)? + k.len();
        Some(tag[i..].split('"').next()?.to_owned())
    };
    let mut out = BTreeMap::new();
    for tag in body.split("<speaker ").skip(1) {
        let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
        let letter = value(tag, "nxt_agent").ok_or("speaker without nxt_agent")?;
        let name = value(tag, "global_name").ok_or("speaker without global_name")?;
        out.insert(letter, name);
    }
    Ok(out)
}

/// A deterministic segment id for turn `n`.
fn turn_id(n: usize) -> EntityId {
    format!("0193a5b0-1c00-7000-8000-{n:012x}")
        .parse()
        .expect("a valid id")
}

/// `meeting`'s reference transcript (sum_eval's rules, `ami.rs`), each speaker named by their
/// AMI `global_name`.
fn ami_input(annotations: &Path, meeting: &str) -> Result<SummaryInput, String> {
    let xml = read_latin1(&annotations.join("corpusResources").join("meetings.xml"))?;
    let meta = ami::meeting_meta(&xml, meeting)?;
    let names = global_names(&xml, meeting)?;
    let mut turns = Vec::new();
    let mut attendees = Vec::new();
    for (letter, _, _) in &meta.speakers {
        let name = names
            .get(letter)
            .ok_or_else(|| format!("{meeting}: no global_name for {letter}"))?;
        attendees.push(name.clone());
        let path = annotations
            .join("words")
            .join(format!("{meeting}.{letter}.words.xml"));
        let w = ami::parse_words(&read_latin1(&path)?).map_err(|e| format!("{meeting}: {e}"))?;
        turns.extend(ami::turns(&w, name));
    }
    turns.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(input_of(
        attendees,
        (meta.duration_s * 1000.0).round() as u64,
        turns
            .into_iter()
            .map(|t| ((t.start * 1000.0).round() as u64, t.speaker, t.text))
            .collect(),
    ))
}

fn input_of(
    attendees: Vec<String>,
    duration_ms: u64,
    turns: Vec<(u64, String, String)>,
) -> SummaryInput {
    SummaryInput {
        meeting: MeetingInfo {
            title: None,
            started: None,
            duration_ms,
            attendees,
        },
        segments: turns
            .into_iter()
            .enumerate()
            .map(|(i, (start, speaker, text))| PromptSegment {
                id: turn_id(i + 1),
                start: MeetingTime::from_millis(start),
                speaker,
                text,
            })
            .collect(),
        notes: Vec::new(),
        template: Template::General,
        prompt_variant: Default::default(),
        rules: Default::default(),
        style: Default::default(),
        style_preset: Default::default(),
        vocabulary: Default::default(),
        people: Default::default(),
        user_name: None,
    }
}

/// The `--dry-run` meeting: 40 minutes of invented filler from two invented speakers.
fn synthetic() -> SummaryInput {
    let lines = [
        "so the plan for the next release is still open",
        "we will look at the budget again on Monday",
        "I can mark the open items and send them round",
        "the prototype needs a new case and a better button",
    ];
    input_of(
        vec!["SPK001".into(), "SPK002".into()],
        2_400_000,
        (0..240)
            .map(|i| {
                let speaker = if i % 2 == 0 { "SPK001" } else { "SPK002" };
                (
                    i as u64 * 10_000,
                    speaker.to_owned(),
                    lines[i % lines.len()].to_owned(),
                )
            })
            .collect(),
    )
}

struct Args {
    annotations: Option<PathBuf>,
    out: Option<PathBuf>,
    dry_run: bool,
}

fn args() -> Result<Args, String> {
    let mut a = Args {
        annotations: None,
        out: None,
        dry_run: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--annotations" => {
                a.annotations = Some(it.next().ok_or("--annotations needs a path")?.into())
            }
            "--out" => a.out = Some(it.next().ok_or("--out needs a path")?.into()),
            "--dry-run" => a.dry_run = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if !a.dry_run && a.annotations.is_none() {
        return Err("--annotations <dir> (or --dry-run) is needed".into());
    }
    Ok(a)
}

fn run() -> Result<(), String> {
    let a = args()?;
    let provenance = Provenance::now();
    let invented = invented_names();
    let (source, all) = if a.dry_run {
        (
            "dry run: one synthetic meeting".to_owned(),
            vec![measure("synthetic", synthetic(), &invented)?],
        )
    } else {
        let annotations = a.annotations.expect("checked");
        let mut all = Vec::new();
        for meeting in DEV_MEETINGS {
            all.push(measure(
                meeting,
                ami_input(&annotations, meeting)?,
                &invented,
            )?);
        }
        (
            "AMI dev (ami-sum dev split), reference transcripts, speakers by global_name"
                .to_owned(),
            all,
        )
    };
    let r = report(provenance, source, &all, invented.len());
    let json = serde_json::to_string_pretty(&r).map_err(|e| e.to_string())? + "\n";
    match a.out {
        Some(out) => {
            if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            std::fs::write(&out, &json).map_err(|e| format!("{}: {e}", out.display()))?;
        }
        None => print!("{json}"),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("summary_names: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_names_invented_names_are_four_hundred_distinct_full_names() {
        let names = invented_names();
        assert_eq!(names.len(), INVENTED);
        let mut lower: Vec<String> = names.iter().map(|n| n.to_lowercase()).collect();
        lower.sort();
        lower.dedup();
        assert_eq!(lower.len(), INVENTED);
        assert!(names.iter().all(|n| n.split(' ').count() == 2));
        // Every surname once, every first name at most twice, 200 first names, no word both a
        // first name and a surname; only the four seeded first names are common words.
        let count = |k: usize| {
            let mut m: BTreeMap<String, usize> = BTreeMap::new();
            for n in &names {
                *m.entry(n.split(' ').nth(k).unwrap().to_lowercase())
                    .or_default() += 1;
            }
            m
        };
        let (first, last) = (count(0), count(1));
        assert_eq!(last.len(), INVENTED);
        assert!(last.values().all(|&c| c == 1));
        assert_eq!(first.len(), 200);
        assert!(first.values().all(|&c| c <= 2));
        assert!(first.keys().all(|f| !last.contains_key(f)));
        let common: Vec<&String> = first.keys().filter(|f| is_common(f)).collect();
        // ("grace" is not among the 5,000 common AMI words; the seeded four stand for such
        // names anyway.)
        assert!(
            common
                .iter()
                .all(|c| ["grace", "mark", "rose", "will"].contains(&c.as_str()))
        );
        assert!(
            ["grace", "mark", "rose", "will"]
                .iter()
                .all(|w| first[*w] == 2)
        );
        assert_eq!(
            People::new(&names, &Vocabulary::default()).names().len(),
            INVENTED
        );
    }

    #[test]
    fn summary_names_reports_say_when_and_on_what_they_ran() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_795_564_799), "2026-11-24T23:59:59Z");
        let p = Provenance::now();
        assert!(p.started_utc.ends_with('Z') && p.started_utc.len() == 20);
        let hex = p.commit.trim_end_matches("-dirty");
        assert!(
            p.commit == "unknown"
                || (hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit())),
            "{}",
            p.commit
        );
        let r = report(p, "t".into(), &[], 0);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(
            json["shipped"]["max_people_per_request"],
            MAX_PEOPLE_PER_REQUEST
        );
        assert!(json["provenance"]["commit"].is_string());
    }

    #[test]
    fn summary_names_a_misheard_surname_keeps_its_sound() {
        for name in invented_names() {
            let surname = name.split(' ').nth(1).unwrap();
            if let Some(heard) = mishear(surname) {
                assert_eq!(sound_key(&heard), sound_key(surname));
                assert_ne!(heard.to_lowercase(), surname.to_lowercase());
                assert!(!is_common(&heard));
            }
        }
        assert_eq!(mishear("Oskarine").as_deref(), Some("Oskerine"));
        assert_eq!(mishear("xyz"), None);
    }

    #[test]
    fn summary_names_the_dry_run_agrees_with_the_builder_and_finds_its_mentions() {
        let invented = invented_names();
        let m = measure("synthetic", synthetic(), &invented).unwrap();
        assert_eq!(m.maps.len(), 4, "40 minutes in 10-minute windows");
        // Every whole-name mention is matched where it was said.
        assert!(
            m.mentions
                .iter()
                .filter(|x| x.0)
                .all(|x| x.1.is_some() && x.2.is_some())
        );
        assert!(m.mentions.iter().any(|x| !x.0), "a misheard mention");
        // Speakers are chosen in every request; the merge carries what the maps carried.
        assert!(m.maps.iter().all(|r| r.chosen >= 2 && r.growth > 0));
        assert!(m.merge.chosen >= m.maps.iter().map(|r| r.chosen).max().unwrap());
        // Namesakes and spurious names are told apart.
        let them = [
            "Avrel Quillmarsh".to_owned(),
            "Avrel Penstock".to_owned(),
            "Bryrin Penstock".to_owned(),
            "Lordris Saltwick".to_owned(),
        ];
        let mention = Mention {
            segment: 0,
            name: 0,
            exact: false,
            heard: Some("Saltwack".into()),
        };
        let suggester = Suggester::new(&listed_words(&[] as &[&str], &them));
        // "Avrel Penstock" shares a spoken word, "Lordris Saltwick" holds the heard word,
        // "Bryrin Penstock" shares nothing spoken.
        assert_eq!(
            unspoken(&them, &["Avrel Quillmarsh"], &[&mention], &suggester),
            (2, 1)
        );
        let r = report(Provenance::now(), "t".into(), &[m], invented.len());
        assert_eq!(r.exact.mentions, MENTIONED);
        let json = serde_json::to_string(&r).unwrap();
        // Counts only: no invented name reaches the report.
        assert!(
            invented
                .iter()
                .all(|n| !json.contains(n.split(' ').nth(1).unwrap()))
        );
    }
}
