//! `cargo xtask fetch-fixtures`: fixture manifest, LibriSpeech subset lists and the smoke set.
//!
//! The LibriSpeech subsets (PLAN.md 4.1) are pinned in `fixtures/librispeech/subset.toml`.
//! They are chosen by a deterministic rule ([`select_split`], [`select_smoke`]) from the
//! extracted corpus: speakers and utterances in numeric id order, no randomness.
//! `--pin` regenerates the list and the committed smoke set; `--verify` re-derives both from
//! the corpus and fails if the committed files differ.
//!
//! The echo canceller's held-out speech (T1.6b) is pinned the same way in
//! `fixtures/librispeech/aec_heldout.toml` ([`select_aec_heldout`]): the smoke rule applied to
//! the next female and male test-clean speaker, so it never shares a voice with the smoke set
//! the canceller is tuned on. Its audio stays in the fixture cache.
//!
//! The canceller's dev-wide speech (T1.6c) is pinned in `fixtures/librispeech/aec_dev.toml`
//! ([`select_aec_dev`]): four female and four male dev-clean speakers, paired both ways
//! round, for tuning and diagnosis on more voices than the smoke pair. dev-clean shares no
//! speaker with test-clean, so it never contains a held-out voice.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::download;
use crate::flac;
use crate::manifest::{self, Manifest};

/// Environment variable overriding the fixture cache directory.
pub const ENV: &str = "EARSHOT_FIXTURES";
/// Default cache directory below `%LOCALAPPDATA%`.
pub const DEFAULT_REL: &str = "earshot-dev\\fixtures";

/// Enrolment audio per speaker, seconds (PLAN.md 4.0: 10 s enrolment).
pub const ENROL_MIN_SECONDS: f64 = 10.0;
/// Further (test) utterances per speaker.
pub const TEST_UTTERANCES: usize = 3;
/// Minimum test utterance length, so a 2-3 s meeting-turn clip can be cut from it.
pub const TEST_MIN_SECONDS: f64 = 3.0;
/// test-clean has 40 speakers; all of them are pinned.
pub const TEST_CLEAN_SPEAKERS: usize = 40;
/// Minimum dev-clean speakers for open-set threshold calibration.
pub const DEV_MIN_SPEAKERS: usize = 20;

/// Smoke set size.
pub const SMOKE_UTTERANCES: usize = 20;
/// Speakers in the smoke set (first female and first male test-clean speaker by id).
pub const SMOKE_SPEAKERS: usize = 2;
/// Smoke utterance length window, seconds.
pub const SMOKE_MIN_SECONDS: f64 = 2.0;
/// Smoke utterance length window, seconds.
pub const SMOKE_MAX_SECONDS: f64 = 5.0;
/// Committed smoke audio budget, bytes (PLAN.md 4.1: < 5 MB).
pub const SMOKE_MAX_BYTES: u64 = 5_000_000;

/// Source of the smoke audio.
pub const TEST_CLEAN_URL: &str = "https://www.openslr.org/resources/12/test-clean.tar.gz";
/// LibriSpeech attribution used for generated manifest entries.
pub const LIBRISPEECH_ATTRIBUTION: &str = "LibriSpeech ASR corpus (Vassil Panayotov, Guoguo Chen, Daniel Povey, Sanjeev Khudanpur), https://www.openslr.org/12, derived from LibriVox public-domain audiobooks";

const SPLITS: [&str; 2] = ["test-clean", "dev-clean"];

/// Fixture cache directory (`EARSHOT_FIXTURES` or `%LOCALAPPDATA%\earshot-dev\fixtures`).
pub fn cache_dir() -> Result<PathBuf> {
    manifest::cache_dir(ENV, DEFAULT_REL)
}

/// `fixtures/manifest.toml`.
pub fn manifest_path(repo: &Path) -> PathBuf {
    repo.join("fixtures").join("manifest.toml")
}

/// `fixtures/librispeech/subset.toml`.
pub fn subset_path(repo: &Path) -> PathBuf {
    repo.join("fixtures")
        .join("librispeech")
        .join("subset.toml")
}

/// `fixtures/smoke`.
pub fn smoke_dir(repo: &Path) -> PathBuf {
    repo.join("fixtures").join("smoke")
}

/// One corpus utterance.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    /// `test-clean` or `dev-clean`.
    pub split: String,
    /// Speaker id.
    pub speaker: u32,
    /// Chapter id.
    pub chapter: u32,
    /// Utterance number within the chapter.
    pub index: u32,
    /// Duration in seconds.
    pub seconds: f64,
}

impl Utterance {
    /// LibriSpeech utterance id, e.g. `1089-134686-0000`.
    pub fn id(&self) -> String {
        format!("{}-{}-{:04}", self.speaker, self.chapter, self.index)
    }

    fn key(&self) -> (u32, u32, u32) {
        (self.speaker, self.chapter, self.index)
    }
}

/// Role of a pinned utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Enrolment audio.
    Enrol,
    /// Further utterance (identification test, WER).
    Test,
}

/// A pinned utterance in `subset.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pick {
    /// Split.
    pub split: String,
    /// Speaker id.
    pub speaker: u32,
    /// Utterance id.
    pub id: String,
    /// Role.
    pub role: Role,
    /// Duration in seconds (3 decimals).
    pub seconds: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubsetFile {
    #[serde(rename = "utterance")]
    utterances: Vec<Pick>,
}

/// Loads the committed subset list.
pub fn load_subset(repo: &Path) -> Result<Vec<Pick>> {
    let p = subset_path(repo);
    let text = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let f: SubsetFile = toml::from_str(&text).with_context(|| p.display().to_string())?;
    Ok(f.utterances)
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Pins one split: for each speaker (numeric id order), enrolment utterances in id order until
/// they total at least [`ENROL_MIN_SECONDS`], then the next [`TEST_UTTERANCES`] utterances (id
/// order) of at least [`TEST_MIN_SECONDS`]. Every speaker in the split must qualify.
pub fn select_split(corpus: &[Utterance], split: &str) -> Result<Vec<Pick>> {
    let mut by_spk: BTreeMap<u32, Vec<&Utterance>> = BTreeMap::new();
    for u in corpus.iter().filter(|u| u.split == split) {
        by_spk.entry(u.speaker).or_default().push(u);
    }
    ensure!(!by_spk.is_empty(), "no utterances for split {split}");
    let mut out = Vec::new();
    for (spk, mut utts) in by_spk {
        utts.sort_by_key(|u| u.key());
        let mut enrol = 0.0;
        let mut i = 0;
        while i < utts.len() && enrol < ENROL_MIN_SECONDS {
            enrol += utts[i].seconds;
            out.push(pick(utts[i], Role::Enrol));
            i += 1;
        }
        ensure!(
            enrol >= ENROL_MIN_SECONDS,
            "{split} speaker {spk}: only {enrol:.1} s of audio"
        );
        let tests: Vec<&Utterance> = utts[i..]
            .iter()
            .copied()
            .filter(|u| u.seconds >= TEST_MIN_SECONDS)
            .take(TEST_UTTERANCES)
            .collect();
        ensure!(
            tests.len() == TEST_UTTERANCES,
            "{split} speaker {spk}: not enough test utterances"
        );
        out.extend(tests.into_iter().map(|u| pick(u, Role::Test)));
    }
    Ok(out)
}

fn pick(u: &Utterance, role: Role) -> Pick {
    Pick {
        split: u.split.clone(),
        speaker: u.speaker,
        id: u.id(),
        role,
        seconds: round3(u.seconds),
    }
}

/// Checks the PLAN.md 4.1 constraints on a subset list (no corpus access needed).
pub fn check_subset(picks: &[Pick]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut per: BTreeMap<(&str, u32), (f64, Vec<f64>)> = BTreeMap::new();
    for p in picks {
        ensure!(
            SPLITS.contains(&p.split.as_str()),
            "unknown split {}",
            p.split
        );
        ensure!(ids.insert(p.id.as_str()), "utterance {} listed twice", p.id);
        ensure!(
            p.id.split('-').next() == Some(p.speaker.to_string().as_str()),
            "utterance {} does not belong to speaker {}",
            p.id,
            p.speaker
        );
        ensure!(p.seconds > 0.0, "utterance {} has no duration", p.id);
        let e = per.entry((p.split.as_str(), p.speaker)).or_default();
        match p.role {
            Role::Enrol => e.0 += p.seconds,
            Role::Test => e.1.push(p.seconds),
        }
    }
    for ((split, spk), (enrol, tests)) in &per {
        ensure!(
            *enrol >= ENROL_MIN_SECONDS,
            "{split} speaker {spk}: enrolment {enrol:.3} s < {ENROL_MIN_SECONDS} s"
        );
        ensure!(
            tests.len() >= TEST_UTTERANCES,
            "{split} speaker {spk}: {} test utterances < {TEST_UTTERANCES}",
            tests.len()
        );
        ensure!(
            tests.iter().all(|s| *s >= TEST_MIN_SECONDS),
            "{split} speaker {spk}: a test utterance is shorter than {TEST_MIN_SECONDS} s"
        );
    }
    let speakers = |split: &str| -> BTreeSet<u32> {
        per.keys().filter(|k| k.0 == split).map(|k| k.1).collect()
    };
    let test = speakers("test-clean");
    let dev = speakers("dev-clean");
    ensure!(
        test.len() == TEST_CLEAN_SPEAKERS,
        "test-clean subset has {} speakers, expected all {TEST_CLEAN_SPEAKERS}",
        test.len()
    );
    ensure!(
        dev.len() >= DEV_MIN_SPEAKERS,
        "dev-clean subset has {} speakers, need >= {DEV_MIN_SPEAKERS}",
        dev.len()
    );
    ensure!(
        test.is_disjoint(&dev),
        "dev-clean and test-clean speakers overlap"
    );
    Ok(())
}

/// Smoke rule: the first female and the first male test-clean speaker (numeric id order),
/// and for each the first `SMOKE_UTTERANCES / SMOKE_SPEAKERS` utterances in id order whose
/// length is within [`SMOKE_MIN_SECONDS`, `SMOKE_MAX_SECONDS`].
pub fn select_smoke<'a>(
    corpus: &'a [Utterance],
    gender: &BTreeMap<u32, char>,
) -> Result<Vec<&'a Utterance>> {
    select_short_pair(corpus, gender, 0)
}

/// Rank (0-based, per gender, numeric id order) of the held-out AEC speakers: the speakers
/// right after the smoke speakers (rank 0).
pub const AEC_HELDOUT_RANK: usize = 1;

/// Held-out AEC speech rule (T1.6b): [`select_smoke`]'s rule applied to the female and male
/// test-clean speaker of rank [`AEC_HELDOUT_RANK`], so no voice is shared with the smoke set.
/// Female first, then male, as the smoke set.
pub fn select_aec_heldout<'a>(
    corpus: &'a [Utterance],
    gender: &BTreeMap<u32, char>,
) -> Result<Vec<&'a Utterance>> {
    select_short_pair(corpus, gender, AEC_HELDOUT_RANK)
}

/// The female and the male test-clean speaker of `rank` (numeric id order within each
/// gender), each with the first `SMOKE_UTTERANCES / SMOKE_SPEAKERS` utterances in id order
/// whose length is within the smoke window.
fn select_short_pair<'a>(
    corpus: &'a [Utterance],
    gender: &BTreeMap<u32, char>,
    rank: usize,
) -> Result<Vec<&'a Utterance>> {
    let test: Vec<&Utterance> = corpus.iter().filter(|u| u.split == "test-clean").collect();
    let spk_ids: BTreeSet<u32> = test.iter().map(|u| u.speaker).collect();
    let per = SMOKE_UTTERANCES / SMOKE_SPEAKERS;
    let mut out = Vec::new();
    for g in ['F', 'M'] {
        let spk = spk_ids
            .iter()
            .copied()
            .filter(|s| gender.get(s) == Some(&g))
            .nth(rank)
            .with_context(|| format!("no {g} speaker of rank {rank} in test-clean"))?;
        let mut utts: Vec<&Utterance> = test
            .iter()
            .copied()
            .filter(|u| {
                u.speaker == spk && u.seconds >= SMOKE_MIN_SECONDS && u.seconds <= SMOKE_MAX_SECONDS
            })
            .collect();
        utts.sort_by_key(|u| u.key());
        ensure!(
            utts.len() >= per,
            "speaker {spk} has too few short utterances"
        );
        out.extend(utts.into_iter().take(per));
    }
    Ok(out)
}

/// `fixtures/librispeech/aec_heldout.toml`.
pub fn aec_heldout_path(repo: &Path) -> PathBuf {
    repo.join("fixtures")
        .join("librispeech")
        .join("aec_heldout.toml")
}

/// Which end of the simulated call a held-out utterance plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallEnd {
    /// Far end (loudspeaker; the male speaker, as smoke speaker 61).
    Far,
    /// Near end (local talker; the female speaker, as smoke speaker 121).
    Near,
}

/// One pinned held-out AEC utterance (`aec_heldout.toml`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldoutPick {
    /// LibriSpeech test-clean utterance id.
    pub id: String,
    /// Speaker id.
    pub speaker: u32,
    /// Call end.
    pub end: CallEnd,
    /// Duration in seconds (3 decimals).
    pub seconds: f64,
    /// Size of the corpus FLAC file, bytes.
    pub size: u64,
    /// SHA-256 of the corpus FLAC file.
    pub sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeldoutFile {
    #[serde(rename = "utterance")]
    utterances: Vec<HeldoutPick>,
}

