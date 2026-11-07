//! Artefact manifests (`fixtures/manifest.toml`, `models/manifest.toml`) and the fetch
//! runner shared by `fetch-fixtures` and `fetch-models`.
//!
//! Every artefact names its source URL, size, SHA-256, licence and attribution. URLs must be
//! `https` and their host must be listed in the manifest's `hosts`; a download may only be
//! redirected (over https) to `hosts` or `redirect_hosts`, so the set of network
//! destinations is visible in one place (PLAN.md 3.9, AGENTS.md section 9).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::archive;
use crate::download::{self, Expected, Outcome};

/// A parsed manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Set names an artefact may belong to (`all` is implicit).
    pub sets: Vec<String>,
    /// Hosts that artefact URLs may point at.
    pub hosts: Vec<String>,
    /// Hosts a download may be redirected to besides `hosts` (CDNs): exact names or
    /// `*.<domain>` for any host below `<domain>`.
    #[serde(default)]
    pub redirect_hosts: Vec<String>,
    /// The artefacts.
    #[serde(default, rename = "artefact")]
    pub artefacts: Vec<Artefact>,
    /// Transcription models made of artefacts (T6.10e).
    #[serde(default, rename = "model")]
    pub models: Vec<ModelEntry>,
    /// Transcription model ladders per backend, best first (T6.10e).
    #[serde(default, rename = "ladder")]
    pub ladders: Vec<Ladder>,
    /// What lands in the app's opt-in CUDA folder (T8.4ae). `fetch-models` only fetches the
    /// artefacts; the app extracts these files (earshot-runtime `cuda`).
    #[serde(default, rename = "cuda_archive")]
    pub cuda_archives: Vec<CudaArchive>,
}

/// The parts of the app's opt-in CUDA download.
pub const CUDA_PARTS: [&str; 2] = ["summaries", "transcription"];
/// The archive formats the app extracts (earshot-runtime `archive::Format`).
pub const CUDA_FORMATS: [&str; 2] = ["zip", "tar.bz2"];

/// An archive that fills part of the app's opt-in CUDA folder.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CudaArchive {
    /// One of [`CUDA_PARTS`].
    pub part: String,
    /// The artefact id of the archive.
    pub artefact: String,
    /// One of [`CUDA_FORMATS`].
    pub format: String,
    /// The files the app takes from it.
    #[serde(default, rename = "file")]
    pub files: Vec<CudaFile>,
}

/// One file of the app's opt-in CUDA folder.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CudaFile {
    /// Its name in the folder (flat).
    pub name: String,
    /// Its path inside the archive.
    pub member: String,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

/// Transcription model engines.
pub const ENGINES: [&str; 3] = ["whisper", "parakeet-ctc", "parakeet-tdt"];
/// Backends a ladder can be for.
pub const LADDER_BACKENDS: [&str; 4] = ["cuda", "vulkan", "qnn", "cpu"];
/// Ladder scopes: `english` (every model transcribes English) and `multilingual` (every
/// model covers more than one language; the model check keeps those covering the user's
/// meeting languages).
pub const LADDER_SCOPES: [&str; 2] = ["english", "multilingual"];

/// A transcription model: the artefacts it needs and the languages it covers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// Name (as `earshot --model` and `cargo xtask model-eval` use it).
    pub name: String,
    /// One of [`ENGINES`].
    pub engine: String,
    /// Artefact ids the model needs.
    pub artefacts: Vec<String>,
    /// Language codes the model transcribes, as its publisher lists them (ISO 639-1, plus
    /// whisper.cpp's `haw`, `yue` and `jw`).
    pub languages: Vec<String>,
    /// Whether the language is detected (true) or must be English/given (false).
    pub detects_language: bool,
}

/// The models for one backend and scope, best first, each the fallback of the one before.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ladder {
    /// One of [`LADDER_BACKENDS`].
    pub backend: String,
    /// One of [`LADDER_SCOPES`].
    pub scope: String,
    /// Model names, best first.
    pub models: Vec<String>,
    /// Where the ordering was measured (a docs note).
    pub source: String,
}

/// Where an artefact lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    /// Downloaded into the cache directory; `path` is relative to the cache.
    Cache,
    /// Committed to the repository; `path` is relative to the repo root.
    Repo,
}

