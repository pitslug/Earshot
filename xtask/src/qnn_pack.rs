//! `cargo xtask qnn-pack` (T8.4g): one zip per NPU conversion of a Parakeet TDT model, for the
//! release that holds every file Earshot hosts ("Earshot Dependencies", PLAN 7).
//!
//! - **Input**: `<models>/parakeet-qnn/<model>`, what `cargo xtask qnn-convert` wrote. Every
//!   output file must match the size and SHA-256 its `conversion.json` records, the converter
//!   must be [`TASK`] with recipe [`DEFAULT_RECIPE`] (docs/qnn.md), and every source file the
//!   record names must be the manifest's pinned file of that model, so the zip holds exactly what
//!   GATE-03a measured, converted from the community copy the manifest names.
//! - **Output**: `<out>/<zip_name(model)>`: stored entries only (the quantised weights do not
//!   compress, and stored bytes do not depend on a deflate implementation), in a fixed order with
//!   a fixed time (1980-01-01 00:00), so two runs on the same files are byte-identical; a
//!   `NOTICE.md` carries the CC-BY-4.0 credit. No zip64: every entry and the whole archive stay
//!   below 4 GiB, which the app's and xtask's readers require.
//! - **Proof**: the zip is extracted again by xtask's own reader ([`crate::archive::extract`]) and
//!   every file compared, then its size and SHA-256 and the manifest entry are printed. Nothing is
//!   uploaded here: publishing is the owner's go-ahead.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::download;
use crate::manifest::Manifest;
use crate::qnn_convert::DEFAULT_RECIPE;

/// The converter version (`task` in `conversion.json`) whose conversions are published: the one
/// earshot-runtime's `backends::QNN_CONVERSION_TASKS` loads.
pub const TASK: &str = "T8.3c";

/// The manifest set of the published conversions.
pub const SET: &str = "qnn-models";

/// The files of a conversion (earshot-runtime `ParakeetModel::qnn_files`), in archive order
/// after the notice; every one but `conversion.json` is listed in its `outputs`.
pub const FILES: [&str; 6] = [
    "conversion.json",
    "tokens.txt",
    "encoder.onnx",
    "encoder.weights",
    "decoder.onnx",
    "joiner.onnx",
];

/// The credit file written into every archive, first.
pub const NOTICE: &str = "NOTICE.md";

/// The repository that publishes the conversions.
pub const REPO_URL: &str = "https://github.com/pitslug/Earshot";

/// The release tag that holds every file Earshot hosts (PLAN 7, 2026-11-08: never marked latest).
pub const RELEASE_TAG: &str = "cuda-set-llama-0.1.160-2";

/// The archive name of `model`'s conversion.
#[must_use]
pub fn zip_name(model: &str) -> String {
    format!("earshot-npu-{model}-{DEFAULT_RECIPE}.zip")
}

/// The release download URL of `model`'s archive.
#[must_use]
pub fn release_url(model: &str) -> String {
    format!(
        "{REPO_URL}/releases/download/{RELEASE_TAG}/{}",
        zip_name(model)
    )
}

/// One archive written by [`pack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    /// The model.
    pub model: String,
    /// The archive.
    pub zip: PathBuf,
    /// Its size in bytes.
    pub size: u64,
    /// Its lower-case hex SHA-256.
    pub sha256: String,
    /// Every entry, in archive order: (name, size, SHA-256).
    pub members: Vec<(String, u64, String)>,
}

impl Packed {
    /// The manifest `[[artefact]]` entry of this archive (attribution from `manifest`).
    ///
    /// # Errors
    /// As [`Credit::of`].
    pub fn manifest_entry(&self, manifest: &Manifest) -> Result<String> {
        let credit = Credit::of(manifest, &self.model)?;
        let mut s = format!(
            "[[artefact]]\nid = \"{model}-npu\"\nsets = [\"{SET}\"]\nlocation = \"cache\"\n\
             url = \"{url}\"\npath = \"archives/{zip}\"\nsha256 = \"{sha}\"\nsize = {size}\n\
             licence = \"CC-BY-4.0\"\nattribution = \"{attribution}\"\n\n[artefact.extract]\n\
             format = \"zip\"\ninto = \"parakeet-qnn/{model}\"\n",
            model = self.model,
            url = release_url(&self.model),
            zip = zip_name(&self.model),
            sha = self.sha256,
            size = self.size,
            attribution = credit.attribution(),
        );
        for (name, size, sha) in &self.members {
            s.push_str(&format!(
                "\n[[artefact.extract.member]]\npath = \"{name}\"\nsha256 = \"{sha}\"\nsize = {size}\n"
            ));
        }
        Ok(s)
    }
}