/// Loads the committed held-out AEC speech list.
pub fn load_aec_heldout(repo: &Path) -> Result<Vec<HeldoutPick>> {
    let p = aec_heldout_path(repo);
    let text = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let f: HeldoutFile = toml::from_str(&text).with_context(|| p.display().to_string())?;
    Ok(f.utterances)
}

/// Checks a held-out list without corpus access: the smoke count per end, one speaker per
/// end and a different one at each end, ids of that speaker, the smoke length window, and no
/// speaker shared with `dev_speakers` (the smoke set).
pub fn check_aec_heldout(picks: &[HeldoutPick], dev_speakers: &BTreeSet<u32>) -> Result<()> {
    let per = SMOKE_UTTERANCES / SMOKE_SPEAKERS;
    for end in [CallEnd::Far, CallEnd::Near] {
        let mine: Vec<&HeldoutPick> = picks.iter().filter(|p| p.end == end).collect();
        ensure!(
            mine.len() == per,
            "held-out {end:?} end has {} utterances, expected {per}",
            mine.len()
        );
        let spk: BTreeSet<u32> = mine.iter().map(|p| p.speaker).collect();
        ensure!(
            spk.len() == 1,
            "held-out {end:?} end has {} speakers",
            spk.len()
        );
    }
    let speaker = |end| picks.iter().find(|p| p.end == end).map(|p| p.speaker);
    ensure!(
        speaker(CallEnd::Far) != speaker(CallEnd::Near),
        "held-out far and near end are the same speaker"
    );
    let mut ids = BTreeSet::new();
    for p in picks {
        ensure!(
            ids.insert(p.id.as_str()),
            "held-out utterance {} listed twice",
            p.id
        );
        ensure!(
            parse_id(&p.id).is_some_and(|(s, _, _)| s == p.speaker),
            "held-out utterance {} does not belong to speaker {}",
            p.id,
            p.speaker
        );
        ensure!(
            !dev_speakers.contains(&p.speaker),
            "held-out speaker {} is also a dev (smoke) speaker",
            p.speaker
        );
        ensure!(
            (SMOKE_MIN_SECONDS..=SMOKE_MAX_SECONDS).contains(&p.seconds),
            "held-out {}: {} s is outside the short-utterance window",
            p.id,
            p.seconds
        );
        ensure!(
            p.sha256.len() == 64 && p.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "held-out {}: bad sha256",
            p.id
        );
    }
    Ok(())
}

/// Speakers of the committed smoke set (the AEC dev speakers).
pub fn smoke_speakers(m: &Manifest) -> BTreeSet<u32> {
    smoke_artefacts(m)
        .0
        .iter()
        .filter_map(|(id, _)| parse_id(id).map(|(s, _, _)| s))
        .collect()
}

/// Checks every held-out file in the cache against its pinned size and SHA-256 and returns
/// the list. A missing or different file fails with the fetch instruction.
pub fn verified_aec_heldout(repo: &Path, cache: &Path) -> Result<Vec<HeldoutPick>> {
    let picks = load_aec_heldout(repo)?;
    let m = Manifest::load(&manifest_path(repo))?;
    check_aec_heldout(&picks, &smoke_speakers(&m))?;
    for p in &picks {
        let f = utterance_path(cache, "test-clean", &p.id)?;
        download::verify_file(
            &f,
            &download::Expected {
                sha256: &p.sha256,
                size: p.size,
            },
        )
        .with_context(|| {
            format!(
                "held-out AEC speech {} ({}): run `cargo xtask fetch-fixtures --set librispeech`",
                p.id,
                f.display()
            )
        })?;
    }
    Ok(picks)
}

fn aec_heldout_toml(entries: &[(&Utterance, CallEnd, u64, String)]) -> String {
    let mut s = String::from(
        "# Held-out speech of the echo-canceller evaluation (T1.6b; docs/aec-dev-split.md).\n\
         # Generated by `cargo xtask fetch-fixtures --set librispeech --pin`; checked by `--verify`\n\
         # and `cargo test -p xtask fixtures`. Do not edit by hand.\n\
         #\n\
         # Rule (xtask/src/fixtures.rs select_aec_heldout): the smoke rule applied to the second\n\
         # female and the second male test-clean speaker (numeric id order), so no voice is shared\n\
         # with the smoke set the canceller is tuned on: the first 10 utterances in id order of\n\
         # 2-5 s. As in the smoke set, the male speaker is the far end and the female the near end.\n\
         # Audio lives in the fixture cache (LibriSpeech, CC BY 4.0, fixtures/LICENSES.md) at\n\
         # librispeech/test-clean/LibriSpeech/test-clean/<speaker>/<chapter>/<id>.flac.\n",
    );
    for (u, end, size, sha) in entries {
        let end = match end {
            CallEnd::Far => "far",
            CallEnd::Near => "near",
        };
        let _ = write!(
            s,
            "\n[[utterance]]\nid = \"{}\"\nspeaker = {}\nend = \"{end}\"\nseconds = {:.3}\nsize = {size}\nsha256 = \"{sha}\"\n",
            u.id(),
            u.speaker,
            round3(u.seconds)
        );
    }
    s
}

/// The held-out list the rule gives on `corpus` (sizes and hashes read from the cache).
fn derive_aec_heldout(
    corpus: &[Utterance],
    genders: &BTreeMap<u32, char>,
    cache: &Path,
) -> Result<String> {
    let mut entries = Vec::new();
    for u in select_aec_heldout(corpus, genders)? {
        let end = match genders.get(&u.speaker) {
            Some('M') => CallEnd::Far,
            _ => CallEnd::Near,
        };
        let f = utterance_path(cache, "test-clean", &u.id())?;
        entries.push((u, end, fs::metadata(&f)?.len(), download::sha256_file(&f)?));
    }
    Ok(aec_heldout_toml(&entries))
}

/// Split of the echo canceller's dev-wide speech (T1.6c): LibriSpeech's own dev split, whose
/// speakers are disjoint from test-clean (the smoke and held-out voices).
pub const AEC_DEV_SPLIT: &str = "dev-clean";
/// Speakers per gender in the dev-wide speech ([`select_aec_dev`]).
pub const AEC_DEV_RANKS: usize = 4;

/// `fixtures/librispeech/aec_dev.toml`.
pub fn aec_dev_path(repo: &Path) -> PathBuf {
    repo.join("fixtures")
        .join("librispeech")
        .join("aec_dev.toml")
}

/// A far-end / near-end speaker pair of the dev-wide speech.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevPair {
    /// Far-end (loudspeaker) speaker.
    pub far: u32,
    /// Near-end (local talker) speaker.
    pub near: u32,
}

/// One pinned dev-wide utterance (`aec_dev.toml`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevPick {
    /// LibriSpeech dev-clean utterance id.
    pub id: String,
    /// Speaker id.
    pub speaker: u32,
    /// Duration in seconds (3 decimals).
    pub seconds: f64,
    /// Size of the corpus FLAC file, bytes.
    pub size: u64,
    /// SHA-256 of the corpus FLAC file.
    pub sha256: String,
}

/// The committed dev-wide AEC speech list.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AecDevFile {
    /// Speaker pairs, in report order.
    #[serde(rename = "pair")]
    pub pairs: Vec<DevPair>,
    /// Utterances, grouped by speaker.
    #[serde(rename = "utterance")]
    pub utterances: Vec<DevPick>,
}

/// Dev-wide AEC speech rule (T1.6c): per gender, the first [`AEC_DEV_RANKS`]
/// [`AEC_DEV_SPLIT`] speakers (numeric id order) that have at least
/// `SMOKE_UTTERANCES / SMOKE_SPEAKERS` utterances in the smoke length window (speakers with
/// fewer are skipped), each with the first that many of them in id order. The female and male
/// speaker of each rank are played both ways round: pair (male far, female near), then
/// (female far, male near). Returns the pairs and every utterance once (rank order, female
/// then male).
pub fn select_aec_dev<'a>(
    corpus: &'a [Utterance],
    gender: &BTreeMap<u32, char>,
) -> Result<(Vec<DevPair>, Vec<&'a Utterance>)> {
    let per = SMOKE_UTTERANCES / SMOKE_SPEAKERS;
    let split: Vec<&Utterance> = corpus.iter().filter(|u| u.split == AEC_DEV_SPLIT).collect();
    let spk_ids: BTreeSet<u32> = split.iter().map(|u| u.speaker).collect();
    let mut chosen: BTreeMap<char, Vec<Vec<&Utterance>>> = BTreeMap::new();
    for g in ['F', 'M'] {
        let picked = chosen.entry(g).or_default();
        for spk in spk_ids.iter().filter(|s| gender.get(s) == Some(&g)) {
            if picked.len() == AEC_DEV_RANKS {
                break;
            }
            let mut utts: Vec<&Utterance> = split
                .iter()
                .copied()
                .filter(|u| {
                    u.speaker == *spk
                        && u.seconds >= SMOKE_MIN_SECONDS
                        && u.seconds <= SMOKE_MAX_SECONDS
                })
                .collect();
            if utts.len() < per {
                continue;
            }
            utts.sort_by_key(|u| u.key());
            utts.truncate(per);
            picked.push(utts);
        }
        ensure!(
            picked.len() == AEC_DEV_RANKS,
            "{AEC_DEV_SPLIT} has {} qualifying {g} speakers, need {AEC_DEV_RANKS}",
            picked.len()
        );
    }
    let (mut pairs, mut out) = (Vec::new(), Vec::new());
    // Ranks in order: the female and the male speaker of each rank.
    for (fu, mu) in chosen[&'F'].iter().zip(&chosen[&'M']) {
        let (f, m) = (fu[0].speaker, mu[0].speaker);
        pairs.push(DevPair { far: m, near: f });
        pairs.push(DevPair { far: f, near: m });
        out.extend(fu.iter().copied());
        out.extend(mu.iter().copied());
    }
    Ok((pairs, out))
}

/// Loads the committed dev-wide AEC speech list.
pub fn load_aec_dev(repo: &Path) -> Result<AecDevFile> {
    let p = aec_dev_path(repo);
    let text = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    toml::from_str(&text).with_context(|| p.display().to_string())
}

/// Checks a dev-wide list without corpus access: at least one pair, distinct far and near
/// speakers per pair, no pair twice, exactly the smoke count of utterances for every paired
/// speaker and none for any other, ids of that speaker, the smoke length window, and no
/// speaker shared with `excluded` (the smoke and held-out speakers).
pub fn check_aec_dev(file: &AecDevFile, excluded: &BTreeSet<u32>) -> Result<()> {
    let per = SMOKE_UTTERANCES / SMOKE_SPEAKERS;
    ensure!(!file.pairs.is_empty(), "dev-wide AEC speech has no pairs");
    let mut seen = BTreeSet::new();
    let mut speakers = BTreeSet::new();
    for p in &file.pairs {
        ensure!(
            p.far != p.near,
            "dev-wide pair {}/{} plays one speaker at both ends",
            p.far,
            p.near
        );
        ensure!(
            seen.insert((p.far, p.near)),
            "dev-wide pair {}/{} listed twice",
            p.far,
            p.near
        );
        speakers.extend([p.far, p.near]);
    }
    let mut ids = BTreeSet::new();
    let mut count: BTreeMap<u32, usize> = BTreeMap::new();
    for u in &file.utterances {
        ensure!(
            ids.insert(u.id.as_str()),
            "dev-wide utterance {} listed twice",
            u.id
        );
        ensure!(
            parse_id(&u.id).is_some_and(|(s, _, _)| s == u.speaker),
            "dev-wide utterance {} does not belong to speaker {}",
            u.id,
            u.speaker
        );
        ensure!(
            speakers.contains(&u.speaker),
            "dev-wide utterance {} belongs to no pair",
            u.id
        );
        ensure!(
            (SMOKE_MIN_SECONDS..=SMOKE_MAX_SECONDS).contains(&u.seconds),
            "dev-wide {}: {} s is outside the short-utterance window",
            u.id,
            u.seconds
        );
        ensure!(
            u.sha256.len() == 64 && u.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "dev-wide {}: bad sha256",
            u.id
        );
        *count.entry(u.speaker).or_default() += 1;
    }
    for s in &speakers {
        ensure!(
            !excluded.contains(s),
            "dev-wide speaker {s} is a smoke or held-out speaker"
        );
        let n = count.get(s).copied().unwrap_or(0);
        ensure!(
            n == per,
            "dev-wide speaker {s} has {n} utterances, expected {per}"
        );
    }
    Ok(())
}

/// Speakers the dev-wide set must never use: the smoke (dev) and held-out speakers.
fn aec_dev_excluded(repo: &Path, m: &Manifest) -> Result<BTreeSet<u32>> {
    let mut s = smoke_speakers(m);
    s.extend(load_aec_heldout(repo)?.iter().map(|p| p.speaker));
    Ok(s)
}

