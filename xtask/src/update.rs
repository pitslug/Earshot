//! The release key and the signed version file of an in-app update (T9.1b, D24).
//!
//! `cargo xtask update-key --out <file>` makes the owner's key once and prints its public half.
//! `cargo xtask sign-update --key <file>` signs the installer `build --bundle` left, and checks the
//! result against the key built into the app, so a file installed copies would refuse never gets
//! published. The private key stays outside the repo. Losing it means installed copies cannot take
//! another update and a new installer has to be installed by hand (docs/updates.md).

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use earshot_update::{Release, from_hex, public_key, sign, to_hex, verify};

use crate::bundle;

/// Refuses a key path inside `root`: everything committed here becomes public (ground rule 9).
fn outside_repo(root: &Path, file: &Path) -> Result<PathBuf> {
    let parent = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = parent
        .canonicalize()
        .with_context(|| format!("the folder of {} does not exist", file.display()))?;
    let root = root.canonicalize().context("the repo root")?;
    ensure!(
        !parent.starts_with(&root),
        "{} is inside the repo: keep the release key outside it",
        file.display()
    );
    Ok(parent.join(file.file_name().context("the key needs a file name")?))
}

/// Writes a new key to `out`, never over an existing file, and returns its public half in hex.
///
/// # Errors
/// A path inside the repo, an existing file, or no random numbers.
pub fn make_key(root: &Path, out: &Path) -> Result<String> {
    let out = outside_repo(root, out)?;
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no random numbers: {e}"))?;
    ensure!(seed != [0u8; 32], "the random numbers look wrong");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&out)
        .with_context(|| format!("{} exists or cannot be made", out.display()))?;
    file.write_all(format!("{}\n", to_hex(&seed)).as_bytes())
        .with_context(|| format!("write {}", out.display()))?;
    Ok(to_hex(&public_key(&seed)))
}

/// The seed in a key file.
fn read_key(root: &Path, file: &Path) -> Result<[u8; 32]> {
    let file = outside_repo(root, file)?;
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("read the release key {}", file.display()))?;
    // Never quote the file: it is the key.
    from_hex::<32>(text.trim()).context("the release key file does not hold a key")
}

/// The version file for an installer's bytes on disk.
fn release_of(installer: &Path, version: &str, arch: &str) -> Result<Release> {
    let size = std::fs::metadata(installer)
        .with_context(|| {
            format!(
                "no {}: run `cargo xtask build --bundle`",
                installer.display()
            )
        })?
        .len();
    Ok(Release {
        product: earshot_update::PRODUCT.to_owned(),
        arch: arch.to_owned(),
        version: version.to_owned(),
        installer: earshot_update::installer_name(version, arch),
        size,
        sha256: crate::download::sha256_file(installer)?,
    })
}

/// Signs `installer` and checks the result against `app_key`, what installed copies will use.
///
/// # Errors
/// A key that is not the app's, or an installer the rules refuse.
pub fn sign_installer(
    installer: &Path,
    version: &str,
    arch: &str,
    seed: &[u8; 32],
    app_key: Option<[u8; 32]>,
) -> Result<Vec<u8>> {
    let Some(app_key) = app_key else {
        bail!("this build has no release key: set RELEASE_PUBLIC_KEY in earshot-update first");
    };
    ensure!(
        public_key(seed) == app_key,
        "this key is not the one built into the app, so installed copies would refuse the file"
    );
    let release = release_of(installer, version, arch)?;
    let file = sign(&release, seed).map_err(|e| anyhow::anyhow!("{e}"))?;
    let back = verify(&file, &app_key, arch).map_err(|e| anyhow::anyhow!("{e}"))?;
    ensure!(back == release, "the signed file does not read back");
    Ok(file)
}