/// Who made the model a conversion starts from, read from the manifest's pins of that model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credit {
    /// The original model and its author, as the manifest names it
    /// ("NVIDIA Parakeet TDT 0.6B v2 (NVIDIA)").
    pub model: String,
    /// The model card.
    pub card: String,
    /// Who made the ONNX export ("the k2-fsa sherpa-onnx project", from the manifest's
    /// "converted to ONNX by ...").
    pub exporter: String,
    /// The export's repository and pinned commit.
    pub source: String,
    /// The pinned commit of the export.
    pub commit: String,
}

impl Credit {
    /// The credit of `model` from its first `[[model]]` artefact: attribution
    /// `<model> (<author>), CC-BY-4.0, <card>; <exporter>` and a Hugging Face URL
    /// `<repo>/resolve/<commit>/<file>`.
    ///
    /// # Errors
    /// No such Parakeet TDT model, or an attribution or URL of another shape.
    pub fn of(manifest: &Manifest, model: &str) -> Result<Self> {
        let entry = manifest
            .models
            .iter()
            .find(|m| m.name == model && m.engine == "parakeet-tdt")
            .with_context(|| format!("{model} is not a Parakeet TDT [[model]] of the manifest"))?;
        let first = entry
            .artefacts
            .first()
            .context("a model with no artefact")?;
        let a = manifest
            .artefacts
            .iter()
            .find(|a| &a.id == first)
            .context("a model naming an unknown artefact")?;
        let shape = || {
            format!(
                "artefact `{}`: attribution or url of an unexpected shape",
                a.id
            )
        };
        let (name, rest) = a
            .attribution
            .split_once(", CC-BY-4.0, ")
            .with_context(shape)?;
        let (card, exporter) = rest.split_once("; ").with_context(shape)?;
        let exporter = exporter
            .strip_prefix("converted to ONNX by ")
            .with_context(shape)?;
        let (source, tail) = a.url.split_once("/resolve/").with_context(shape)?;
        let commit = tail.split('/').next().unwrap_or_default();
        let author = name
            .split_once(" (")
            .and_then(|(_, a)| a.strip_suffix(')'))
            .unwrap_or_default();
        ensure!(
            a.licence == "CC-BY-4.0"
                && author.contains("NVIDIA")
                && card.starts_with("https://")
                && !exporter.is_empty()
                && commit.len() == 40,
            shape()
        );
        Ok(Self {
            model: name.to_owned(),
            card: card.to_owned(),
            exporter: exporter.to_owned(),
            source: source.to_owned(),
            commit: commit.to_owned(),
        })
    }

    /// The model without its author ("NVIDIA Parakeet TDT 0.6B v2").
    #[must_use]
    pub fn title(&self) -> &str {
        self.model.split_once(" (").map_or(&self.model, |(t, _)| t)
    }

    /// The author in the manifest's parenthesis ("NVIDIA", "NVIDIA NeMo and Suno.ai").
    #[must_use]
    pub fn author(&self) -> &str {
        self.model
            .split_once(" (")
            .and_then(|(_, a)| a.strip_suffix(')'))
            .unwrap_or("NVIDIA")
    }

    /// The manifest attribution of the conversion.
    #[must_use]
    pub fn attribution(&self) -> String {
        format!(
            "Earshot's NPU conversion of {}, CC-BY-4.0, {}; quantised for the Qualcomm NPU by \
             `cargo xtask qnn-convert` (recipe {DEFAULT_RECIPE}) from the ONNX export by {} (see \
             NOTICE.md in the archive)",
            self.model, self.card, self.exporter
        )
    }
}

/// The NPU conversion record (`conversion.json`), the fields packing reads.
struct Record {
    recipe: String,
    quantisation: String,
    /// Output file -> (size, SHA-256).
    outputs: Vec<(String, u64, String)>,
    /// Source file -> (size, SHA-256).
    sources: Vec<(String, u64, String)>,
}