/// Checks every dev-wide file in the cache against its pinned size and SHA-256 and returns
/// the list. A missing or different file fails with the fetch instruction.
pub fn verified_aec_dev(repo: &Path, cache: &Path) -> Result<AecDevFile> {
    let file = load_aec_dev(repo)?;
    let m = Manifest::load(&manifest_path(repo))?;
    check_aec_dev(&file, &aec_dev_excluded(repo, &m)?)?;
    for u in &file.utterances {
        let f = utterance_path(cache, AEC_DEV_SPLIT, &u.id)?;
        download::verify_file(
            &f,
            &download::Expected {
                sha256: &u.sha256,
                size: u.size,
            },
        )
        .with_context(|| {
            format!(
                "dev-wide AEC speech {} ({}): run `cargo xtask fetch-fixtures --set librispeech`",
                u.id,
                f.display()
            )
        })?;
    }
    Ok(file)
}

/// The dev-wide list the rule gives on `corpus` (sizes and hashes read from the cache).
fn derive_aec_dev(
    corpus: &[Utterance],
    genders: &BTreeMap<u32, char>,
    cache: &Path,
) -> Result<String> {
    let (pairs, utts) = select_aec_dev(corpus, genders)?;
    let mut s = String::from(
        "# Dev-wide speech of the echo-canceller evaluation (T1.6c): more dev voices, for tuning\n\
         # and diagnosis. Generated by `cargo xtask fetch-fixtures --set librispeech --pin`;\n\
         # checked by `--verify` and `cargo test -p xtask fixtures`. Do not edit by hand.\n\
         #\n\
         # Rule (xtask/src/fixtures.rs select_aec_dev): per gender, the first 4 dev-clean speakers\n\
         # (numeric id order) with at least 10 utterances of 2-5 s, each with the first 10 of\n\
         # them in id order; the female and male speaker of each rank are played both ways round\n\
         # (male far / female near, then female far / male near). dev-clean shares no voice with test-clean\n\
         # (the smoke and held-out speakers). Audio lives in the fixture cache (LibriSpeech,\n\
         # CC BY 4.0, fixtures/LICENSES.md) at\n\
         # librispeech/dev-clean/LibriSpeech/dev-clean/<speaker>/<chapter>/<id>.flac.\n",
    );
    for p in &pairs {
        let _ = write!(s, "\n[[pair]]\nfar = {}\nnear = {}\n", p.far, p.near);
    }
    for u in utts {
        let f = utterance_path(cache, AEC_DEV_SPLIT, &u.id())?;
        let _ = write!(
            s,
            "\n[[utterance]]\nid = \"{}\"\nspeaker = {}\nseconds = {:.3}\nsize = {}\nsha256 = \"{}\"\n",
            u.id(),
            u.speaker,
            round3(u.seconds),
            fs::metadata(&f)?.len(),
            download::sha256_file(&f)?
        );
    }
    Ok(s)
}

/// Extracted corpus directory of a split: `<cache>/librispeech/<split>/LibriSpeech/<split>`.
pub fn split_dir(cache: &Path, split: &str) -> PathBuf {
    cache
        .join("librispeech")
        .join(split)
        .join("LibriSpeech")
        .join(split)
}

/// Path of one utterance's FLAC file in the cache.
pub fn utterance_path(cache: &Path, split: &str, id: &str) -> Result<PathBuf> {
    let parts: Vec<&str> = id.split('-').collect();
    ensure!(parts.len() == 3, "bad utterance id {id}");
    Ok(split_dir(cache, split)
        .join(parts[0])
        .join(parts[1])
        .join(format!("{id}.flac")))
}

fn parse_id(stem: &str) -> Option<(u32, u32, u32)> {
    let mut it = stem.split('-').map(|s| s.parse::<u32>().ok());
    let r = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(r)
}

/// Scans an extracted split and reads every utterance's duration from its FLAC header.
pub fn scan_split(cache: &Path, split: &str) -> Result<Vec<Utterance>> {
    let dir = split_dir(cache, split);
    let mut out = Vec::new();
    for spk in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let spk = spk?.path();
        if !spk.is_dir() {
            continue;
        }
        for chap in fs::read_dir(&spk)? {
            let chap = chap?.path();
            if !chap.is_dir() {
                continue;
            }
            for f in fs::read_dir(&chap)? {
                let f = f?.path();
                if f.extension().and_then(|e| e.to_str()) != Some("flac") {
                    continue;
                }
                let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let (speaker, chapter, index) =
                    parse_id(stem).with_context(|| format!("unexpected file {}", f.display()))?;
                let si = flac::read(&f)?;
                ensure!(
                    si.sample_rate == 16_000 && si.channels == 1,
                    "{} is not 16 kHz mono",
                    f.display()
                );
                out.push(Utterance {
                    split: split.to_string(),
                    speaker,
                    chapter,
                    index,
                    seconds: si.seconds(),
                });
            }
        }
    }
    out.sort_by_key(|u| u.key());
    Ok(out)
}

/// Reads speaker genders from LibriSpeech `SPEAKERS.TXT` (`ID | SEX | SUBSET | ...`).
pub fn read_genders(text: &str) -> BTreeMap<u32, char> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with(';'))
        .filter_map(|l| {
            let mut f = l.split('|').map(str::trim);
            let id = f.next()?.parse().ok()?;
            let sex = f.next()?.chars().next()?;
            Some((id, sex))
        })
        .collect()
}

fn read_transcript(cache: &Path, split: &str, id: &str) -> Result<String> {
    let parts: Vec<&str> = id.split('-').collect();
    let p = split_dir(cache, split)
        .join(parts[0])
        .join(parts[1])
        .join(format!("{}-{}.trans.txt", parts[0], parts[1]));
    let text = fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    text.lines()
        .find_map(|l| l.strip_prefix(id).and_then(|r| r.strip_prefix(' ')))
        .map(str::to_string)
        .with_context(|| format!("no transcript for {id} in {}", p.display()))
}

fn subset_toml(picks: &[Pick]) -> String {
    let mut s = String::from(
        "# Pinned LibriSpeech subsets (PLAN.md 4.1). Generated by\n\
         # `cargo xtask fetch-fixtures --set librispeech --pin`; checked by `--verify` and\n\
         # `cargo test -p xtask fixtures`. Do not edit by hand.\n\
         #\n\
         # Rule (xtask/src/fixtures.rs select_split): every speaker of the split in numeric id\n\
         # order; enrolment = utterances in id order until they total >= 10 s; test = the next\n\
         # 3 utterances in id order that are >= 3 s long. Audio lives in the fixture cache at\n\
         # librispeech/<split>/LibriSpeech/<split>/<speaker>/<chapter>/<id>.flac.\n",
    );
    for p in picks {
        let role = match p.role {
            Role::Enrol => "enrol",
            Role::Test => "test",
        };
        let _ = write!(
            s,
            "\n[[utterance]]\nsplit = \"{}\"\nspeaker = {}\nid = \"{}\"\nrole = \"{role}\"\nseconds = {:.3}\n",
            p.split, p.speaker, p.id, p.seconds
        );
    }
    s
}

const SMOKE_BEGIN: &str = "# BEGIN generated smoke set";
const SMOKE_END: &str = "# END generated smoke set";

fn smoke_entries(files: &[(String, u64, String, String)]) -> String {
    let mut s = String::new();
    for (id, size, sha, member) in files {
        let path = if id == "transcripts" {
            "fixtures/smoke/transcripts.txt".to_string()
        } else {
            format!("fixtures/smoke/{id}.flac")
        };
        let member_line = if member.is_empty() {
            String::new()
        } else {
            format!("source_member = \"{member}\"\n")
        };
        let _ = write!(
            s,
            "\n[[artefact]]\nid = \"smoke-{id}\"\nsets = [\"smoke\"]\nlocation = \"repo\"\nurl = \"{TEST_CLEAN_URL}\"\n{member_line}path = \"{path}\"\nsha256 = \"{sha}\"\nsize = {size}\nlicence = \"CC-BY-4.0\"\nattribution = \"{LIBRISPEECH_ATTRIBUTION}\"\n"
        );
    }
    s
}

fn replace_block(text: &str, block: &str) -> Result<String> {
    let start = text
        .find(SMOKE_BEGIN)
        .context("manifest has no smoke begin marker")?;
    let end = text
        .find(SMOKE_END)
        .context("manifest has no smoke end marker")?;
    ensure!(start < end, "smoke markers out of order");
    let after_begin = start + text[start..].find('\n').context("marker line")? + 1;
    Ok(format!(
        "{}{}\n{}",
        &text[..after_begin],
        block,
        &text[end..]
    ))
}

/// Regenerates `subset.toml`, the smoke audio, its transcripts and the manifest smoke block
/// from the extracted corpus.
pub fn pin(repo: &Path, cache: &Path) -> Result<()> {
    let mut corpus = Vec::new();
    for split in SPLITS {
        corpus.extend(scan_split(cache, split)?);
    }
    let mut picks = Vec::new();
    for split in SPLITS {
        picks.extend(select_split(&corpus, split)?);
    }
    check_subset(&picks)?;
    let sp = subset_path(repo);
    fs::create_dir_all(sp.parent().unwrap())?;
    fs::write(&sp, subset_toml(&picks))?;
    println!("pin: wrote {} ({} utterances)", sp.display(), picks.len());

    let genders = read_genders(&fs::read_to_string(
        cache
            .join("librispeech")
            .join("test-clean")
            .join("LibriSpeech")
            .join("SPEAKERS.TXT"),
    )?);
    let smoke = select_smoke(&corpus, &genders)?;
    let dir = smoke_dir(repo);
    if dir.exists() {
        for f in fs::read_dir(&dir)? {
            let f = f?.path();
            if f.extension().and_then(|e| e.to_str()) == Some("flac") {
                fs::remove_file(f)?;
            }
        }
    }
    fs::create_dir_all(&dir)?;
    let mut entries = Vec::new();
    let mut transcripts = String::new();
    for u in &smoke {
        let id = u.id();
        let src = utterance_path(cache, "test-clean", &id)?;
        let dst = dir.join(format!("{id}.flac"));
        fs::copy(&src, &dst)?;
        let member = format!(
            "LibriSpeech/test-clean/{}/{}/{id}.flac",
            u.speaker, u.chapter
        );
        entries.push((
            id.clone(),
            fs::metadata(&dst)?.len(),
            download::sha256_file(&dst)?,
            member,
        ));
        let _ = writeln!(
            transcripts,
            "{id} {}",
            read_transcript(cache, "test-clean", &id)?
        );
    }
    let tp = dir.join("transcripts.txt");
    fs::write(&tp, &transcripts)?;
    entries.push((
        "transcripts".to_string(),
        fs::metadata(&tp)?.len(),
        download::sha256_file(&tp)?,
        String::new(),
    ));
    let mp = manifest_path(repo);
    let text = fs::read_to_string(&mp)?;
    fs::write(&mp, replace_block(&text, &smoke_entries(&entries))?)?;
    Manifest::load(&mp)?;
    println!(
        "pin: wrote {} smoke utterances to {}",
        smoke.len(),
        dir.display()
    );
    let hp = aec_heldout_path(repo);
    let held = derive_aec_heldout(&corpus, &genders, cache)?;
    // Checked before writing, so a rule that yields a bad list never replaces a good one.
    let parsed: HeldoutFile = toml::from_str(&held).context("derived aec_heldout.toml")?;
    check_aec_heldout(&parsed.utterances, &smoke_speakers(&Manifest::load(&mp)?))?;
    fs::write(&hp, held)?;
    println!("pin: wrote {}", hp.display());
    let dp = aec_dev_path(repo);
    let dev = derive_aec_dev(&corpus, &genders, cache)?;
    let parsed: AecDevFile = toml::from_str(&dev).context("derived aec_dev.toml")?;
    check_aec_dev(&parsed, &aec_dev_excluded(repo, &Manifest::load(&mp)?)?)?;
    fs::write(&dp, dev)?;
    println!("pin: wrote {}", dp.display());
    Ok(())
}

/// Smoke artefacts of the manifest as `(utterance id, artefact)`; the transcripts file is
/// returned separately.
pub fn smoke_artefacts(
    m: &Manifest,
) -> (
    Vec<(String, &manifest::Artefact)>,
    Option<&manifest::Artefact>,
) {
    let mut audio = Vec::new();
    let mut transcripts = None;
    for a in m
        .artefacts
        .iter()
        .filter(|a| a.sets.iter().any(|s| s == "smoke"))
    {
        if a.path.ends_with(".flac") {
            let id = a
                .path
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_end_matches(".flac")
                .to_string();
            audio.push((id, a));
        } else if a.path.ends_with("transcripts.txt") {
            transcripts = Some(a);
        }
    }
    (audio, transcripts)
}