/// One downloadable or committed file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artefact {
    /// Unique id.
    pub id: String,
    /// Sets this artefact belongs to.
    pub sets: Vec<String>,
    /// Cache (downloaded) or repo (committed).
    pub location: Location,
    /// Source URL. For committed files this records where the file came from.
    pub url: String,
    /// For committed files copied out of an archive: the member path inside `url`.
    #[serde(default)]
    pub source_member: Option<String>,
    /// Relative path (to the cache or the repo root, see `location`).
    pub path: String,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
    /// SPDX-style licence id.
    pub licence: String,
    /// Attribution text.
    pub attribution: String,
    /// Optional publisher checksum this pin was checked against (for example `md5:...`).
    #[serde(default)]
    pub publisher_checksum: Option<String>,
    /// Archive extraction, if any.
    #[serde(default)]
    pub extract: Option<Extract>,
}

/// Extraction of an archive artefact.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extract {
    /// `tar.gz`, `tar.bz2` or `zip`.
    pub format: String,
    /// Directory (relative to the cache) the archive is extracted into.
    pub into: String,
    /// Extracted files whose checksum is pinned (paths relative to `into`).
    #[serde(default, rename = "member")]
    pub members: Vec<Member>,
}

/// A pinned file inside an extracted archive.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    /// Path relative to the extraction directory.
    pub path: String,
    /// Lower-case hex SHA-256.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Host part of an `https://` URL.