fn pins(v: &serde_json::Value, what: &str) -> Result<Vec<(String, u64, String)>> {
    let map = v
        .as_object()
        .with_context(|| format!("conversion.json lists no {what}"))?;
    map.iter()
        .map(|(name, pin)| {
            let (Some(size), Some(sha)) = (pin["size"].as_u64(), pin["sha256"].as_str()) else {
                bail!("conversion.json has no size or sha256 for {what} {name}");
            };
            Ok((name.clone(), size, sha.to_ascii_lowercase()))
        })
        .collect()
}

fn read_record(dir: &Path) -> Result<Record> {
    let path = dir.join("conversion.json");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let task = v["task"].as_str().unwrap_or("none");
    let recipe = v["recipe"].as_str().unwrap_or("none");
    ensure!(
        task == TASK && recipe == DEFAULT_RECIPE,
        "{}: converter {task} with recipe {recipe}; only {TASK} with {DEFAULT_RECIPE} \
         (docs/qnn.md) is published",
        path.display()
    );
    let q = &v["quantisation"];
    let field = |k: &str| q[k].as_str().unwrap_or("not recorded").to_owned();
    Ok(Record {
        recipe: recipe.to_owned(),
        quantisation: format!(
            "{}; activations {}, weights {}, calibration {}",
            field("format"),
            field("activations"),
            field("weights"),
            field("calibration")
        ),
        outputs: pins(&v["outputs"], "outputs")?,
        sources: pins(&v["sources"], "sources")?,
    })
}

/// Checks `dir` against its record and the manifest (module docs).
fn check_conversion(manifest: &Manifest, model: &str, dir: &Path, record: &Record) -> Result<()> {
    let listed: Vec<&str> = record.outputs.iter().map(|(n, ..)| n.as_str()).collect();
    let want: Vec<&str> = FILES
        .iter()
        .copied()
        .filter(|f| *f != "conversion.json")
        .collect();
    let mut sorted = listed.clone();
    sorted.sort_unstable();
    let mut want_sorted = want.clone();
    want_sorted.sort_unstable();
    ensure!(
        sorted == want_sorted,
        "{}: conversion.json lists outputs {listed:?}, expected {want:?}",
        dir.display()
    );
    for (name, size, sha) in &record.outputs {
        download::verify_file(
            &dir.join(name),
            &download::Expected {
                sha256: sha,
                size: *size,
            },
        )
        .with_context(|| format!("{}: {name} differs from conversion.json", dir.display()))?;
    }
    let entry = manifest
        .models
        .iter()
        .find(|m| m.name == model && m.engine == "parakeet-tdt")
        .with_context(|| format!("{model} is not a Parakeet TDT [[model]] of the manifest"))?;
    for (name, size, sha) in &record.sources {
        let pinned = entry.artefacts.iter().find_map(|id| {
            manifest
                .artefacts
                .iter()
                .find(|a| &a.id == id && a.path.rsplit('/').next() == Some(name.as_str()))
        });
        let Some(a) = pinned else {
            bail!("conversion.json source {name} is no pinned file of {model}");
        };
        ensure!(
            a.size == *size && a.sha256 == *sha,
            "conversion.json source {name} is not the manifest's pin of {model} (`{}`)",
            a.id
        );
    }
    ensure!(
        !record.sources.is_empty(),
        "conversion.json names no source file"
    );
    Ok(())
}