/// Checks the committed smoke set: count, speakers, length window, size budget, checksums,
/// and one non-empty transcript per utterance.
pub fn check_smoke(repo: &Path, m: &Manifest) -> Result<()> {
    let (audio, transcripts) = smoke_artefacts(m);
    ensure!(
        audio.len() == SMOKE_UTTERANCES,
        "smoke set has {} utterances, expected {SMOKE_UTTERANCES}",
        audio.len()
    );
    let speakers: BTreeSet<&str> = audio
        .iter()
        .filter_map(|(id, _)| id.split('-').next())
        .collect();
    ensure!(
        speakers.len() >= SMOKE_SPEAKERS,
        "smoke set has {} speakers, need >= {SMOKE_SPEAKERS}",
        speakers.len()
    );
    let mut total = 0;
    for (id, a) in &audio {
        ensure!(
            parse_id(id).is_some(),
            "smoke file {id} is not a LibriSpeech id"
        );
        ensure!(
            a.licence == "CC-BY-4.0",
            "smoke {id}: licence {}",
            a.licence
        );
        let p = download::jail_join(repo, &a.path)?;
        download::verify_file(
            &p,
            &download::Expected {
                sha256: &a.sha256,
                size: a.size,
            },
        )?;
        let secs = flac::read(&p)?.seconds();
        ensure!(
            (SMOKE_MIN_SECONDS..=SMOKE_MAX_SECONDS).contains(&secs),
            "smoke {id}: {secs:.2} s is outside the short-utterance window"
        );
        total += a.size;
    }
    let t = transcripts.context("smoke set has no transcripts.txt artefact")?;
    let tp = download::jail_join(repo, &t.path)?;
    download::verify_file(
        &tp,
        &download::Expected {
            sha256: &t.sha256,
            size: t.size,
        },
    )?;
    total += t.size;
    ensure!(
        total < SMOKE_MAX_BYTES,
        "smoke set is {total} bytes, budget {SMOKE_MAX_BYTES}"
    );
    let text = fs::read_to_string(&tp)?;
    let mut have = BTreeMap::new();
    for l in text.lines() {
        let (id, words) = l.split_once(' ').context("transcript line without text")?;
        ensure!(!words.trim().is_empty(), "empty transcript for {id}");
        ensure!(
            have.insert(id, words).is_none(),
            "duplicate transcript {id}"
        );
    }
    let want: BTreeSet<&str> = audio.iter().map(|(id, _)| id.as_str()).collect();
    let got: BTreeSet<&str> = have.keys().copied().collect();
    ensure!(
        want == got,
        "transcripts do not match the smoke audio files"
    );
    Ok(())
}

/// With the corpus extracted: every listed file exists with the listed duration, and the
/// selection rules reproduce the committed subset list and smoke set exactly.
pub fn verify_librispeech(repo: &Path, cache: &Path, m: &Manifest) -> Result<()> {
    let picks = load_subset(repo)?;
    check_subset(&picks)?;
    let mut corpus = Vec::new();
    for split in SPLITS {
        corpus.extend(scan_split(cache, split)?);
    }
    let mut expected = Vec::new();
    for split in SPLITS {
        expected.extend(select_split(&corpus, split)?);
    }
    if expected != picks {
        bail!(
            "fixtures/librispeech/subset.toml differs from the selection rule applied to the corpus; rerun with --pin"
        );
    }
    for p in &picks {
        let f = utterance_path(cache, &p.split, &p.id)?;
        let secs = flac::read(&f)?.seconds();
        ensure!(
            (round3(secs) - p.seconds).abs() < 1e-6,
            "{}: {secs:.3} s on disk, list says {}",
            p.id,
            p.seconds
        );
    }
    let genders = read_genders(&fs::read_to_string(
        cache
            .join("librispeech")
            .join("test-clean")
            .join("LibriSpeech")
            .join("SPEAKERS.TXT"),
    )?);
    let smoke: Vec<String> = select_smoke(&corpus, &genders)?
        .iter()
        .map(|u| u.id())
        .collect();
    let (audio, _) = smoke_artefacts(m);
    let committed: Vec<String> = audio.iter().map(|(id, _)| id.clone()).collect();
    ensure!(
        smoke == committed,
        "committed smoke set differs from the selection rule; rerun with --pin"
    );
    for (id, a) in &audio {
        let src = utterance_path(cache, "test-clean", id)?;
        ensure!(
            download::sha256_file(&src)? == a.sha256,
            "smoke {id} is not a byte copy of the corpus file"
        );
    }
    let held = load_aec_heldout(repo)?;
    check_aec_heldout(&held, &smoke_speakers(m))?;
    ensure!(
        fs::read_to_string(aec_heldout_path(repo))?
            == derive_aec_heldout(&corpus, &genders, cache)?,
        "fixtures/librispeech/aec_heldout.toml differs from the selection rule applied to the corpus; rerun with --pin"
    );
    let dev = load_aec_dev(repo)?;
    check_aec_dev(&dev, &aec_dev_excluded(repo, m)?)?;
    ensure!(
        fs::read_to_string(aec_dev_path(repo))? == derive_aec_dev(&corpus, &genders, cache)?,
        "fixtures/librispeech/aec_dev.toml differs from the selection rule applied to the corpus; rerun with --pin"
    );
    println!(
        "verify: librispeech subset ({} utterances), smoke set, held-out AEC speech ({} utterances) and dev-wide AEC speech ({} utterances) match the corpus",
        picks.len(),
        held.len(),
        dev.utterances.len()
    );
    Ok(())
}

/// AMI test meetings (PLAN.md 4.1: 2 test meetings): the first two entries of
/// `lists/test.mini.txt` in pyannote AMI-diarization-setup at [`AMI_SETUP_COMMIT`].
pub const AMI_MEETINGS: [&str; 2] = ["IS1009a", "ES2004a"];
/// Pinned commit of https://github.com/pyannote/AMI-diarization-setup (Apache-2.0).
pub const AMI_SETUP_COMMIT: &str = "67c2d539286e89f68952d5dcf83912bd9f01dfae";
/// AMI audio channels: headset mix (IHM-mix), single distant mic (SDM, Array1-01) and the
/// four individual headsets (IHM, scored per speaker by `asr.wer_ami_ihm`, T2.5b).
pub const AMI_CHANNELS: [&str; 6] = [
    "Mix-Headset",
    "Array1-01",
    "Headset-0",
    "Headset-1",
    "Headset-2",
    "Headset-3",
];

/// AMI participants per meeting (both meetings have four).
pub const AMI_SPEAKERS: [&str; 4] = ["A", "B", "C", "D"];
/// AMI manual annotations v1.6.2: the word-level references of `asr.wer_ami_ihm` (T2.5).
pub const AMI_ANNOTATIONS_URL: &str =
    "https://groups.inf.ed.ac.uk/ami/AMICorpusAnnotations/ami_public_manual_1.6.2.zip";
/// Cache path of the annotations archive.
pub const AMI_ANNOTATIONS_PATH: &str = "ami/ami_public_manual_1.6.2.zip";
/// Cache directory the annotations archive is extracted into.
pub const AMI_ANNOTATIONS_DIR: &str = "ami/annotations";

/// AMI corpus resource mapping each speaker to a headset channel (T2.5b), relative to
/// [`AMI_ANNOTATIONS_DIR`].
pub const AMI_MEETINGS_XML: &str = "corpusResources/meetings.xml";

/// Summary-quality evaluation (T6.20): the AMI scenario-only partition's dev series (used
/// for settings choices) and test series (scored once). Each series is four meetings
/// (`a`-`d`) with a human abstractive summary.
pub const AMI_SUM_DEV_SERIES: [&str; 5] = ["ES2003", "ES2011", "IS1008", "TS3004", "TS3006"];
/// Test series of the same partition (it contains both [`AMI_MEETINGS`]).
pub const AMI_SUM_TEST_SERIES: [&str; 5] = ["ES2004", "ES2014", "IS1009", "TS3003", "TS3007"];

/// The meetings of the `dev` or `test` split of the summary evaluation, in order.
///
/// # Panics
/// On any other split name.
#[must_use]
pub fn ami_sum_meetings(split: &str) -> Vec<String> {
    let series: &[&str] = match split {
        "dev" => &AMI_SUM_DEV_SERIES,
        "test" => &AMI_SUM_TEST_SERIES,
        other => panic!("unknown summary split {other}"),
    };
    series
        .iter()
        .flat_map(|s| ["a", "b", "c", "d"].map(|p| format!("{s}{p}")))
        .collect()
}

/// AMI dev meeting speech (T12.15c, set `ami-dev-ihm`): the meetings of set `ami-dev` (the
/// first meeting of each dev series of the scenario-only partition) and the AMI-diarization-setup
/// folder their UEM is in (`uems/<folder>/<meeting>.uem`). Dev data only, never an acceptance
/// input.
pub const AMI_DEV_MEETINGS: [(&str, &str); 5] = [
    ("ES2003a", "train"),
    ("ES2011a", "dev"),
    ("IS1008a", "dev"),
    ("TS3004a", "dev"),
    ("TS3006a", "train"),
];
/// The named-entity type tree of the annotations archive (T12.15c).
pub const AMI_NE_TYPES: &str = "ontologies/ne-types.xml";

/// Pinned members of the annotations archive, relative to [`AMI_ANNOTATIONS_DIR`]: the
/// words files (`words/<meeting>.<speaker>.words.xml`) of every participant of
/// [`AMI_MEETINGS`] and of both summary splits ([`ami_sum_meetings`]), the oracle segments
/// (`segments/<meeting>.<speaker>.segments.xml`, T2.5b) of [`AMI_MEETINGS`] and
/// [`AMI_DEV_MEETINGS`] (the summary evaluation builds turns from the words), the named
/// entities (`namedEntities/<meeting>.<speaker>.ne.xml`) of [`AMI_DEV_MEETINGS`] with
/// [`AMI_NE_TYPES`] (T12.15c), the abstractive summaries (`abstractive/<meeting>.abssumm.xml`)
/// of the summary splits, and [`AMI_MEETINGS_XML`].
pub fn ami_annotation_members() -> Vec<String> {
    let mut v = vec![AMI_MEETINGS_XML.to_owned(), AMI_NE_TYPES.to_owned()];
    let mut meetings: Vec<String> = AMI_MEETINGS.iter().map(|m| (*m).to_owned()).collect();
    for meeting in AMI_MEETINGS {
        for spk in AMI_SPEAKERS {
            v.push(format!("segments/{meeting}.{spk}.segments.xml"));
        }
    }
    for (meeting, _) in AMI_DEV_MEETINGS {
        for spk in AMI_SPEAKERS {
            v.push(format!("segments/{meeting}.{spk}.segments.xml"));
            v.push(format!("namedEntities/{meeting}.{spk}.ne.xml"));
        }
    }
    for split in ["dev", "test"] {
        for m in ami_sum_meetings(split) {
            v.push(format!("abstractive/{m}.abssumm.xml"));
            meetings.push(m);
        }
    }
    meetings.sort();
    meetings.dedup();
    for meeting in meetings {
        for spk in AMI_SPEAKERS {
            v.push(format!("words/{meeting}.{spk}.words.xml"));
        }
    }
    v.sort();
    v
}

/// An AMI abstractive summary file (`abstractive/<meeting>.abssumm.xml`, ISO-8859-1): an
/// `<abstract>` with at least one sentence, and `<actions>` and `<decisions>` sections
/// (which may hold only "NA.").
pub fn ami_abssumm_looks_valid(text: &str) -> bool {
    let section = |name: &str| {
        let open = format!("<{name} ");
        let close = format!("</{name}>");
        text.find(&open)
            .and_then(|a| text[a..].find(&close).map(|b| &text[a..a + b]))
    };
    section("abstract").is_some_and(|a| a.contains("<sentence "))
        && section("actions").is_some()
        && section("decisions").is_some()
}

/// `(cache path, url)` of the four headset (IHM) recordings of one summary-evaluation
/// test meeting: Earshot's own final pass runs on them (`cargo xtask sum-eval`, T6.20).
pub fn ami_sum_audio_expected(meeting: &str) -> Vec<(String, String)> {
    (0..4)
        .map(|n| {
            (
                format!("ami/audio/{meeting}.Headset-{n}.wav"),
                format!(
                    "https://groups.inf.ed.ac.uk/ami/AMICorpusMirror/amicorpus/{meeting}/audio/{meeting}.Headset-{n}.wav"
                ),
            )
        })
        .collect()
}

/// Checks that the `ami-sum` set holds exactly the annotations archive (with every
/// [`ami_annotation_members`] pinned) and the four headsets of every test meeting of the
/// summary evaluation, from the official AMI host, CC BY 4.0. Returns the number of
/// artefacts.
pub fn check_ami_sum_manifest(m: &Manifest) -> Result<usize> {
    let set: Vec<&manifest::Artefact> = m
        .artefacts
        .iter()
        .filter(|a| a.sets.iter().any(|s| s == "ami-sum"))
        .collect();
    let annotations = set
        .iter()
        .find(|a| a.path == AMI_ANNOTATIONS_PATH)
        .with_context(|| format!("ami-sum artefact {AMI_ANNOTATIONS_PATH} missing"))?;
    check_ami_annotations(annotations)?;
    let mut want = Vec::new();
    for meeting in ami_sum_meetings("test") {
        want.extend(ami_sum_audio_expected(&meeting));
    }
    ensure!(
        set.len() == want.len() + 1,
        "ami-sum set has {} artefacts, expected {}",
        set.len(),
        want.len() + 1
    );
    for (path, url) in &want {
        let a = set
            .iter()
            .find(|a| &a.path == path)
            .with_context(|| format!("ami-sum artefact {path} missing"))?;
        ensure!(&a.url == url, "ami-sum {path}: url {} != {url}", a.url);
        ensure!(
            a.location == manifest::Location::Cache,
            "ami-sum {path} must be cached"
        );
        ensure!(
            a.licence == "CC-BY-4.0",
            "ami-sum {path}: licence {}",
            a.licence
        );
    }
    Ok(set.len())
}