pub fn https_host(url: &str) -> Result<&str> {
    let rest = url
        .strip_prefix("https://")
        .with_context(|| format!("url `{url}` must use https"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.contains('@') || host.contains(':') {
        bail!("url `{url}` has no plain host");
    }
    Ok(host)
}

/// Whether `h` is a plain lower-case DNS host name with at least two labels.
fn is_host_name(h: &str) -> bool {
    let labels: Vec<&str> = h.split('.').collect();
    labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Lower-cased path components of a cache-relative path (the cache may live on a
/// case-insensitive file system).
fn components(rel: &str) -> Vec<String> {
    rel.split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_lowercase)
        .collect()
}

/// Whether a manifest path is inside the app's CUDA folder. The rule of earshot-runtime's
/// `catalogue::in_cuda_folder`, pinned by `fixtures/models/cuda_folder_cases.toml`.
pub fn in_cuda_folder(path: &str) -> bool {
    components(path).first().is_some_and(|c| c == "cuda")
}

/// Checks that no two cache locations overlap: an artefact file and its `.part`, an
/// extraction directory, its `.tmp` and the `.old` the app puts it aside to (earshot-runtime's
/// `catalogue::extract_cache_paths`). Two locations overlap when they are equal or one
/// lies inside the other (extracting would replace the archive or another artefact).
fn check_cache_overlaps(artefacts: &[Artefact]) -> Result<()> {
    let mut owned: Vec<(Vec<String>, String)> = Vec::new();
    for a in artefacts.iter().filter(|a| a.location == Location::Cache) {
        owned.push((components(&a.path), format!("`{}` path", a.id)));
        owned.push((
            components(&format!("{}.part", a.path)),
            format!("`{}` .part", a.id),
        ));
        if let Some(x) = &a.extract {
            owned.push((components(&x.into), format!("`{}` extract.into", a.id)));
            owned.push((
                components(&format!("{}.tmp", x.into)),
                format!("`{}` extract temp", a.id),
            ));
            owned.push((
                components(&format!("{}.old", x.into)),
                format!("`{}` extract aside", a.id),
            ));
        }
    }
    for (i, (pa, wa)) in owned.iter().enumerate() {
        for (pb, wb) in &owned[i + 1..] {
            let n = pa.len().min(pb.len());
            if pa[..n] == pb[..n] {
                bail!(
                    "{wa} ({}) and {wb} ({}) overlap",
                    pa.join("/"),
                    pb.join("/")
                );
            }
        }
    }
    Ok(())
}

impl Manifest {
    /// Loads and validates a manifest file.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("manifest {}", path.display()))
    }

    /// Parses and validates manifest text.
    pub fn parse(text: &str) -> Result<Self> {
        let m: Self = toml::from_str(text)?;
        m.validate()?;
        Ok(m)
    }

    /// Checks ids, sets, hosts, checksums, licences and that every path stays in its root.
    pub fn validate(&self) -> Result<()> {
        if self.sets.iter().any(|s| s == "all") {
            bail!("`all` is implicit and may not be declared as a set");
        }
        for h in &self.hosts {
            if !is_host_name(h) {
                bail!("`hosts` entry `{h}` must be a lower-case host name");
            }
        }
        for h in &self.redirect_hosts {
            if !is_host_name(h.strip_prefix("*.").unwrap_or(h)) {
                bail!(
                    "`redirect_hosts` entry `{h}` must be a lower-case host name or `*.<domain>`"
                );
            }
        }
        let root = Path::new("root");
        let mut ids = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for a in &self.artefacts {
            let ctx = || format!("artefact `{}`", a.id);
            if a.id.is_empty() || !ids.insert(a.id.as_str()) {
                bail!("artefact id `{}` is empty or duplicated", a.id);
            }
            if a.sets.is_empty() {
                bail!("{}: belongs to no set", ctx());
            }
            for s in &a.sets {
                if !self.sets.contains(s) {
                    bail!("{}: unknown set `{s}`", ctx());
                }
            }
            let host = https_host(&a.url).with_context(ctx)?;
            if !self.hosts.iter().any(|h| h == host) {
                bail!("{}: host `{host}` is not listed in `hosts`", ctx());
            }
            if !is_sha256(&a.sha256) {
                bail!("{}: sha256 must be 64 lower-case hex digits", ctx());
            }
            if a.size == 0 {
                bail!("{}: size must be > 0", ctx());
            }
            if a.licence.trim().is_empty() || a.attribution.trim().is_empty() {
                bail!("{}: licence and attribution are required", ctx());
            }
            download::jail_join(root, &a.path).with_context(ctx)?;
            if !paths.insert((a.location == Location::Repo, a.path.clone())) {
                bail!("{}: path `{}` is used twice", ctx(), a.path);
            }
            if let Some(x) = &a.extract {
                if a.location != Location::Cache {
                    bail!("{}: only cache artefacts can be extracted", ctx());
                }
                archive::Format::parse(&x.format).with_context(ctx)?;
                download::jail_join(root, &x.into).with_context(ctx)?;
                for m in &x.members {
                    download::jail_join(root, &m.path).with_context(ctx)?;
                    if !is_sha256(&m.sha256) || m.size == 0 {
                        bail!("{}: member `{}` needs sha256 and size", ctx(), m.path);
                    }
                }
            }
        }
        check_cache_overlaps(&self.artefacts)?;
        self.validate_models()?;
        self.validate_cuda()
    }

    /// Checks `[[cuda_archive]]`, the rules of earshot-runtime's `Catalogue::cuda_archives_of`
    /// (a rule change edits both): a known part and format; an artefact that exists, used by one
    /// archive only; every file a plain name, unique without regard to case across both parts,
    /// its digest and size given and equal to the artefact's own pin of that member if it has
    /// one; and no cache artefact stored inside the folder.
    fn validate_cuda(&self) -> Result<()> {
        for a in &self.artefacts {
            let into = a.extract.as_ref().map_or("", |x| x.into.as_str());
            if in_cuda_folder(&a.path) || in_cuda_folder(into) {
                bail!("artefact `{}` is stored inside the CUDA folder", a.id);
            }
        }
        let root = Path::new("root");
        let mut used = BTreeSet::new();
        let mut names = BTreeSet::new();
        for r in &self.cuda_archives {
            let ctx = || format!("cuda_archive `{}`", r.artefact);
            if !CUDA_PARTS.contains(&r.part.as_str()) {
                bail!("{}: unknown part `{}`", ctx(), r.part);
            }
            if !CUDA_FORMATS.contains(&r.format.as_str()) {
                bail!("{}: the app cannot extract format `{}`", ctx(), r.format);
            }
            let Some(artefact) = self.artefacts.iter().find(|a| a.id == r.artefact) else {
                bail!("{}: unknown artefact", ctx());
            };
            if !used.insert(r.artefact.as_str()) {
                bail!("{}: listed twice", ctx());
            }
            if r.files.is_empty() {
                bail!("{}: names no file", ctx());
            }
            for f in &r.files {
                if f.name.is_empty()
                    || f.name == "."
                    || f.name == ".."
                    || f.name.contains(['/', '\\', ':'])
                {
                    bail!("{}: file name `{}` is not a plain name", ctx(), f.name);
                }
                if !names.insert(f.name.to_lowercase()) {
                    bail!("{}: file name `{}` repeats", ctx(), f.name);
                }
                download::jail_join(root, &f.member).with_context(ctx)?;
                if !is_sha256(&f.sha256) || f.size == 0 {
                    bail!("{}: file `{}` needs a sha256 and a size", ctx(), f.name);
                }
                let pin = artefact
                    .extract
                    .as_ref()
                    .and_then(|x| x.members.iter().find(|m| m.path == f.member));
                if pin.is_some_and(|m| m.sha256 != f.sha256 || m.size != f.size) {
                    bail!(
                        "{}: file `{}` differs from the artefact's own pin",
                        ctx(),
                        f.name
                    );
                }
            }
        }
        Ok(())
    }

    /// Checks `[[model]]` and `[[ladder]]`: unique names, known engines and backends,
    /// existing artefacts, language codes, ladders naming only listed models once each.
    fn validate_models(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        for m in &self.models {
            let ctx = || format!("model `{}`", m.name);
            if m.name.is_empty() || !names.insert(m.name.as_str()) {
                bail!("model name `{}` is empty or duplicated", m.name);
            }
            if !ENGINES.contains(&m.engine.as_str()) {
                bail!("{}: unknown engine `{}`", ctx(), m.engine);
            }
            if m.artefacts.is_empty() {
                bail!("{}: lists no artefacts", ctx());
            }
            for id in &m.artefacts {
                if !self.artefacts.iter().any(|a| &a.id == id) {
                    bail!("{}: unknown artefact `{id}`", ctx());
                }
            }
            let code = |l: &String| {
                (2..=3).contains(&l.len()) && l.bytes().all(|b| b.is_ascii_lowercase())
            };
            let mut seen = BTreeSet::new();
            if m.languages.is_empty()
                || !m.languages.iter().all(code)
                || !m.languages.iter().all(|l| seen.insert(l))
            {
                bail!(
                    "{}: languages must be distinct lower-case language codes",
                    ctx()
                );
            }
        }
        let mut keys = BTreeSet::new();
        for l in &self.ladders {
            let ctx = || format!("ladder `{}`/`{}`", l.backend, l.scope);
            if !LADDER_BACKENDS.contains(&l.backend.as_str())
                || !LADDER_SCOPES.contains(&l.scope.as_str())
                || !keys.insert((&l.backend, &l.scope))
            {
                bail!("{}: unknown backend or scope, or listed twice", ctx());
            }
            if l.models.is_empty() || l.source.trim().is_empty() {
                bail!("{}: needs models and a source", ctx());
            }
            let mut seen = BTreeSet::new();
            for n in &l.models {
                if !names.contains(n.as_str()) || !seen.insert(n) {
                    bail!(
                        "{}: model `{n}` is not a [[model]] or is listed twice",
                        ctx()
                    );
                }
                let m = self.models.iter().find(|m| &m.name == n);
                let fits = m.is_some_and(|m| match l.scope.as_str() {
                    "english" => m.languages.iter().any(|x| x == "en"),
                    _ => m.languages.len() > 1,
                });
                if !fits {
                    bail!(
                        "{}: model `{n}` does not cover the ladder's languages",
                        ctx()
                    );
                }
            }
        }
        Ok(())
    }

    /// What in `set` is not a default model (PLAN D22: tests, thresholds
    /// and ci use only the default models), one line per offence: an artefact of a
    /// `[[model]]` that no English `[[ladder]]` (of any backend) names, a whisper or Parakeet
    /// model file that no `[[model]]` owns, or a local LLM (`.gguf`) outside the `summary`
    /// set. The NeMo reference front end (`parakeet/onnx-asr/`) is not a transcription model
    /// and is not judged. The ladders are read from this manifest, never from a second list.
    pub fn non_default_models(&self, set: &str) -> Vec<String> {
        let english: BTreeSet<&str> = self
            .ladders
            .iter()
            .filter(|l| l.scope == "english")
            .flat_map(|l| l.models.iter().map(String::as_str))
            .collect();
        let mut out = Vec::new();
        for a in self
            .artefacts
            .iter()
            .filter(|a| a.sets.iter().any(|s| s == set))
        {
            let owners: Vec<&str> = self
                .models
                .iter()
                .filter(|m| m.artefacts.contains(&a.id))
                .map(|m| m.name.as_str())
                .collect();
            for m in owners.iter().filter(|m| !english.contains(**m)) {
                out.push(format!(
                    "{set}: {} belongs to model {m}, which is on no English ladder",
                    a.id
                ));
            }
            let model_file = a.path.starts_with("whisper/")
                || (a.path.starts_with("parakeet/") && !a.path.starts_with("parakeet/onnx-asr/"));
            if owners.is_empty() && model_file {
                out.push(format!(
                    "{set}: {} is a model file that no [[model]] names",
                    a.id
                ));
            }
            if a.path.ends_with(".gguf") && !a.sets.iter().any(|s| s == "summary") {
                out.push(format!(
                    "{set}: {} is a local LLM outside the summary set",
                    a.id
                ));
            }
        }
        out
    }

    /// The download policy of this manifest (see [`download::Policy`]).
    pub fn policy(&self) -> download::Policy {
        download::Policy::new(&self.hosts, &self.redirect_hosts)
    }

    /// Artefacts in any of `sets` (`all` selects everything), in manifest order.
    /// Unknown set names and sets without artefacts are errors.
    pub fn select(&self, sets: &[String]) -> Result<Vec<&Artefact>> {
        if sets.is_empty() {
            bail!("no set given");
        }
        for s in sets {
            if s != "all" && !self.sets.contains(s) {
                bail!("unknown set `{s}` (known: {}, all)", self.sets.join(", "));
            }
            if s != "all" && !self.artefacts.iter().any(|a| a.sets.contains(s)) {
                bail!("set `{s}` has no artefacts yet");
            }
        }
        let all = sets.iter().any(|s| s == "all");
        Ok(self
            .artefacts
            .iter()
            .filter(|a| all || a.sets.iter().any(|s| sets.contains(s)))
            .collect())
    }
}

/// Resolves a cache directory: `env_var` if set and non-empty, else
/// `%LOCALAPPDATA%\<default_rel>`.
pub fn cache_dir(env_var: &str, default_rel: &str) -> Result<PathBuf> {
    if let Some(v) = std::env::var_os(env_var).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .with_context(|| format!("neither {env_var} nor LOCALAPPDATA is set"))?;
    Ok(PathBuf::from(base).join(default_rel))
}

/// What happened to one artefact.
#[derive(Debug)]
pub struct Report {
    /// Artefact id.
    pub id: String,
    /// Bytes of the artefact itself.
    pub size: u64,
    /// Bytes actually transferred in this run.
    pub downloaded: u64,
    /// Short status for the summary table.
    pub status: &'static str,
}

/// Fetches (or, for committed files, checks) every selected artefact.
///
/// With `verify`, existing downloads are re-hashed and every extracted file is re-hashed
/// against the extraction marker (a mismatch, or a marker without file hashes, re-extracts);
/// without it, a file of the right size and a completed extraction are trusted. Committed
/// files are always hashed. Downloads follow `policy`.
pub fn fetch_all(
    arts: &[&Artefact],
    repo: &Path,
    cache: &Path,
    verify: bool,
    policy: &download::Policy,
) -> Result<Vec<Report>> {
    let mut out = Vec::new();
    for a in arts {
        let exp = Expected {
            sha256: &a.sha256,
            size: a.size,
        };
        let r = match a.location {
            Location::Repo => {
                let p = download::jail_join(repo, &a.path)?;
                download::verify_file(&p, &exp).with_context(|| format!("artefact `{}`", a.id))?;
                Report {
                    id: a.id.clone(),
                    size: a.size,
                    downloaded: 0,
                    status: "committed, verified",
                }
            }
            Location::Cache => {
                let p = download::jail_join(cache, &a.path)?;
                let outcome = download::fetch(&a.url, &p, &exp, verify, policy)
                    .with_context(|| format!("artefact `{}`", a.id))?;
                let downloaded = match outcome {
                    Outcome::Present => 0,
                    Outcome::Downloaded { resumed_from } => a.size - resumed_from,
                };
                let mut status = match (&outcome, verify) {
                    (Outcome::Present, true) => "present, verified",
                    (Outcome::Present, false) => "present",
                    (Outcome::Downloaded { .. }, _) => "downloaded, verified",
                };
                if let Some(x) = &a.extract {
                    let into = download::jail_join(cache, &x.into)?;
                    let mut extracted = archive::is_extracted(&into, &a.sha256);
                    if extracted
                        && verify
                        && let Err(e) = archive::verify_extracted(&into, &a.sha256)
                    {
                        eprintln!("fetch: {}: {e:#}; extracting again", a.id);
                        extracted = false;
                    }
                    if !extracted {
                        println!("fetch: extracting {} -> {}", a.id, into.display());
                        archive::extract(&p, archive::Format::parse(&x.format)?, &into, &a.sha256)?;
                        status = "extracted, verified";
                    }
                    check_members(x, &into, verify || status == "extracted, verified")
                        .with_context(|| format!("artefact `{}`", a.id))?;
                }
                Report {
                    id: a.id.clone(),
                    size: a.size,
                    downloaded,
                    status,
                }
            }
        };
        out.push(r);
    }
    Ok(out)
}

fn check_members(x: &Extract, into: &Path, hash: bool) -> Result<()> {
    for m in &x.members {
        let p = download::jail_join(into, &m.path)?;
        let exp = Expected {
            sha256: &m.sha256,
            size: m.size,
        };
        if hash {
            download::verify_file(&p, &exp)?;
        } else if std::fs::metadata(&p).map(|md| md.len()).ok() != Some(m.size) {
            bail!(
                "extracted file {} is missing or has the wrong size",
                p.display()
            );
        }
    }
    Ok(())
}

/// Prints the per-artefact summary and totals.
pub fn print_reports(what: &str, cache: &Path, reports: &[Report]) {
    println!("{what}: cache {}", cache.display());
    for r in reports {
        println!("  {:<40} {:>14} bytes  {}", r.id, r.size, r.status);
    }
    let total: u64 = reports.iter().map(|r| r.size).sum();
    let got: u64 = reports.iter().map(|r| r.downloaded).sum();
    println!(
        "{what}: {} artefacts, {total} bytes ({:.1} MB), {got} bytes downloaded this run",
        reports.len(),
        total as f64 / 1e6
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn manifest(artefact: &str) -> String {
        format!("sets = [\"a\", \"b\"]\nhosts = [\"example.org\"]\n\n[[artefact]]\n{artefact}\n")
    }

    fn good() -> String {
        format!(
            "id = \"x\"\nsets = [\"a\"]\nlocation = \"cache\"\nurl = \"https://example.org/x.tar.gz\"\npath = \"d/x.tar.gz\"\nsha256 = \"{SHA}\"\nsize = 10\nlicence = \"CC-BY-4.0\"\nattribution = \"someone\""
        )
    }

    #[test]
    fn valid_manifest_parses() {
        let m = Manifest::parse(&manifest(&good())).unwrap();
        assert_eq!(m.artefacts.len(), 1);
    }

    #[test]
    fn validation_rejects_bad_fields() {
        let cases = [
            (
                "url = \"https://example.org/x.tar.gz\"",
                "url = \"http://example.org/x\"",
            ),
            (
                "url = \"https://example.org/x.tar.gz\"",
                "url = \"https://evil.org/x\"",
            ),
            ("path = \"d/x.tar.gz\"", "path = \"../x\""),
            ("path = \"d/x.tar.gz\"", "path = \"C:/x\""),
            ("sets = [\"a\"]", "sets = [\"c\"]"),
            ("sets = [\"a\"]", "sets = []"),
            ("size = 10", "size = 0"),
            ("licence = \"CC-BY-4.0\"", "licence = \"\""),
            ("attribution = \"someone\"", "attribution = \" \""),
        ];
        for (from, to) in cases {
            let text = manifest(&good().replace(from, to));
            assert!(Manifest::parse(&text).is_err(), "{to} must be rejected");
        }
        let upper = manifest(&good().replace(SHA, &"A".repeat(64)));
        assert!(Manifest::parse(&upper).is_err());
        let short = manifest(&good().replace(SHA, "abc"));
        assert!(Manifest::parse(&short).is_err());
        let extract = format!(
            "{}\nextract = {{ format = \"rar\", into = \"d/x\" }}",
            good()
        );
        assert!(Manifest::parse(&manifest(&extract)).is_err());
        let escape = format!(
            "{}\nextract = {{ format = \"tar.gz\", into = \"../d\" }}",
            good()
        );
        assert!(Manifest::parse(&manifest(&escape)).is_err());
    }

    fn with_models(extra: &str) -> String {
        format!(
            "{}
[[model]]
name = \"m\"
engine = \"whisper\"
artefacts = [\"x\"]
languages = [\"en\"]
detects_language = false

[[ladder]]
backend = \"cpu\"
scope = \"english\"
models = [\"m\"]
source = \"docs/x.md\"
{extra}",
            manifest(&good())
        )
    }

    #[test]
    fn models_and_ladders_are_validated() {
        let m = Manifest::parse(&with_models("")).unwrap();
        assert_eq!((m.models.len(), m.ladders.len()), (1, 1));
        let bad = [
            ("engine = \"whisper\"", "engine = \"other\""),
            ("artefacts = [\"x\"]", "artefacts = [\"nope\"]"),
            ("artefacts = [\"x\"]", "artefacts = []"),
            ("languages = [\"en\"]", "languages = [\"English\"]"),
            ("languages = [\"en\"]", "languages = []"),
            ("languages = [\"en\"]", "languages = [\"en\", \"en\"]"),
            ("backend = \"cpu\"", "backend = \"gpu\""),
            ("scope = \"english\"", "scope = \"all\""),
            ("models = [\"m\"]", "models = [\"unknown\"]"),
            ("models = [\"m\"]", "models = [\"m\", \"m\"]"),
            ("models = [\"m\"]", "models = []"),
            ("source = \"docs/x.md\"", "source = \" \""),
        ];
        for (from, to) in bad {
            let text = with_models("").replace(from, to);
            assert!(Manifest::parse(&text).is_err(), "{to} must be rejected");
        }
        let twice = with_models(
            "
[[ladder]]
backend = \"cpu\"
scope = \"english\"
models = [\"m\"]
source = \"y\"
",
        );
        assert!(
            Manifest::parse(&twice).is_err(),
            "duplicated backend and scope"
        );
        // The same backend with the other scope is fine only for a model that covers it:
        // "m" is English only, so a multilingual ladder of it is refused.
        let multi = with_models(
            "
[[ladder]]
backend = \"cpu\"
scope = \"multilingual\"
models = [\"m\"]
source = \"y\"
",
        );
        assert!(
            Manifest::parse(&multi).is_err(),
            "English-only model on a multilingual ladder"
        );
        let ok_multi = with_models(
            "
[[model]]
name = \"w\"
engine = \"whisper\"
artefacts = [\"x\"]
languages = [\"en\", \"de\"]
detects_language = true

[[ladder]]
backend = \"cpu\"
scope = \"multilingual\"
models = [\"w\"]
source = \"y\"
",
        );
        Manifest::parse(&ok_multi).unwrap();
        let dup = with_models(
            "
[[model]]
name = \"m\"
engine = \"whisper\"
artefacts = [\"x\"]
languages = [\"en\"]
detects_language = false
",
        );
        assert!(Manifest::parse(&dup).is_err(), "duplicated model");
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let text = format!(
            "{}\n[[artefact]]\n{}",
            manifest(&good()),
            good().replace("d/x", "d/y")
        );
        assert!(Manifest::parse(&text).is_err());
    }

    #[test]
    fn select_by_set_and_all() {
        let second = good()
            .replace("id = \"x\"", "id = \"y\"")
            .replace("d/x", "d/y")
            .replace("sets = [\"a\"]", "sets = [\"b\"]");
        let text = format!("{}\n[[artefact]]\n{second}", manifest(&good()));
        let m = Manifest::parse(&text).unwrap();
        let ids = |sets: &[&str]| -> Vec<String> {
            let sets: Vec<String> = sets.iter().map(|s| s.to_string()).collect();
            m.select(&sets)
                .unwrap()
                .iter()
                .map(|a| a.id.clone())
                .collect()
        };
        assert_eq!(ids(&["a"]), ["x"]);
        assert_eq!(ids(&["b"]), ["y"]);
        assert_eq!(ids(&["a", "b"]), ["x", "y"]);
        assert_eq!(ids(&["all"]), ["x", "y"]);
        assert!(m.select(&["c".to_string()]).is_err());
        assert!(m.select(&[]).is_err());
    }

    #[test]
    fn redirect_hosts_are_validated() {
        let with = |list: &str| {
            manifest(&good()).replace(
                "hosts = [\"example.org\"]",
                &format!("hosts = [\"example.org\"]\nredirect_hosts = [{list}]"),
            )
        };
        let m = Manifest::parse(&with("\"cdn.example.net\", \"*.example.com\"")).unwrap();
        assert_eq!(m.redirect_hosts, ["cdn.example.net", "*.example.com"]);
        assert!(
            Manifest::parse(&manifest(&good()))
                .unwrap()
                .redirect_hosts
                .is_empty()
        );
        for bad in [
            "\"*\"",
            "\"*.com\"",
            "\"CDN.example.net\"",
            "\"a.b:443\"",
            "\"**.a.b\"",
            "\"\"",
        ] {
            assert!(
                Manifest::parse(&with(bad)).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn overlapping_cache_paths_are_rejected() {
        let extract = |into: &str| {
            format!(
                "{}\nextract = {{ format = \"tar.gz\", into = \"{into}\" }}",
                good()
            )
        };
        // Sibling directory: fine.
        Manifest::parse(&manifest(&extract("d/x"))).unwrap();
        // Extracting into the archive's own directory, onto the archive, below it, or onto
        // its .part would destroy the archive.
        for into in [
            "d",
            "D",
            "d/x.tar.gz",
            "d/x.tar.gz/inner",
            "d/x.tar.gz.part",
            "./d",
        ] {
            let err = Manifest::parse(&manifest(&extract(into))).unwrap_err();
            assert!(format!("{err:#}").contains("overlap"), "{into}: {err:#}");
        }
        // Two artefacts: one extracts where the other is stored.
        let second = good()
            .replace("id = \"x\"", "id = \"y\"")
            .replace("d/x.tar.gz", "e/y.tar.gz");
        let text = format!("{}\n[[artefact]]\n{second}", manifest(&extract("e")));
        let err = Manifest::parse(&text).unwrap_err();
        assert!(format!("{err:#}").contains("overlap"), "{err:#}");
        // An artefact path equal to another's extraction temp directory.
        let tmp_clash = good()
            .replace("id = \"x\"", "id = \"y\"")
            .replace("d/x.tar.gz", "d/out.tmp");
        let text = format!("{}\n[[artefact]]\n{tmp_clash}", manifest(&extract("d/out")));
        assert!(Manifest::parse(&text).is_err());
        // The same for the folder the app puts an extraction aside to, and below it.
        for path in ["d/out.old", "d/out.old/y.tar.gz"] {
            let old_clash = good()
                .replace("id = \"x\"", "id = \"y\"")
                .replace("d/x.tar.gz", path);
            let text = format!("{}\n[[artefact]]\n{old_clash}", manifest(&extract("d/out")));
            let err = Manifest::parse(&text).unwrap_err();
            assert!(
                format!("{err:#}").contains("extract aside"),
                "{path}: {err:#}"
            );
        }
    }

    #[test]
    fn verify_reextracts_a_corrupted_extraction() {
        use std::io::Write as _;
        let dir =
            std::env::temp_dir().join(format!("earshot-xtask-mf-reextract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("d")).unwrap();
        // A tar.gz with one file, already "downloaded" so no request is made.
        let mut tb = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o644);
        h.set_cksum();
        tb.append_data(&mut h, "top/a.txt", &b"alpha"[..]).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tb.into_inner().unwrap()).unwrap();
        let bytes = gz.finish().unwrap();
        std::fs::write(dir.join("d").join("x.tar.gz"), &bytes).unwrap();
        let sum = download::hex(&<sha2::Sha256 as sha2::Digest>::digest(&bytes));
        let text = manifest(
            &format!(
                "{}\nextract = {{ format = \"tar.gz\", into = \"d/x\" }}",
                good()
            )
            .replace(SHA, &sum)
            .replace("size = 10", &format!("size = {}", bytes.len())),
        );
        let m = Manifest::parse(&text).unwrap();
        let arts: Vec<&Artefact> = m.artefacts.iter().collect();
        let policy = m.policy();
        let file = dir.join("d").join("x").join("top").join("a.txt");

        let r = fetch_all(&arts, &dir, &dir, false, &policy).unwrap();
        assert_eq!(r[0].status, "extracted, verified");
        // Same size, different bytes.
        std::fs::write(&file, b"ALPHA").unwrap();
        let r = fetch_all(&arts, &dir, &dir, false, &policy).unwrap();
        assert_eq!(
            r[0].status, "present",
            "without --verify a completed extraction is trusted"
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"ALPHA");
        let r = fetch_all(&arts, &dir, &dir, true, &policy).unwrap();
        assert_eq!(r[0].status, "extracted, verified");
        assert_eq!(std::fs::read(&file).unwrap(), b"alpha");
        let r = fetch_all(&arts, &dir, &dir, true, &policy).unwrap();
        assert_eq!(r[0].status, "present, verified");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_parsing() {
        assert_eq!(https_host("https://a.b/c?d").unwrap(), "a.b");
        assert!(https_host("http://a.b/c").is_err());
        assert!(https_host("https://user@a.b/c").is_err());
        assert!(https_host("https://a.b:8080/c").is_err());
    }
}