/// The `NOTICE.md` of `model`'s archive.
fn notice(model: &str, credit: &Credit, record: &Record) -> String {
    let unchanged: Vec<&str> = record
        .outputs
        .iter()
        .filter(|(n, s, h)| {
            record
                .sources
                .iter()
                .any(|(sn, ss, sh)| sn == n && ss == s && sh == h)
        })
        .map(|(n, ..)| n.as_str())
        .collect();
    let unchanged = if unchanged.is_empty() {
        "Every file was changed.".to_owned()
    } else {
        format!(
            "Unchanged from the ONNX export: {}.",
            unchanged
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "# NPU conversion of {name}\n\
         \n\
         This archive (`{zip}`) holds Earshot's conversion of {name} for the Qualcomm NPU \
         (Hexagon, through ONNX Runtime's QNN execution provider). Earshot downloads it from its \
         Models page into `parakeet-qnn/{model}` and checks every file against `conversion.json` \
         before it loads.\n\
         \n\
         ## Credit and licence\n\
         \n\
         - Original model: {title}. Original author: {author}. Model card: {card}. This \
         conversion is not made, endorsed or supported by {author}.\n\
         - Licence: Creative Commons Attribution 4.0 International (CC-BY-4.0), \
         https://creativecommons.org/licenses/by/4.0/, the licence of the original model. This \
         conversion is shared under the same licence.\n\
         - Converted from: the ONNX export by {exporter}, {source} at commit {commit}.\n\
         \n\
         ## Changes\n\
         \n\
         This is a modified version of the model. The encoder was given a fixed input shape and \
         quantised for Qualcomm's HTP backend by `cargo xtask qnn-convert` (recipe {recipe}, \
         converter {TASK}) in {REPO_URL}: {quantisation}. {unchanged} `conversion.json` records \
         the tool versions and the size and SHA-256 of every source and output file.\n\
         \n\
         ## No warranty\n\
         \n\
         This conversion is provided as is, without warranty of any kind, express or implied, \
         including fitness for a particular purpose and the accuracy of its transcriptions \
         (CC-BY-4.0 section 5). Use it at your own risk.\n",
        name = credit.model,
        title = credit.title(),
        author = credit.author(),
        zip = zip_name(model),
        card = credit.card,
        exporter = credit.exporter,
        source = credit.source,
        commit = credit.commit,
        recipe = record.recipe,
        quantisation = record.quantisation,
    )
}

/// A zip entry's contents.
enum Source {
    Bytes(Vec<u8>),
    File(PathBuf),
}

impl Source {
    fn reader(&self) -> Result<Box<dyn Read + '_>> {
        Ok(match self {
            Self::Bytes(b) => Box::new(&b[..]),
            Self::File(p) => Box::new(BufReader::new(
                File::open(p).with_context(|| format!("open {}", p.display()))?,
            )),
        })
    }
}

/// CRC-32, size and SHA-256 of `src`, read once.
fn measure(src: &Source) -> Result<(u32, u64, String)> {
    use sha2::Digest;
    let mut r = src.reader()?;
    let (mut crc, mut sha, mut n) = (flate2::Crc::new(), sha2::Sha256::new(), 0u64);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            break;
        }
        crc.update(&buf[..k]);
        sha.update(&buf[..k]);
        n += k as u64;
    }
    Ok((crc.sum(), n, download::hex(&sha.finalize())))
}

/// MS-DOS time and date of every entry: 1980-01-01 00:00, the earliest a zip can say.
const DOS_TIME: u16 = 0;
const DOS_DATE: u16 = (1 << 5) | 1;
/// Version needed to extract (2.0) and made by (Unix, 3.0), so the mode below is read.
const VERSION_NEEDED: u16 = 20;
const VERSION_MADE_BY: u16 = 0x031e;
/// A regular file, rw-r--r--.
const FILE_MODE: u32 = 0o100_644;