/// `(cache path, url, licence)` of the four headset (IHM) recordings and the UEM of one AMI
/// dev meeting (T12.15c, set `ami-dev-ihm`), the UEM from `uems/<folder>/` at
/// [`AMI_SETUP_COMMIT`].
pub fn ami_dev_ihm_expected(meeting: &str, folder: &str) -> Vec<(String, String, String)> {
    let mut v: Vec<(String, String, String)> = ami_sum_audio_expected(meeting)
        .into_iter()
        .map(|(p, u)| (p, u, "CC-BY-4.0".to_owned()))
        .collect();
    v.push((
        format!("ami/uem/{meeting}.uem"),
        format!(
            "https://raw.githubusercontent.com/pyannote/AMI-diarization-setup/{AMI_SETUP_COMMIT}/uems/{folder}/{meeting}.uem"
        ),
        "Apache-2.0 AND CC-BY-4.0".to_owned(),
    ));
    v
}

/// Checks that the `ami-dev-ihm` set holds exactly the annotations archive (with every
/// [`ami_annotation_members`] pinned) and the headsets and UEM of every
/// [`AMI_DEV_MEETINGS`] meeting, from the official hosts, with their licences. Returns the
/// number of artefacts.
pub fn check_ami_dev_ihm_manifest(m: &Manifest) -> Result<usize> {
    let set: Vec<&manifest::Artefact> = m
        .artefacts
        .iter()
        .filter(|a| a.sets.iter().any(|s| s == "ami-dev-ihm"))
        .collect();
    let annotations = set
        .iter()
        .find(|a| a.path == AMI_ANNOTATIONS_PATH)
        .with_context(|| format!("ami-dev-ihm artefact {AMI_ANNOTATIONS_PATH} missing"))?;
    check_ami_annotations(annotations)?;
    let want: Vec<(String, String, String)> = AMI_DEV_MEETINGS
        .iter()
        .flat_map(|(meeting, folder)| ami_dev_ihm_expected(meeting, folder))
        .collect();
    ensure!(
        set.len() == want.len() + 1,
        "ami-dev-ihm set has {} artefacts, expected {}",
        set.len(),
        want.len() + 1
    );
    for (path, url, licence) in &want {
        let a = set
            .iter()
            .find(|a| &a.path == path)
            .with_context(|| format!("ami-dev-ihm artefact {path} missing"))?;
        ensure!(&a.url == url, "ami-dev-ihm {path}: url {} != {url}", a.url);
        ensure!(
            a.location == manifest::Location::Cache,
            "ami-dev-ihm {path} must be cached"
        );
        ensure!(
            &a.licence == licence,
            "ami-dev-ihm {path}: licence {}",
            a.licence
        );
    }
    Ok(set.len())
}

/// Checks the fetched dev meeting files: every speaker's oracle segments are on the headset
/// `meetings.xml` names, every named-entity file is one, every UEM is the meeting's and every
/// headset a 16 kHz WAV.
fn check_ami_dev_ihm_files(cache: &Path) -> Result<()> {
    let p = download::jail_join(cache, &format!("{AMI_ANNOTATIONS_DIR}/{AMI_MEETINGS_XML}"))?;
    let xml = read_latin1(&p)?;
    for (meeting, folder) in AMI_DEV_MEETINGS {
        for (channel, spk) in
            ami_headset_speakers(&xml, meeting).with_context(|| p.display().to_string())?
        {
            let s = download::jail_join(
                cache,
                &format!("{AMI_ANNOTATIONS_DIR}/segments/{meeting}.{spk}.segments.xml"),
            )?;
            check_ami_segments(&read_latin1(&s)?, channel)
                .with_context(|| s.display().to_string())?;
            let ne = download::jail_join(
                cache,
                &format!("{AMI_ANNOTATIONS_DIR}/namedEntities/{meeting}.{spk}.ne.xml"),
            )?;
            ensure!(
                read_latin1(&ne)?.contains(&format!("nite:id=\"{meeting}.{spk}.ne\"")),
                "{} is not the named entities of {meeting} speaker {spk}",
                ne.display()
            );
        }
        for (path, _, _) in ami_dev_ihm_expected(meeting, folder) {
            let f = download::jail_join(cache, &path)?;
            if path.ends_with(".uem") {
                check_uem(meeting, &fs::read_to_string(&f)?)?;
            } else {
                check_wav_16k(&f)?;
            }
        }
    }
    Ok(())
}

/// An ISO-8859-1 file decoded byte for byte.
fn read_latin1(p: &Path) -> Result<String> {
    Ok(fs::read(p)
        .with_context(|| format!("read {}", p.display()))?
        .into_iter()
        .map(char::from)
        .collect())
}

/// Checks the extracted summary-evaluation files: every abstractive summary looks valid,
/// every meeting has one speaker per headset 0-3 with a words file, and every test headset
/// is a 16 kHz WAV.
fn check_ami_sum_files(cache: &Path) -> Result<()> {
    let p = download::jail_join(cache, &format!("{AMI_ANNOTATIONS_DIR}/{AMI_MEETINGS_XML}"))?;
    let xml = read_latin1(&p)?;
    for split in ["dev", "test"] {
        for meeting in ami_sum_meetings(split) {
            let a = download::jail_join(
                cache,
                &format!("{AMI_ANNOTATIONS_DIR}/abstractive/{meeting}.abssumm.xml"),
            )?;
            ensure!(
                ami_abssumm_looks_valid(&read_latin1(&a)?),
                "{} is not an AMI abstractive summary",
                a.display()
            );
            for (_, spk) in
                ami_headset_speakers(&xml, &meeting).with_context(|| p.display().to_string())?
            {
                let w = download::jail_join(
                    cache,
                    &format!("{AMI_ANNOTATIONS_DIR}/words/{meeting}.{spk}.words.xml"),
                )?;
                let bytes = fs::read(&w).with_context(|| format!("read {}", w.display()))?;
                ensure!(
                    ami_words_file_looks_valid(&bytes),
                    "{} is not an AMI words file",
                    w.display()
                );
            }
        }
    }
    for meeting in ami_sum_meetings("test") {
        for (path, _) in ami_sum_audio_expected(&meeting) {
            check_wav_16k(&download::jail_join(cache, &path)?)?;
        }
    }
    Ok(())
}

/// A 16 kHz RIFF/WAVE file (header check only).
fn check_wav_16k(p: &Path) -> Result<()> {
    let mut head = [0u8; 28];
    std::io::Read::read_exact(&mut fs::File::open(p)?, &mut head)?;
    let rate = u32::from_le_bytes([head[24], head[25], head[26], head[27]]);
    ensure!(
        &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" && rate == 16_000,
        "{} is not a 16 kHz WAV file",
        p.display()
    );
    Ok(())
}

/// Checks the annotations artefact: official host, CC BY 4.0, extracted as a zip into
/// [`AMI_ANNOTATIONS_DIR`] with exactly the [`ami_annotation_members`] pinned.
pub fn check_ami_annotations(a: &manifest::Artefact) -> Result<()> {
    ensure!(
        a.url == AMI_ANNOTATIONS_URL,
        "ami annotations: url {} != {AMI_ANNOTATIONS_URL}",
        a.url
    );
    ensure!(
        a.location == manifest::Location::Cache,
        "ami annotations must be cached"
    );
    ensure!(
        a.licence == "CC-BY-4.0",
        "ami annotations: licence {}",
        a.licence
    );
    let x = a
        .extract
        .as_ref()
        .context("ami annotations must be extracted")?;
    ensure!(
        x.format == "zip" && x.into == AMI_ANNOTATIONS_DIR,
        "ami annotations: extract {} into {}, expected zip into {AMI_ANNOTATIONS_DIR}",
        x.format,
        x.into
    );
    let mut members: Vec<String> = x.members.iter().map(|m| m.path.clone()).collect();
    members.sort();
    ensure!(
        members == ami_annotation_members(),
        "ami annotations: pinned members {members:?}, expected {:?}",
        ami_annotation_members()
    );
    Ok(())
}

/// Expected `(kind, cache path, url)` of every AMI artefact of one meeting.
pub fn ami_expected(meeting: &str) -> Vec<(String, String, String)> {
    let mut v = Vec::new();
    for c in AMI_CHANNELS {
        v.push((
            format!("audio-{c}"),
            format!("ami/audio/{meeting}.{c}.wav"),
            format!(
                "https://groups.inf.ed.ac.uk/ami/AMICorpusMirror/amicorpus/{meeting}/audio/{meeting}.{c}.wav"
            ),
        ));
    }
    let setup = format!(
        "https://raw.githubusercontent.com/pyannote/AMI-diarization-setup/{AMI_SETUP_COMMIT}"
    );
    v.push((
        "rttm".into(),
        format!("ami/rttm/{meeting}.rttm"),
        format!("{setup}/only_words/rttms/test/{meeting}.rttm"),
    ));
    v.push((
        "uem".into(),
        format!("ami/uem/{meeting}.uem"),
        format!("{setup}/uems/test/{meeting}.uem"),
    ));
    v
}

/// Checks that the `ami` set holds exactly the audio, `only_words` RTTM and UEM of
/// [`AMI_MEETINGS`], from the official AMI host and the pinned setup commit, with licences,
/// plus the manual annotations archive with their words files.
/// Returns the meetings found.
pub fn check_ami_manifest(m: &Manifest) -> Result<Vec<String>> {
    let ami: Vec<&manifest::Artefact> = m
        .artefacts
        .iter()
        .filter(|a| a.sets.iter().any(|s| s == "ami"))
        .collect();
    let mut want = Vec::new();
    for meeting in AMI_MEETINGS {
        want.extend(ami_expected(meeting));
    }
    // Plus the shared annotations archive.
    ensure!(
        ami.len() == want.len() + 1,
        "ami set has {} artefacts, expected {}",
        ami.len(),
        want.len() + 1
    );
    let annotations = ami
        .iter()
        .find(|a| a.path == AMI_ANNOTATIONS_PATH)
        .with_context(|| format!("ami artefact {AMI_ANNOTATIONS_PATH} missing"))?;
    check_ami_annotations(annotations)?;
    for (kind, path, url) in &want {
        let a = ami
            .iter()
            .find(|a| &a.path == path)
            .with_context(|| format!("ami artefact {path} missing"))?;
        ensure!(&a.url == url, "ami {path}: url {} != {url}", a.url);
        ensure!(
            a.location == manifest::Location::Cache,
            "ami {path} must be cached"
        );
        let licence = if kind.starts_with("audio") {
            "CC-BY-4.0"
        } else {
            "Apache-2.0 AND CC-BY-4.0"
        };
        ensure!(a.licence == licence, "ami {path}: licence {}", a.licence);
    }
    Ok(AMI_MEETINGS.iter().map(|s| s.to_string()).collect())
}

/// An `only_words` RTTM must be non-empty and every line a `SPEAKER <meeting> ...` turn.
pub fn check_rttm(meeting: &str, text: &str) -> Result<()> {
    let mut n = 0;
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = l.split_whitespace().collect();
        ensure!(
            f.len() >= 8 && f[0] == "SPEAKER" && f[1] == meeting,
            "RTTM line `{l}` is not a SPEAKER turn of {meeting}"
        );
        let start: f64 = f[3].parse().context("RTTM onset")?;
        let dur: f64 = f[4].parse().context("RTTM duration")?;
        ensure!(start >= 0.0 && dur > 0.0, "RTTM line `{l}` has a bad span");
        n += 1;
    }
    ensure!(n > 0, "RTTM for {meeting} is empty");
    Ok(())
}

/// A UEM must have lines `<meeting> <channel> <start> <end>` with start < end.
pub fn check_uem(meeting: &str, text: &str) -> Result<()> {
    let mut n = 0;
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = l.split_whitespace().collect();
        ensure!(
            f.len() == 4 && f[0] == meeting,
            "UEM line `{l}` is not for {meeting}"
        );
        let a: f64 = f[2].parse().context("UEM start")?;
        let b: f64 = f[3].parse().context("UEM end")?;
        ensure!(a < b, "UEM line `{l}` has start >= end");
        n += 1;
    }
    ensure!(n > 0, "UEM for {meeting} is empty");
    Ok(())
}

/// An AMI words file (ISO-8859-1 bytes): an XML declaration and at least one word.
pub fn ami_words_file_looks_valid(bytes: &[u8]) -> bool {
    bytes.starts_with(b"<?xml") && bytes.windows(3).any(|w| w == b"<w ")
}

/// Value of `name="..."` inside one XML start tag.
fn xml_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let len = tag[at..].find('"')?;
    Some(&tag[at..at + len])
}

/// The `(headset channel, speaker letter)` pairs of `meeting` in the AMI corpus resource
/// [`AMI_MEETINGS_XML`] (`<speaker channel="n" nxt_agent="X" .../>` inside
/// `<meeting ... observation="<meeting>">`), sorted by channel. `asr.wer_ami_ihm` scores
/// `<meeting>.Headset-<n>.wav` against the words of speaker `X` (T2.5b).
///
/// # Errors
/// The meeting is missing, or its speakers are not one per headset 0-3 with the letters
/// [`AMI_SPEAKERS`].
pub fn ami_headset_speakers(xml: &str, meeting: &str) -> Result<Vec<(u8, String)>> {
    let open = format!(" observation=\"{meeting}\"");
    let at = xml
        .find(&open)
        .with_context(|| format!("meeting {meeting} not in {AMI_MEETINGS_XML}"))?;
    let body = &xml[at..];
    let body = &body[..body.find("</meeting>").context("unterminated <meeting>")?];
    let mut speakers = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("<speaker ") {
        rest = &rest[i..];
        let end = rest.find('>').context("unterminated <speaker>")?;
        let tag = &rest[..end];
        rest = &rest[end..];
        let channel: u8 = xml_attr(tag, "channel")
            .with_context(|| format!("speaker without channel: {tag}"))?
            .parse()
            .with_context(|| format!("bad channel in {tag}"))?;
        let agent = xml_attr(tag, "nxt_agent")
            .with_context(|| format!("speaker without nxt_agent: {tag}"))?;
        speakers.push((channel, agent.to_owned()));
    }
    speakers.sort();
    let channels: Vec<u8> = speakers.iter().map(|(c, _)| *c).collect();
    ensure!(
        channels == [0, 1, 2, 3],
        "{meeting}: expected one speaker on each headset 0-3, got channels {channels:?}"
    );
    let mut letters: Vec<&str> = speakers.iter().map(|(_, a)| a.as_str()).collect();
    letters.sort_unstable();
    ensure!(
        letters == AMI_SPEAKERS,
        "{meeting}: expected speakers {AMI_SPEAKERS:?}, got {letters:?}"
    );
    Ok(speakers)
}