/// `cargo xtask sign-update`: writes the version file beside the installer and prints both, the
/// two files a release needs. Returns the version file's path.
///
/// # Errors
/// As [`sign_installer`], or an unwritable folder.
pub fn run_sign(
    root: &Path,
    key: &Path,
    target: Option<&str>,
    target_dir: Option<&Path>,
) -> Result<PathBuf> {
    let triple = bundle::effective_target(target).to_owned();
    let version = bundle::workspace_version(root)?;
    let dir =
        crate::build::short_target_dir(root, target_dir, std::env::var_os("SystemDrive"), target);
    let release_dir = match target {
        Some(t) if t != crate::accept::host_triple().as_str() => dir.join(t).join("release"),
        _ => dir.join("release"),
    };
    let installer = release_dir
        .join("bundle")
        .join(bundle::installer_path(&version, &triple)?);
    let arch = if triple.starts_with("aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let seed = read_key(root, key)?;
    let file = sign_installer(
        &installer,
        &version,
        arch,
        &seed,
        earshot_update::RELEASE_PUBLIC_KEY,
    )?;
    let out = installer.with_file_name(earshot_update::version_file_name(arch));
    std::fs::write(&out, file).with_context(|| format!("write {}", out.display()))?;
    println!("upload both to release v{version}:");
    println!("  {}", installer.display());
    println!("  {}", out.display());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder outside the repo, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("earshot-update-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SEED: [u8; 32] = [5; 32];

    #[test]
    fn update_key_refuses_the_repo_and_an_existing_file_and_is_usable() {
        let root = crate::repo_root();
        let inside = root.join("target").join("never-a-key.txt");
        let e = make_key(&root, &inside).unwrap_err().to_string();
        assert!(e.contains("inside the repo"), "{e}");
        assert!(!inside.exists());

        let scratch = Scratch::new("key");
        let out = scratch.0.join("key.txt");
        let public = make_key(&root, &out).unwrap();
        let seed = read_key(&root, &out).unwrap();
        assert_eq!(to_hex(&public_key(&seed)), public);
        let kept = std::fs::read(&out).unwrap();
        assert!(make_key(&root, &out).is_err(), "never over a key");
        assert_eq!(std::fs::read(&out).unwrap(), kept);
    }

    #[test]
    fn update_a_key_file_that_is_not_a_key_is_refused_without_quoting_it() {
        let root = crate::repo_root();
        let scratch = Scratch::new("bad-key");
        let out = scratch.0.join("key.txt");
        std::fs::write(&out, "SECRET-LOOKING-TEXT").unwrap();
        let e = format!("{:#}", read_key(&root, &out).unwrap_err());
        assert!(!e.contains("SECRET-LOOKING-TEXT"), "{e}");
    }

    #[test]
    fn update_signing_checks_the_key_against_the_app_and_reads_back() {
        let scratch = Scratch::new("sign");
        let installer = scratch.0.join("installer.exe");
        std::fs::write(&installer, b"installer bytes").unwrap();

        let e = sign_installer(&installer, "1.0.0", "x64", &SEED, None).unwrap_err();
        assert!(e.to_string().contains("no release key"), "{e}");
        let other = public_key(&[6; 32]);
        let e = sign_installer(&installer, "1.0.0", "x64", &SEED, Some(other)).unwrap_err();
        assert!(e.to_string().contains("not the one built into"), "{e}");

        let key = public_key(&SEED);
        let file = sign_installer(&installer, "1.0.0", "x64", &SEED, Some(key)).unwrap();
        let release = verify(&file, &key, "x64").unwrap();
        assert_eq!(release.size, 15);
        assert_eq!(release.installer, "Earshot_1.0.0_x64-setup.exe");
        assert_eq!(
            release.sha256,
            crate::download::sha256_file(&installer).unwrap()
        );

        let missing = scratch.0.join("missing.exe");
        assert!(sign_installer(&missing, "1.0.0", "x64", &SEED, Some(key)).is_err());
    }

    #[test]
    fn update_the_installer_name_is_the_bundlers() {
        for (arch, triple) in [
            ("x64", crate::accept::m7::X64_WINDOWS),
            ("arm64", crate::build::ARM64_WINDOWS),
        ] {
            let built = bundle::installer_path("1.2.3", triple).unwrap();
            assert_eq!(
                built.file_name().unwrap().to_str().unwrap(),
                earshot_update::installer_name("1.2.3", arch)
            );
        }
    }
}