/// Writes `entries` to `dest` as a zip of stored entries (PKWARE APPNOTE.TXT 4.3.7 local file
/// header, 4.3.12 central directory header, 4.3.16 end of central directory), deterministic
/// (module docs). Returns each entry's (name, size, SHA-256).
fn write_stored_zip(
    dest: &Path,
    entries: &[(String, Source)],
) -> Result<Vec<(String, u64, String)>> {
    let too_big = |what: &str| anyhow::anyhow!("{what} needs zip64, which no reader here accepts");
    ensure!(entries.len() < 0xFFFF, too_big("the entry count"));
    let mut out =
        BufWriter::new(File::create(dest).with_context(|| format!("create {}", dest.display()))?);
    let (mut cd, mut members, mut offset) = (Vec::new(), Vec::new(), 0u64);
    for (name, src) in entries {
        ensure!(
            !name.is_empty()
                && name.is_ascii()
                && !name.contains(['\\', ':'])
                && !name.starts_with('/'),
            "zip entry name `{name}`"
        );
        let (crc, size, sha) = measure(src)?;
        let size32 = u32::try_from(size)
            .ok()
            .filter(|s| *s != u32::MAX)
            .ok_or_else(|| too_big(name))?;
        let offset32 = u32::try_from(offset)
            .ok()
            .filter(|s| *s != u32::MAX)
            .ok_or_else(|| too_big("the archive"))?;
        let n = u16::try_from(name.len())?;
        let mut lh = Vec::with_capacity(30 + name.len());
        lh.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        lh.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
        lh.extend_from_slice(&0u16.to_le_bytes()); // flags
        lh.extend_from_slice(&0u16.to_le_bytes()); // stored
        lh.extend_from_slice(&DOS_TIME.to_le_bytes());
        lh.extend_from_slice(&DOS_DATE.to_le_bytes());
        lh.extend_from_slice(&crc.to_le_bytes());
        lh.extend_from_slice(&size32.to_le_bytes());
        lh.extend_from_slice(&size32.to_le_bytes());
        lh.extend_from_slice(&n.to_le_bytes());
        lh.extend_from_slice(&0u16.to_le_bytes()); // extra
        lh.extend_from_slice(name.as_bytes());
        out.write_all(&lh)?;
        let copied = std::io::copy(&mut src.reader()?, &mut out)?;
        ensure!(copied == size, "{name} changed while it was written");
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&VERSION_MADE_BY.to_le_bytes());
        cd.extend_from_slice(&VERSION_NEEDED.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes()); // flags
        cd.extend_from_slice(&0u16.to_le_bytes()); // stored
        cd.extend_from_slice(&DOS_TIME.to_le_bytes());
        cd.extend_from_slice(&DOS_DATE.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&size32.to_le_bytes());
        cd.extend_from_slice(&size32.to_le_bytes());
        cd.extend_from_slice(&n.to_le_bytes());
        cd.extend_from_slice(&[0; 8]); // extra, comment, disk, internal attributes
        cd.extend_from_slice(&(FILE_MODE << 16).to_le_bytes());
        cd.extend_from_slice(&offset32.to_le_bytes());
        cd.extend_from_slice(name.as_bytes());
        offset += lh.len() as u64 + size;
        members.push((name.clone(), size, sha));
    }
    let cd_offset = u32::try_from(offset)
        .ok()
        .filter(|s| *s != u32::MAX)
        .ok_or_else(|| too_big("the archive"))?;
    let cd_len = u32::try_from(cd.len())?;
    let count = u16::try_from(entries.len())?;
    out.write_all(&cd)?;
    let mut eocd = Vec::with_capacity(22);
    eocd.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    eocd.extend_from_slice(&[0; 4]); // disk numbers
    eocd.extend_from_slice(&count.to_le_bytes());
    eocd.extend_from_slice(&count.to_le_bytes());
    eocd.extend_from_slice(&cd_len.to_le_bytes());
    eocd.extend_from_slice(&cd_offset.to_le_bytes());
    eocd.extend_from_slice(&0u16.to_le_bytes()); // comment
    out.write_all(&eocd)?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(members)
}

/// Packs the conversion of `model` in `src` (its `parakeet-qnn/<model>` folder) into
/// `<out>/<zip_name(model)>` (module docs), extracts it again with xtask's reader and compares
/// every file.
///
/// # Errors
/// A conversion that differs from its record, the converter or recipe, or the manifest; I/O;
/// a round trip that does not give back the same files.
pub fn pack(manifest: &Manifest, model: &str, src: &Path, out: &Path) -> Result<Packed> {
    let record = read_record(src)?;
    check_conversion(manifest, model, src, &record)?;
    let credit = Credit::of(manifest, model)?;
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let mut entries = vec![(
        NOTICE.to_owned(),
        Source::Bytes(notice(model, &credit, &record).into_bytes()),
    )];
    entries.extend(
        FILES
            .iter()
            .map(|f| ((*f).to_owned(), Source::File(src.join(f)))),
    );
    let zip = out.join(zip_name(model));
    let part = out.join(format!("{}.part", zip_name(model)));
    let members = write_stored_zip(&part, &entries)?;
    if zip.exists() {
        fs::remove_file(&zip).with_context(|| format!("remove {}", zip.display()))?;
    }
    fs::rename(&part, &zip).with_context(|| format!("rename {}", part.display()))?;
    let packed = Packed {
        model: model.to_owned(),
        size: fs::metadata(&zip)?.len(),
        sha256: download::sha256_file(&zip)?,
        zip,
        members,
    };
    round_trip(&packed, out)?;
    Ok(packed)
}