/// Parses one AMI segments file (`segments/<meeting>.<speaker>.segments.xml`, the oracle
/// segments `asr.wer_ami_ihm` decodes, T2.5b) into its headset channel and
/// `(transcriber_start, transcriber_end)` spans sorted by start. Same rule as the asr_eval
/// example's `refs::parse_ami_segments` (shared cases in `fixtures/ami/parse_cases.toml`).
///
/// # Errors
/// No `<segment>`, a missing or unparseable channel or time, an empty or negative span,
/// or segments on more than one channel.
pub fn ami_segments(xml: &str) -> Result<(u8, Vec<(f64, f64)>)> {
    let mut channel = None;
    let mut spans = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<segment ") {
        rest = &rest[i..];
        let end = rest.find('>').context("unterminated <segment>")?;
        let tag = &rest[..end];
        rest = &rest[end..];
        let c: u8 = xml_attr(tag, "channel")
            .with_context(|| format!("segment without channel: {tag}"))?
            .parse()
            .with_context(|| format!("bad channel in {tag}"))?;
        let first = *channel.get_or_insert(c);
        ensure!(c == first, "segments on channels {first} and {c}");
        let time = |name: &str| -> Result<f64> {
            xml_attr(tag, name)
                .with_context(|| format!("segment without {name}: {tag}"))?
                .parse()
                .with_context(|| format!("bad {name} in {tag}"))
        };
        let (a, b) = (time("transcriber_start")?, time("transcriber_end")?);
        ensure!(
            a >= 0.0 && a < b,
            "segment with an empty or negative span: {tag}"
        );
        spans.push((a, b));
    }
    let channel = channel.context("no <segment> in segments file")?;
    spans.sort_by(|x, y| x.0.total_cmp(&y.0));
    Ok((channel, spans))
}

/// Checks one AMI segments file against the speaker's headset `channel` from
/// [`AMI_MEETINGS_XML`]: it parses ([`ami_segments`]) and every segment is on `channel`.
/// Returns the number of segments.
pub fn check_ami_segments(xml: &str, channel: u8) -> Result<usize> {
    let (c, spans) = ami_segments(xml)?;
    ensure!(
        c == channel,
        "segments on headset {c}, but meetings.xml puts this speaker on {channel}"
    );
    Ok(spans.len())
}

fn check_ami_files(cache: &Path) -> Result<()> {
    let p = download::jail_join(cache, &format!("{AMI_ANNOTATIONS_DIR}/{AMI_MEETINGS_XML}"))?;
    // Declared ISO-8859-1 like the words files: decode bytes, not UTF-8.
    let xml: String = fs::read(&p)
        .with_context(|| format!("read {}", p.display()))?
        .into_iter()
        .map(char::from)
        .collect();
    for meeting in AMI_MEETINGS {
        let speakers =
            ami_headset_speakers(&xml, meeting).with_context(|| p.display().to_string())?;
        for (channel, spk) in speakers {
            let s = download::jail_join(
                cache,
                &format!("{AMI_ANNOTATIONS_DIR}/segments/{meeting}.{spk}.segments.xml"),
            )?;
            let text: String = fs::read(&s)
                .with_context(|| format!("read {}", s.display()))?
                .into_iter()
                .map(char::from)
                .collect();
            check_ami_segments(&text, channel).with_context(|| s.display().to_string())?;
        }
    }
    for member in ami_annotation_members()
        .into_iter()
        .filter(|m| m.starts_with("words/"))
    {
        let p = download::jail_join(cache, &format!("{AMI_ANNOTATIONS_DIR}/{member}"))?;
        // The words files are ISO-8859-1, so read bytes (not UTF-8 text).
        let bytes = fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        ensure!(
            ami_words_file_looks_valid(&bytes),
            "{} is not an AMI words file",
            p.display()
        );
    }
    for meeting in AMI_MEETINGS {
        for (kind, path, _) in ami_expected(meeting) {
            let p = download::jail_join(cache, &path)?;
            match kind.as_str() {
                "rttm" => check_rttm(meeting, &fs::read_to_string(&p)?)?,
                "uem" => check_uem(meeting, &fs::read_to_string(&p)?)?,
                _ => check_wav_16k(&p)?,
            }
        }
    }
    Ok(())
}

/// `cargo xtask fetch-fixtures`.
pub fn run(repo: &Path, sets: &[String], verify: bool, pin_lists: bool) -> Result<()> {
    let mp = manifest_path(repo);
    let m = Manifest::load(&mp)?;
    let cache = cache_dir()?;
    let wants = |s: &str| sets.iter().any(|x| x == s || x == "all");
    if pin_lists {
        ensure!(wants("librispeech"), "--pin needs the librispeech set");
    }
    // Committed smoke files are checked after pinning, so fetch the cache sets first.
    let cache_sets: Vec<&manifest::Artefact> = m
        .select(sets)?
        .into_iter()
        .filter(|a| a.location == manifest::Location::Cache)
        .collect();
    let mut reports = manifest::fetch_all(&cache_sets, repo, &cache, verify, &m.policy())?;
    if pin_lists {
        pin(repo, &cache)?;
    }
    let m = Manifest::load(&mp)?;
    let repo_sets: Vec<&manifest::Artefact> = m
        .select(sets)?
        .into_iter()
        .filter(|a| a.location == manifest::Location::Repo)
        .collect();
    reports.extend(manifest::fetch_all(
        &repo_sets,
        repo,
        &cache,
        true,
        &m.policy(),
    )?);
    if wants("smoke") {
        check_smoke(repo, &m)?;
    }
    if wants("ami") {
        check_ami_manifest(&m)?;
        check_ami_files(&cache)?;
    }
    if wants("ami-sum") {
        check_ami_sum_manifest(&m)?;
        check_ami_sum_files(&cache)?;
    }
    if wants("ami-dev-ihm") {
        check_ami_dev_ihm_manifest(&m)?;
        check_ami_dev_ihm_files(&cache)?;
    }
    if wants("librispeech") {
        if verify {
            verify_librispeech(repo, &cache, &m)?;
        } else {
            for p in load_subset(repo)? {
                let f = utterance_path(&cache, &p.split, &p.id)?;
                ensure!(f.exists(), "missing {}", f.display());
            }
            verified_aec_heldout(repo, &cache)?;
            verified_aec_dev(repo, &cache)?;
        }
    }
    manifest::print_reports("fetch-fixtures", &cache, &reports);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(split: &str, speaker: u32, chapter: u32, index: u32, seconds: f64) -> Utterance {
        Utterance {
            split: split.into(),
            speaker,
            chapter,
            index,
            seconds,
        }
    }

    /// A synthetic split where every speaker has 4 s utterances.
    fn synthetic(split: &str, speakers: std::ops::Range<u32>, per: u32) -> Vec<Utterance> {
        let mut v = Vec::new();
        for s in speakers {
            for i in 0..per {
                v.push(u(split, s, 100 + s, i, 4.0));
            }
        }
        v
    }

    #[test]
    fn fixtures_rule_takes_enrolment_until_ten_seconds_then_three_long_tests() {
        let c = vec![
            u("test-clean", 7, 1, 0, 6.0),
            u("test-clean", 7, 1, 1, 2.5),
            u("test-clean", 7, 1, 2, 2.0),
            u("test-clean", 7, 1, 3, 2.9),
            u("test-clean", 7, 1, 4, 3.0),
            u("test-clean", 7, 2, 0, 9.0),
            u("test-clean", 7, 10, 0, 3.5),
            u("test-clean", 7, 10, 1, 8.0),
        ];
        let p = select_split(&c, "test-clean").unwrap();
        let enrol: Vec<&str> = p
            .iter()
            .filter(|p| p.role == Role::Enrol)
            .map(|p| p.id.as_str())
            .collect();
        let test: Vec<&str> = p
            .iter()
            .filter(|p| p.role == Role::Test)
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(enrol, ["7-1-0000", "7-1-0001", "7-1-0002"]);
        // 7-1-0003 is too short for a test clip; chapter 10 sorts after chapter 2 numerically.
        assert_eq!(test, ["7-1-0004", "7-2-0000", "7-10-0000"]);
    }

    #[test]
    fn fixtures_rule_is_independent_of_input_order() {
        let mut c = synthetic("test-clean", 1..5, 8);
        let a = select_split(&c, "test-clean").unwrap();
        c.reverse();
        assert_eq!(select_split(&c, "test-clean").unwrap(), a);
    }

    #[test]
    fn fixtures_rule_fails_for_a_speaker_without_enough_audio() {
        let mut c = synthetic("test-clean", 1..3, 8);
        c.push(u("test-clean", 9, 1, 0, 4.0));
        c.push(u("test-clean", 9, 1, 1, 4.0));
        assert!(select_split(&c, "test-clean").is_err());
    }

    fn full_synthetic_subset() -> Vec<Pick> {
        let mut c = synthetic("test-clean", 1..41, 8);
        c.extend(synthetic("dev-clean", 100..120, 8));
        let mut p = select_split(&c, "test-clean").unwrap();
        p.extend(select_split(&c, "dev-clean").unwrap());
        p
    }

    #[test]
    fn fixtures_check_accepts_a_rule_built_subset() {
        check_subset(&full_synthetic_subset()).unwrap();
    }

    #[test]
    fn fixtures_check_rejects_constraint_violations() {
        let base = full_synthetic_subset();
        // Drop one test-clean speaker -> 39 speakers.
        let fewer: Vec<Pick> = base.iter().filter(|p| p.speaker != 1).cloned().collect();
        assert!(check_subset(&fewer).is_err());
        // Short enrolment.
        let mut short = base.clone();
        let i = short.iter().position(|p| p.role == Role::Enrol).unwrap();
        short.remove(i);
        assert!(check_subset(&short).is_err());
        // Only two test utterances for one speaker.
        let mut two = base.clone();
        let i = two.iter().position(|p| p.role == Role::Test).unwrap();
        two.remove(i);
        assert!(check_subset(&two).is_err());
        // A test clip below 3 s.
        let mut clip = base.clone();
        let i = clip.iter().position(|p| p.role == Role::Test).unwrap();
        clip[i].seconds = 2.9;
        assert!(check_subset(&clip).is_err());
        // Duplicate utterance.
        let mut dup = base.clone();
        dup.push(dup[0].clone());
        assert!(check_subset(&dup).is_err());
        // Utterance id not of its speaker.
        let mut wrong = base.clone();
        wrong[0].id = "999-1-0000".into();
        assert!(check_subset(&wrong).is_err());
        // Too few dev speakers.
        let few_dev: Vec<Pick> = base
            .iter()
            .filter(|p| p.split == "test-clean" || p.speaker < 105)
            .cloned()
            .collect();
        assert!(check_subset(&few_dev).is_err());
    }

    #[test]
    fn fixtures_smoke_rule_picks_first_female_and_male() {
        let mut c = Vec::new();
        for s in [5u32, 3, 8] {
            for i in 0..25 {
                c.push(u("test-clean", s, 1, i, if i % 2 == 0 { 3.0 } else { 7.0 }));
            }
        }
        let g = BTreeMap::from([(3, 'M'), (5, 'F'), (8, 'F')]);
        let s = select_smoke(&c, &g).unwrap();
        assert_eq!(s.len(), SMOKE_UTTERANCES);
        assert!(s[..10].iter().all(|u| u.speaker == 5));
        assert!(s[10..].iter().all(|u| u.speaker == 3));
        assert!(s.iter().all(|u| u.seconds == 3.0));
    }

    #[test]
    fn fixtures_aec_heldout_rule_picks_the_next_female_and_male() {
        let mut c = Vec::new();
        for s in [5u32, 3, 8, 9, 2] {
            for i in 0..25 {
                c.push(u("test-clean", s, 1, i, if i % 2 == 0 { 3.0 } else { 7.0 }));
            }
        }
        let g = BTreeMap::from([(2, 'F'), (3, 'M'), (5, 'F'), (8, 'F'), (9, 'M')]);
        let smoke = select_smoke(&c, &g).unwrap();
        let held = select_aec_heldout(&c, &g).unwrap();
        assert_eq!(held.len(), SMOKE_UTTERANCES);
        // Smoke: F 2, M 3; held-out: the next of each, F 5 and M 9.
        assert!(smoke[..10].iter().all(|u| u.speaker == 2));
        assert!(smoke[10..].iter().all(|u| u.speaker == 3));
        assert!(held[..10].iter().all(|u| u.speaker == 5));
        assert!(held[10..].iter().all(|u| u.speaker == 9));
        assert!(held.iter().all(|u| u.seconds == 3.0));
        // No second male: the rule fails rather than reusing a smoke voice.
        let g1 = BTreeMap::from([(2, 'F'), (3, 'M'), (5, 'F')]);
        assert!(select_aec_heldout(&c, &g1).is_err());
    }

    fn heldout_pick(id: &str, end: CallEnd) -> HeldoutPick {
        HeldoutPick {
            id: id.into(),
            speaker: id.split('-').next().unwrap().parse().unwrap(),
            end,
            seconds: 3.0,
            size: 1,
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn fixtures_aec_heldout_check_rejects_shared_or_malformed_speech() {
        let good: Vec<HeldoutPick> = (0..10)
            .map(|i| heldout_pick(&format!("9-1-{i:04}"), CallEnd::Far))
            .chain((0..10).map(|i| heldout_pick(&format!("5-1-{i:04}"), CallEnd::Near)))
            .collect();
        let dev = BTreeSet::from([2u32, 3]);
        check_aec_heldout(&good, &dev).unwrap();
        // A dev (smoke) speaker is never held out.
        assert!(check_aec_heldout(&good, &BTreeSet::from([5u32])).is_err());
        let mut short = good.clone();
        short.pop();
        assert!(check_aec_heldout(&short, &dev).is_err());
        let mut long = good.clone();
        long[0].seconds = 5.5;
        assert!(check_aec_heldout(&long, &dev).is_err());
        let mut dup = good.clone();
        dup[1].id = dup[0].id.clone();
        assert!(check_aec_heldout(&dup, &dev).is_err());
        let mut wrong = good.clone();
        wrong[0].speaker = 7;
        assert!(check_aec_heldout(&wrong, &dev).is_err());
        let same: Vec<HeldoutPick> = (0..10)
            .map(|i| heldout_pick(&format!("9-1-{i:04}"), CallEnd::Far))
            .chain((10..20).map(|i| heldout_pick(&format!("9-1-{i:04}"), CallEnd::Near)))
            .collect();
        assert!(check_aec_heldout(&same, &dev).is_err());
    }

    #[test]
    fn fixtures_aec_dev_rule_fails_without_enough_dev_clean_speakers() {
        let mut c = Vec::new();
        for s in [2u32, 3, 5, 8, 9, 10, 11] {
            for i in 0..25 {
                c.push(u("dev-clean", s, 1, i, 3.0));
            }
        }
        // A fourth male exists only in test-clean: the rule never borrows it.
        for i in 0..25 {
            c.push(u("test-clean", 13, 1, i, 3.0));
        }
        let g = BTreeMap::from([
            (2, 'F'),
            (5, 'F'),
            (8, 'F'),
            (10, 'F'),
            (3, 'M'),
            (9, 'M'),
            (11, 'M'),
            (13, 'M'),
        ]);
        assert!(select_aec_dev(&c, &g).is_err());
    }

    #[test]
    fn fixtures_aec_dev_rule_picks_four_ranks_per_gender() {
        let mut c = Vec::new();
        let f = [2u32, 5, 8, 10];
        let m = [3u32, 9, 11, 13];
        for s in f.iter().chain(&m) {
            for i in 0..25 {
                c.push(u("dev-clean", *s, 1, i, if i % 2 == 0 { 3.0 } else { 7.0 }));
            }
        }
        // Speaker 4 (F) has only 9 short utterances and is skipped, not picked short.
        for i in 0..25 {
            c.push(u("dev-clean", 4, 1, i, if i < 9 { 3.0 } else { 7.0 }));
        }
        let mut g: BTreeMap<u32, char> = f
            .iter()
            .map(|&s| (s, 'F'))
            .chain(m.iter().map(|&s| (s, 'M')))
            .collect();
        g.insert(4, 'F');
        let (pairs, utts) = select_aec_dev(&c, &g).unwrap();
        let want: Vec<DevPair> = (0..AEC_DEV_RANKS)
            .flat_map(|r| {
                [
                    DevPair {
                        far: m[r],
                        near: f[r],
                    },
                    DevPair {
                        far: f[r],
                        near: m[r],
                    },
                ]
            })
            .collect();
        assert_eq!(pairs, want);
        assert_eq!(utts.len(), 8 * SMOKE_UTTERANCES / SMOKE_SPEAKERS);
        assert!(
            utts.iter()
                .all(|u| u.split == "dev-clean" && u.seconds == 3.0)
        );
    }

    fn dev_pick(id: &str) -> DevPick {
        DevPick {
            id: id.into(),
            speaker: id.split('-').next().unwrap().parse().unwrap(),
            seconds: 3.0,
            size: 1,
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn fixtures_aec_dev_check_rejects_shared_or_malformed_speech() {
        let good = AecDevFile {
            pairs: vec![DevPair { far: 9, near: 5 }, DevPair { far: 5, near: 9 }],
            utterances: (0..10)
                .map(|i| dev_pick(&format!("9-1-{i:04}")))
                .chain((0..10).map(|i| dev_pick(&format!("5-1-{i:04}"))))
                .collect(),
        };
        let excluded = BTreeSet::from([2u32, 3]);
        check_aec_dev(&good, &excluded).unwrap();
        // A smoke or held-out speaker is never a dev-wide voice.
        assert!(check_aec_dev(&good, &BTreeSet::from([5u32])).is_err());
        let mut short = good.clone();
        short.utterances.pop();
        assert!(check_aec_dev(&short, &excluded).is_err());
        let mut long = good.clone();
        long.utterances[0].seconds = 5.5;
        assert!(check_aec_dev(&long, &excluded).is_err());
        let mut dup = good.clone();
        dup.utterances[1].id = dup.utterances[0].id.clone();
        assert!(check_aec_dev(&dup, &excluded).is_err());
        let mut wrong = good.clone();
        wrong.utterances[0].speaker = 7;
        assert!(check_aec_dev(&wrong, &excluded).is_err());
        let mut stray = good.clone();
        stray.utterances.push(dev_pick("7-1-0000"));
        assert!(check_aec_dev(&stray, &excluded).is_err());
        let mut same = good.clone();
        same.pairs[0] = DevPair { far: 9, near: 9 };
        assert!(check_aec_dev(&same, &excluded).is_err());
        let mut twice = good.clone();
        twice.pairs[1] = twice.pairs[0];
        assert!(check_aec_dev(&twice, &excluded).is_err());
        let mut none = good.clone();
        none.pairs.clear();
        assert!(check_aec_dev(&none, &excluded).is_err());
    }

    #[test]
    fn fixtures_committed_aec_dev_speech_avoids_the_smoke_and_held_out_voices() {
        let repo = crate::repo_root();
        let m = Manifest::load(&manifest_path(&repo)).unwrap();
        let excluded = aec_dev_excluded(&repo, &m).unwrap();
        assert_eq!(excluded, BTreeSet::from([61u32, 121, 237, 260]));
        let dev = load_aec_dev(&repo).unwrap();
        check_aec_dev(&dev, &excluded).unwrap();
        assert_eq!(dev.pairs.len(), 2 * AEC_DEV_RANKS);
    }

    #[test]
    fn fixtures_committed_aec_heldout_speech_is_disjoint_from_the_smoke_set() {
        let repo = crate::repo_root();
        let m = Manifest::load(&manifest_path(&repo)).unwrap();
        let dev = smoke_speakers(&m);
        assert_eq!(dev, BTreeSet::from([61u32, 121]));
        let held = load_aec_heldout(&repo).unwrap();
        check_aec_heldout(&held, &dev).unwrap();
        let smoke_ids: BTreeSet<String> = smoke_artefacts(&m)
            .0
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(held.iter().all(|p| !smoke_ids.contains(&p.id)));
    }

    #[test]
    fn fixtures_speakers_txt_parsing() {
        let t = ";ID  |SEX| SUBSET |MINUTES| NAME\n14   | F | train-clean-360 | 25.03 | Kristin\n1089 | M | test-clean | 8.0 | Someone\n";
        let g = read_genders(t);
        assert_eq!(g.get(&14), Some(&'F'));
        assert_eq!(g.get(&1089), Some(&'M'));
        assert_eq!(g.len(), 2);
    }

    // Committed metadata (no download needed).

    #[test]
    fn fixtures_manifest_pins_two_ami_meetings() {
        let repo = crate::repo_root();
        let m = Manifest::load(&manifest_path(&repo)).unwrap();
        let meetings = check_ami_manifest(&m).unwrap();
        assert_eq!(meetings, AMI_MEETINGS);
        let licences = fs::read_to_string(repo.join("fixtures").join("LICENSES.md")).unwrap();
        assert!(
            licences.contains("AMI Meeting Corpus") && licences.contains("AMI-diarization-setup")
        );
    }

    #[test]
    fn fixtures_ami_sum_manifest_pins_the_scenario_partition() {
        let m = Manifest::load(&manifest_path(&crate::repo_root())).unwrap();
        // The annotations archive plus four headsets of each of the 20 test meetings.
        assert_eq!(check_ami_sum_manifest(&m).unwrap(), 1 + 20 * 4);
        let dev = ami_sum_meetings("dev");
        let test = ami_sum_meetings("test");
        assert_eq!((dev.len(), test.len()), (20, 20));
        assert!(
            dev.iter().all(|d| !test.contains(d)),
            "dev and test overlap"
        );
        for m in AMI_MEETINGS {
            assert!(test.iter().any(|t| t == m), "{m} is an AMI test meeting");
        }
        let members = ami_annotation_members();
        for m in dev.iter().chain(&test) {
            assert!(
                members.contains(&format!("abstractive/{m}.abssumm.xml")),
                "{m}"
            );
            for spk in AMI_SPEAKERS {
                assert!(
                    members.contains(&format!("words/{m}.{spk}.words.xml")),
                    "{m}"
                );
            }
        }
        // Segments only for the two m2 meetings and the five dev meetings of T12.15c (summaries
        // use words; some summary meetings' segments have negative spans, ES2011c.C); named
        // entities only for the dev meetings; plus meetings.xml and the entity types.
        for m in AMI_MEETINGS {
            for spk in AMI_SPEAKERS {
                assert!(members.contains(&format!("segments/{m}.{spk}.segments.xml")));
            }
        }
        assert!(!members.iter().any(|m| m.starts_with("segments/ES2011c")));
        assert_eq!(members.len(), 2 + 40 * 5 + 2 * 4 + 5 * 4 * 2);
    }

    #[test]
    fn fixtures_ami_dev_ihm_manifest_pins_the_dev_headsets_segments_and_entities() {
        let m = Manifest::load(&manifest_path(&crate::repo_root())).unwrap();
        // The annotations archive plus four headsets and a UEM for each of five meetings.
        assert_eq!(check_ami_dev_ihm_manifest(&m).unwrap(), 1 + 5 * 5);
        let members = ami_annotation_members();
        assert!(members.contains(&AMI_NE_TYPES.to_owned()));
        let dev = ami_sum_meetings("dev");
        let test = ami_sum_meetings("test");
        for (meeting, _) in AMI_DEV_MEETINGS {
            // Dev data: never a test meeting.
            assert!(dev.iter().any(|d| d == meeting), "{meeting}");
            assert!(!test.iter().any(|t| t == meeting), "{meeting}");
            for spk in AMI_SPEAKERS {
                for member in [
                    format!("segments/{meeting}.{spk}.segments.xml"),
                    format!("namedEntities/{meeting}.{spk}.ne.xml"),
                    format!("words/{meeting}.{spk}.words.xml"),
                ] {
                    assert!(members.contains(&member), "{member}");
                }
            }
        }
        // The dev meetings of set ami-dev, whose mixes are pinned there.
        for (meeting, _) in AMI_DEV_MEETINGS {
            assert!(m.artefacts.iter().any(|a| {
                a.sets.iter().any(|s| s == "ami-dev")
                    && a.path == format!("ami/audio/{meeting}.Mix-Headset.wav")
            }));
        }
    }

    #[test]
    fn fixtures_ami_dev_ihm_manifest_check_rejects_a_missing_or_moved_artefact() {
        let text = fs::read_to_string(manifest_path(&crate::repo_root())).unwrap();
        let mut missing = Manifest::parse(&text).unwrap();
        let at = missing
            .artefacts
            .iter()
            .position(|a| a.path == "ami/audio/TS3006a.Headset-2.wav")
            .unwrap();
        missing.artefacts.remove(at);
        assert!(check_ami_dev_ihm_manifest(&missing).is_err());
        let mut moved = Manifest::parse(&text).unwrap();
        let a = moved
            .artefacts
            .iter_mut()
            .find(|a| a.path == "ami/uem/ES2003a.uem")
            .unwrap();
        a.url = a.url.replace("/uems/train/", "/uems/dev/");
        assert!(check_ami_dev_ihm_manifest(&moved).is_err());
    }

    #[test]
    fn fixtures_ami_sum_manifest_check_rejects_a_missing_or_moved_headset() {
        let text = fs::read_to_string(manifest_path(&crate::repo_root())).unwrap();
        let mut missing = Manifest::parse(&text).unwrap();
        let at = missing
            .artefacts
            .iter()
            .position(|a| a.path == "ami/audio/TS3007d.Headset-3.wav")
            .unwrap();
        missing.artefacts.remove(at);
        assert!(check_ami_sum_manifest(&missing).is_err());
        let mut moved = Manifest::parse(&text).unwrap();
        let a = moved
            .artefacts
            .iter_mut()
            .find(|a| a.path == "ami/audio/TS3003b.Headset-1.wav")
            .unwrap();
        a.url = a.url.replace("TS3003b.Headset-1", "TS3003b.Headset-2");
        assert!(check_ami_sum_manifest(&moved).is_err());
    }

    #[test]
    fn fixtures_ami_abssumm_needs_an_abstract_and_both_item_sections() {
        let ok = r#"<?xml version="1.0" encoding="ISO-8859-1"?><nite:root>
<abstract nite:id="x.abstract.1"><sentence nite:id="x.s.1">They met.</sentence></abstract>
<actions nite:id="x.actions.1"><sentence nite:id="x.s.2">NA.</sentence></actions>
<decisions nite:id="x.decisions.1"><sentence nite:id="x.s.3">A remote.</sentence></decisions>
</nite:root>"#;
        assert!(ami_abssumm_looks_valid(ok));
        assert!(!ami_abssumm_looks_valid(
            &ok.replace("<actions ", "<other ")
        ));
        assert!(!ami_abssumm_looks_valid(
            &ok.replace("decisions", "problems")
        ));
        let empty = ok.replace(r#"<sentence nite:id="x.s.1">They met.</sentence>"#, "");
        assert!(!ami_abssumm_looks_valid(&empty));
    }

    fn ami_text(rttm_line: &str) -> String {
        let mut s = String::from(
            "sets = [\"ami\"]\nhosts = [\"groups.inf.ed.ac.uk\", \"raw.githubusercontent.com\"]\n",
        );
        for m in AMI_MEETINGS {
            for (kind, path, url) in ami_expected(m) {
                let url = if kind == "rttm" {
                    url.replace(AMI_SETUP_COMMIT, rttm_line)
                } else {
                    url
                };
                let licence = if kind.starts_with("audio") {
                    "CC-BY-4.0"
                } else {
                    "Apache-2.0 AND CC-BY-4.0"
                };
                let _ = write!(
                    s,
                    "\n[[artefact]]\nid = \"ami-{m}-{kind}\"\nsets = [\"ami\"]\nlocation = \"cache\"\nurl = \"{url}\"\npath = \"{path}\"\nsha256 = \"{}\"\nsize = 1\nlicence = \"{licence}\"\nattribution = \"x\"\n",
                    "0".repeat(64)
                );
            }
        }
        let zero = "0".repeat(64);
        let _ = write!(
            s,
            r#"
[[artefact]]
id = "ami-manual-annotations"
sets = ["ami"]
location = "cache"
url = "{AMI_ANNOTATIONS_URL}"
path = "{AMI_ANNOTATIONS_PATH}"
sha256 = "{zero}"
size = 1
licence = "CC-BY-4.0"
attribution = "x"

[artefact.extract]
format = "zip"
into = "{AMI_ANNOTATIONS_DIR}"
"#
        );
        for member in ami_annotation_members() {
            let _ = write!(
                s,
                r#"
[[artefact.extract.member]]
path = "{member}"
sha256 = "{zero}"
size = 1
"#
            );
        }
        s
    }

    #[test]
    fn fixtures_ami_manifest_check_rejects_unpinned_references() {
        let ok = Manifest::parse(&ami_text(AMI_SETUP_COMMIT)).unwrap();
        check_ami_manifest(&ok).unwrap();
        // RTTM taken from a moving branch instead of the pinned commit.
        let moving = Manifest::parse(&ami_text("main")).unwrap();
        assert!(check_ami_manifest(&moving).is_err());
        // One artefact missing.
        let mut missing = Manifest::parse(&ami_text(AMI_SETUP_COMMIT)).unwrap();
        missing.artefacts.remove(0);
        assert!(check_ami_manifest(&missing).is_err());
        // The annotations archive missing, or one words file not pinned.
        let mut no_words = Manifest::parse(&ami_text(AMI_SETUP_COMMIT)).unwrap();
        no_words.artefacts.pop();
        assert!(check_ami_manifest(&no_words).is_err());
        let mut unpinned = Manifest::parse(&ami_text(AMI_SETUP_COMMIT)).unwrap();
        let last = unpinned.artefacts.last_mut().unwrap();
        last.extract.as_mut().unwrap().members.pop();
        assert!(check_ami_manifest(&unpinned).is_err());
    }

    #[test]
    fn fixtures_ami_words_files_are_checked_as_bytes() {
        // ISO-8859-1: 0xE9 is not valid UTF-8, so a text read would reject the file.
        let latin1: &[u8] =
            b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n<w>caf\xe9</w>\n<w a=\"1\">x</w>";
        assert!(ami_words_file_looks_valid(latin1));
        assert!(!ami_words_file_looks_valid(b"<?xml?><gap/>"));
        assert!(!ami_words_file_looks_valid(b"<w a=1>x</w>"));
    }

    #[test]
    fn fixtures_ami_meetings_xml_maps_each_headset_to_one_speaker() {
        // Lines as in the official corpusResources/meetings.xml (v1.6.2), speakers unsorted.
        let xml = r#"<meetings>
   <meeting nite:id="meet_37" type="scenario" observation="IS1009a" duration="831">
      <speaker nite:id="IS1009a_4" channel="3" nxt_agent="D" camera="Closeup4" global_name="FIO089" role="ME"/>
      <speaker nite:id="IS1009a_2" channel="1" nxt_agent="B" camera="Closeup2" global_name="FIO087" role="UI"/>
      <speaker nite:id="IS1009a_1" channel="0" nxt_agent="A" camera="Closeup1" global_name="FIE088" role="PM"/>
      <speaker nite:id="IS1009a_3" channel="2" nxt_agent="C" camera="Closeup3" global_name="FIO084" role="ID"/>
   </meeting>
   <meeting nite:id="meet_38" type="scenario" observation="IS1009b" duration="2051">
      <speaker nite:id="IS1009b_4" channel="3" nxt_agent="D"/>
   </meeting>
</meetings>"#;
        let got = ami_headset_speakers(xml, "IS1009a").unwrap();
        assert_eq!(
            got,
            [
                (0, "A".to_owned()),
                (1, "B".to_owned()),
                (2, "C".to_owned()),
                (3, "D".to_owned())
            ]
        );
        // The next meeting's speakers never leak in; a missing or incomplete meeting fails.
        assert!(ami_headset_speakers(xml, "IS1009b").is_err());
        assert!(ami_headset_speakers(xml, "ES2004a").is_err());
        // Two speakers on one headset, or a letter outside A-D, fails.
        let dup = xml.replace("channel=\"2\"", "channel=\"1\"");
        assert!(ami_headset_speakers(&dup, "IS1009a").is_err());
        let letter = xml.replace("nxt_agent=\"C\"", "nxt_agent=\"E\"");
        assert!(ami_headset_speakers(&letter, "IS1009a").is_err());
        // The mapping is read, not assumed: a permuted table is returned as written.
        let permuted = xml
            .replace(
                "channel=\"0\" nxt_agent=\"A\"",
                "channel=\"0\" nxt_agent=\"X\"",
            )
            .replace(
                "channel=\"3\" nxt_agent=\"D\"",
                "channel=\"3\" nxt_agent=\"A\"",
            )
            .replace("nxt_agent=\"X\"", "nxt_agent=\"D\"");
        let got = ami_headset_speakers(&permuted, "IS1009a").unwrap();
        assert_eq!(got[0], (0, "D".to_owned()));
        assert_eq!(got[3], (3, "A".to_owned()));
    }

    /// `fixtures/ami/parse_cases.toml`, also run by the asr_eval example's tests.
    #[derive(Deserialize)]
    struct AmiParseCases {
        meetings: Vec<MeetingsCase>,
        segments: Vec<SegmentsCase>,
    }

    #[derive(Deserialize)]
    struct MeetingsCase {
        name: String,
        meeting: String,
        xml: String,
        #[serde(default)]
        error: bool,
        #[serde(default)]
        headsets: Vec<u8>,
        #[serde(default)]
        speakers: Vec<String>,
    }

    #[derive(Deserialize)]
    struct SegmentsCase {
        name: String,
        xml: String,
        #[serde(default)]
        error: bool,
        channel: Option<u8>,
        #[serde(default)]
        spans: Vec<(f64, f64)>,
    }

    #[test]
    fn fixtures_ami_parse_cases_shared_with_asr_eval() {
        let path = crate::repo_root().join("fixtures/ami/parse_cases.toml");
        let cases: AmiParseCases = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(cases.meetings.len() >= 2 && cases.segments.len() >= 2);
        for c in &cases.meetings {
            let got = ami_headset_speakers(&c.xml, &c.meeting);
            if c.error {
                assert!(got.is_err(), "{}: {got:?}", c.name);
            } else {
                let want: Vec<(u8, String)> = c
                    .headsets
                    .iter()
                    .copied()
                    .zip(c.speakers.iter().cloned())
                    .collect();
                assert_eq!(got.unwrap(), want, "{}", c.name);
            }
        }
        for c in &cases.segments {
            let got = ami_segments(&c.xml);
            if c.error {
                assert!(got.is_err(), "{}: {got:?}", c.name);
            } else {
                let want = (c.channel.unwrap(), c.spans.clone());
                assert_eq!(got.unwrap(), want, "{}", c.name);
            }
        }
    }

    #[test]
    fn fixtures_ami_segments_are_on_their_speakers_headset() {
        // Lines as in the official segments/IS1009a.B.segments.xml (v1.6.2).
        let xml = r#"<?xml version="1.0" encoding="ISO-8859-1" standalone="yes"?>
<nite:root nite:id="IS1009a.B.segs" xmlns:nite="http://nite.sourceforge.net/">
   <segment nite:id="IS1009a.sync.154" channel="1" transcriber_start="66.608" transcriber_end="80.88">
      <nite:child href="IS1009a.B.words.xml#id(IS1009a.B.words0)..id(IS1009a.B.words39)"/>
   </segment>
   <segment nite:id="IS1009a.sync.156" channel="1" transcriber_start="283.776" transcriber_end="285.12">
      <nite:child href="IS1009a.B.words.xml#id(IS1009a.B.words40)"/>
   </segment>
</nite:root>"#;
        assert_eq!(check_ami_segments(xml, 1).unwrap(), 2);
        // Another headset than meetings.xml gives, a bad span, or no segments: rejected.
        assert!(check_ami_segments(xml, 2).is_err());
        let mixed = xml.replacen("channel=\"1\"", "channel=\"0\"", 1);
        assert!(check_ami_segments(&mixed, 1).is_err());
        let bad = xml.replacen("transcriber_end=\"80.88\"", "transcriber_end=\"60\"", 1);
        assert!(check_ami_segments(&bad, 1).is_err());
        assert!(check_ami_segments("<nite:root/>", 1).is_err());
    }

    #[test]
    fn fixtures_ami_reference_files_are_checked_against_their_meeting() {
        check_rttm(
            "IS1009a",
            "SPEAKER IS1009a 1 54.95 5.9 <NA> <NA> FIE088 <NA> <NA>\n",
        )
        .unwrap();
        assert!(
            check_rttm(
                "IS1009a",
                "SPEAKER ES2004a 1 0.37 1.39 <NA> <NA> MEO015 <NA> <NA>\n"
            )
            .is_err()
        );
        assert!(check_rttm("IS1009a", "").is_err());
        check_uem("IS1009a", "IS1009a 1 0.000 838.833313\n").unwrap();
        assert!(check_uem("IS1009a", "ES2004a 1 0.000 1049.354687\n").is_err());
        assert!(check_uem("IS1009a", "IS1009a 1 5 1\n").is_err());
    }

    #[test]
    fn fixtures_committed_librispeech_subset_meets_plan_constraints() {
        let repo = crate::repo_root();
        let picks = load_subset(&repo).unwrap();
        check_subset(&picks).unwrap();
        let test_speakers: BTreeSet<u32> = picks
            .iter()
            .filter(|p| p.split == "test-clean")
            .map(|p| p.speaker)
            .collect();
        assert_eq!(test_speakers.len(), 40);
    }

    #[test]
    fn fixtures_committed_smoke_set_meets_plan_constraints() {
        let repo = crate::repo_root();
        let m = Manifest::load(&manifest_path(&repo)).unwrap();
        check_smoke(&repo, &m).unwrap();
    }

    #[test]
    fn fixtures_manifest_pins_librispeech_archives() {
        let repo = crate::repo_root();
        let m = Manifest::load(&manifest_path(&repo)).unwrap();
        let ls = m.select(&["librispeech".to_string()]).unwrap();
        for split in SPLITS {
            let a = ls
                .iter()
                .find(|a| a.url.ends_with(&format!("/{split}.tar.gz")))
                .unwrap_or_else(|| panic!("{split} archive missing"));
            assert_eq!(a.licence, "CC-BY-4.0");
            assert!(a.url.starts_with("https://www.openslr.org/resources/12/"));
            let x = a.extract.as_ref().expect("archive is extracted");
            assert_eq!(x.into, format!("librispeech/{split}"));
        }
        let licences = fs::read_to_string(repo.join("fixtures").join("LICENSES.md")).unwrap();
        assert!(licences.contains("LibriSpeech") && licences.contains("CC BY 4.0"));
    }
}