/// Extracts `packed` with xtask's own reader into a scratch folder of `out` and compares every
/// entry; the folder is removed afterwards.
fn round_trip(packed: &Packed, out: &Path) -> Result<()> {
    let into = out.join(format!(".round-trip-{}", packed.model));
    if into.exists() {
        fs::remove_dir_all(&into)?;
    }
    let result = (|| {
        crate::archive::extract(
            &packed.zip,
            crate::archive::Format::Zip,
            &into,
            &packed.sha256,
        )?;
        crate::archive::verify_extracted(&into, &packed.sha256)?;
        let mut names: Vec<String> = fs::read_dir(&into)?
            .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_>>()?;
        names.retain(|n| n != crate::archive::MARKER);
        names.sort();
        let mut want: Vec<String> = packed.members.iter().map(|(n, ..)| n.clone()).collect();
        want.sort();
        ensure!(
            names == want,
            "the round trip gave {names:?}, expected {want:?}"
        );
        for (name, size, sha) in &packed.members {
            download::verify_file(
                &into.join(name),
                &download::Expected {
                    sha256: sha,
                    size: *size,
                },
            )
            .with_context(|| format!("round trip of {name}"))?;
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&into);
    result.with_context(|| format!("round trip of {}", packed.zip.display()))
}

/// The models whose conversions are published (the NPU ladder, PLAN 7).
pub const MODELS: [&str; 2] = ["parakeet-tdt-0.6b-v2", "parakeet-tdt-110m"];

/// `cargo xtask qnn-pack`: packs `models` (default [`MODELS`]) from `<models dir>/parakeet-qnn`
/// into `out` and prints each archive's size, SHA-256 and manifest entry.
///
/// # Errors
/// As [`pack`]; an unknown model.
pub fn run(root: &Path, models_dir: Option<&Path>, out: &Path, models: &[String]) -> Result<()> {
    let manifest = Manifest::load(&crate::models::manifest_path(root))?;
    let dir = match models_dir {
        Some(d) => d.to_path_buf(),
        None => crate::models::cache_dir()?,
    };
    let names: Vec<String> = if models.is_empty() {
        MODELS.iter().map(|m| (*m).to_owned()).collect()
    } else {
        models.to_vec()
    };
    for m in &names {
        ensure!(
            MODELS.contains(&m.as_str()),
            "--model {m}: not one of {MODELS:?}"
        );
    }
    let mut entries = Vec::new();
    for m in &names {
        let src = dir.join("parakeet-qnn").join(m);
        println!("qnn-pack: {m} from {}", src.display());
        let p = pack(&manifest, m, &src, out)?;
        println!(
            "qnn-pack: {} {} bytes sha256 {} (round trip through xtask's extraction: ok)",
            p.zip.display(),
            p.size,
            p.sha256
        );
        entries.push(p.manifest_entry(&manifest)?);
    }
    println!("\nqnn-pack: manifest entries (models/manifest.toml, set `{SET}`):\n");
    for e in entries {
        println!("{e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    fn sha(b: &[u8]) -> String {
        download::hex(&sha2::Sha256::digest(b))
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "earshot-xtask-qnn-pack-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    const ENCODER_SRC: &[u8] = b"float encoder";
    const DECODER: &[u8] = b"decoder";
    const JOINER: &[u8] = b"joiner";
    const TOKENS: &[u8] = b"<blk> 0\n";

    /// A manifest with one Parakeet TDT model whose pins are the fake source files.
    fn manifest() -> Manifest {
        let art = |id: &str, file: &str, bytes: &[u8]| {
            format!(
                "[[artefact]]\nid = \"{id}\"\nsets = [\"ci\"]\nlocation = \"cache\"\n\
                 url = \"https://huggingface.co/someone/export-tdt/resolve/{commit}/{file}\"\n\
                 path = \"parakeet/tdt-x/{file}\"\nsha256 = \"{sha}\"\nsize = {size}\n\
                 licence = \"CC-BY-4.0\"\n\
                 attribution = \"NVIDIA Parakeet TDT X (NVIDIA), CC-BY-4.0, https://huggingface.co/nvidia/x; converted to ONNX by someone\"\n\n",
                commit = "a".repeat(40),
                sha = sha(bytes),
                size = bytes.len()
            )
        };
        let text = format!(
            "sets = [\"ci\"]\nhosts = [\"huggingface.co\"]\n\n{}{}{}{}\
             [[model]]\nname = \"parakeet-tdt-x\"\nengine = \"parakeet-tdt\"\n\
             artefacts = [\"x-encoder\", \"x-decoder\", \"x-joiner\", \"x-tokens\"]\n\
             languages = [\"en\"]\ndetects_language = false\n",
            art("x-encoder", "encoder.onnx", ENCODER_SRC),
            art("x-decoder", "decoder.onnx", DECODER),
            art("x-joiner", "joiner.onnx", JOINER),
            art("x-tokens", "tokens.txt", TOKENS),
        );
        Manifest::parse(&text).unwrap()
    }

    /// A conversion folder of `parakeet-tdt-x` whose record matches its files.
    fn conversion(dir: &Path) {
        let outputs: [(&str, &[u8]); 5] = [
            ("encoder.onnx", b"quantised graph"),
            ("encoder.weights", &[7u8; 5000]),
            ("decoder.onnx", DECODER),
            ("joiner.onnx", JOINER),
            ("tokens.txt", TOKENS),
        ];
        let pin = |b: &[u8]| serde_json::json!({ "sha256": sha(b), "size": b.len() });
        let mut out = serde_json::Map::new();
        for (name, bytes) in outputs {
            fs::write(dir.join(name), bytes).unwrap();
            out.insert(name.to_owned(), pin(bytes));
        }
        let record = serde_json::json!({
            "task": TASK,
            "recipe": DEFAULT_RECIPE,
            "quantisation": {
                "format": "QDQ", "activations": "QUInt16", "weights": "QUInt8", "calibration": "MinMax"
            },
            "sources": {
                "encoder.onnx": pin(ENCODER_SRC),
                "decoder.onnx": pin(DECODER),
                "joiner.onnx": pin(JOINER),
                "tokens.txt": pin(TOKENS),
            },
            "outputs": out,
        });
        fs::write(
            dir.join("conversion.json"),
            serde_json::to_vec_pretty(&record).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn qnn_pack_is_byte_identical_run_to_run_and_round_trips() {
        let d = scratch("determinism");
        let src = d.join("src");
        fs::create_dir_all(&src).unwrap();
        conversion(&src);
        let m = manifest();
        let a = pack(&m, "parakeet-tdt-x", &src, &d.join("a")).unwrap();
        let first = fs::read(&a.zip).unwrap();
        // Another folder, another moment: the same bytes.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let b = pack(&m, "parakeet-tdt-x", &src, &d.join("b")).unwrap();
        assert_eq!(first, fs::read(&b.zip).unwrap());
        assert_eq!((a.size, &a.sha256), (b.size, &b.sha256));
        assert_eq!(a.sha256, sha(&first));
        let names: Vec<&str> = a.members.iter().map(|(n, ..)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                NOTICE,
                "conversion.json",
                "tokens.txt",
                "encoder.onnx",
                "encoder.weights",
                "decoder.onnx",
                "joiner.onnx"
            ]
        );
        // Stored entries: every file's bytes are in the zip as they are.
        let weights = vec![7u8; 5000];
        assert!(
            first
                .windows(weights.len())
                .any(|w| w == weights.as_slice())
        );
        // xtask's own reader gives back every file (pack also checks this).
        let into = d.join("x");
        crate::archive::extract(&a.zip, crate::archive::Format::Zip, &into, &a.sha256).unwrap();
        for (name, size, digest) in &a.members {
            let bytes = fs::read(into.join(name)).unwrap();
            assert_eq!(
                (bytes.len() as u64, sha(&bytes)),
                (*size, digest.clone()),
                "{name}"
            );
        }
        assert_eq!(fs::read(into.join("encoder.weights")).unwrap(), weights);
        // No scratch folder or .part is left beside the archive.
        let left: Vec<String> = fs::read_dir(d.join("a"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, [zip_name("parakeet-tdt-x")]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn qnn_pack_notice_credits_nvidia_under_cc_by_4_0_and_says_what_changed() {
        let d = scratch("notice");
        let src = d.join("src");
        fs::create_dir_all(&src).unwrap();
        conversion(&src);
        let m = manifest();
        let p = pack(&m, "parakeet-tdt-x", &src, &d.join("out")).unwrap();
        let into = d.join("x");
        crate::archive::extract(&p.zip, crate::archive::Format::Zip, &into, &p.sha256).unwrap();
        let text = fs::read_to_string(into.join(NOTICE)).unwrap();
        for want in [
            "NVIDIA Parakeet TDT X (NVIDIA)",
            "https://huggingface.co/nvidia/x",
            "Original model: NVIDIA Parakeet TDT X. Original author: NVIDIA. Model card: https://huggingface.co/nvidia/x.",
            "not made, endorsed or supported by NVIDIA",
            "CC-BY-4.0",
            "https://creativecommons.org/licenses/by/4.0/",
            "modified version",
            "`cargo xtask qnn-convert`",
            DEFAULT_RECIPE,
            "the ONNX export by someone, https://huggingface.co/someone/export-tdt at commit",
            "Unchanged from the ONNX export: `decoder.onnx`, `joiner.onnx`, `tokens.txt`.",
            "without warranty",
        ] {
            assert!(text.contains(want), "NOTICE.md lacks {want:?}:\n{text}");
        }
        assert!(!text.contains('\u{2014}'), "no em dashes");
        let entry = p.manifest_entry(&m).unwrap();
        assert!(entry.contains(&format!("url = \"{}\"", release_url("parakeet-tdt-x"))));
        assert!(entry.contains("into = \"parakeet-qnn/parakeet-tdt-x\""));
        assert!(entry.contains("licence = \"CC-BY-4.0\""));
        assert_eq!(entry.matches("[[artefact.extract.member]]").count(), 7);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn qnn_pack_refuses_a_conversion_that_is_not_what_was_measured() {
        let m = manifest();
        fn edit(d: &Path, f: impl FnOnce(&mut serde_json::Value)) {
            let path = d.join("conversion.json");
            let mut v: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            f(&mut v);
            fs::write(&path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
        }
        /// What the error must name, and how the conversion folder is broken.
        type Case<'a> = (&'a str, &'a dyn Fn(&Path));
        let cases: [Case<'_>; 6] = [
            ("differs from conversion.json", &|d| {
                fs::write(d.join("joiner.onnx"), b"JOINER").unwrap();
            }),
            ("encoder.weights", &|d| {
                fs::remove_file(d.join("encoder.weights")).unwrap();
            }),
            ("only T8.3c", &|d| {
                edit(d, |v| v["recipe"] = "a8w8-other".into());
            }),
            ("only T8.3c", &|d| edit(d, |v| v["task"] = "T8.3".into())),
            ("not the manifest's pin", &|d| {
                edit(d, |v| {
                    v["sources"]["encoder.onnx"]["sha256"] = "b".repeat(64).into()
                });
            }),
            ("no pinned file", &|d| {
                edit(d, |v| {
                    let pin = v["sources"]["encoder.onnx"].take();
                    let sources = v["sources"].as_object_mut().unwrap();
                    sources.remove("encoder.onnx");
                    sources.insert("model.onnx".into(), pin);
                });
            }),
        ];
        for (i, (why, break_it)) in cases.iter().enumerate() {
            let d = scratch(&format!("refuse-{i}"));
            let src = d.join("src");
            fs::create_dir_all(&src).unwrap();
            conversion(&src);
            break_it(&src);
            let err = format!(
                "{:#}",
                pack(&m, "parakeet-tdt-x", &src, &d.join("out")).unwrap_err()
            );
            assert!(err.contains(why), "{why}: {err}");
            assert!(
                !d.join("out").join(zip_name("parakeet-tdt-x")).exists(),
                "{why}"
            );
            let _ = fs::remove_dir_all(&d);
        }
        // A model the manifest does not hold.
        let d = scratch("refuse-model");
        conversion(&d);
        assert!(pack(&m, "parakeet-tdt-y", &d, &d.join("out")).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn qnn_pack_names_the_release_that_holds_every_hosted_file() {
        assert_eq!(
            release_url("parakeet-tdt-110m"),
            "https://github.com/pitslug/Earshot/releases/download/cuda-set-llama-0.1.160-2/\
             earshot-npu-parakeet-tdt-110m-a16w8pc-minmax-j2.zip"
        );
        // The CUDA set's own entry points at the same release.
        let m = Manifest::load(&crate::models::manifest_path(&crate::repo_root())).unwrap();
        let cuda_set = m
            .artefacts
            .iter()
            .find(|a| a.id.starts_with("earshot-cuda-set-"))
            .unwrap();
        assert!(
            cuda_set
                .url
                .starts_with(&format!("{REPO_URL}/releases/download/{RELEASE_TAG}/"))
        );
    }
}
