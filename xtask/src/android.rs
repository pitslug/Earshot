//! `cargo xtask android build` (T10.2a, PLAN D26, docs/android-build.md): the Android app's
//! release APK for `arm64-v8a`, through the Tauri CLI in `apps/android` (`tauri android build`),
//! which builds the front end, then `earshot-android` through Gradle's Rust task, then the APK.
//!
//! Tauri's dev-server proxy, the only code that calls its reqwest on Android, is compiled
//! exactly when `tauri/custom-protocol` is off (tauri's build.rs), whatever the profile.
//! `earshot-android` turns that feature on by default, and the net guard's allowance
//! ([`crate::net_guard::ALLOWED_BENEATH`]) rests on it. Gradle may
//! download its distribution (pinned by SHA-256 in `gradle-wrapper.properties`) and its
//! plugins and libraries from Google's Maven and Maven Central, for the build only.
//!
//! Windows needs Developer Mode on (or an administrator shell): the CLI symlinks the built
//! library into the Gradle project.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::build::find_android_ndk;
use crate::proc::run_in_job_with_timeout;

/// Deadline of one APK build: a cold one downloads Gradle and its plugins and compiles Tauri.
const APK_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The pnpm arguments of the release APK build.
#[must_use]
pub fn apk_build_args() -> Vec<&'static str> {
    vec![
        "--filter",
        "@earshot/android",
        "tauri",
        "android",
        "build",
        "--apk",
        "--target",
        "aarch64",
        "--ci",
    ]
}

/// Where Gradle writes the release APK of the Android app (`apps/android`, unsigned).
#[must_use]
pub fn apk_path(root: &Path) -> PathBuf {
    root.join(
        r"apps\android\src-tauri\gen\android\app\build\outputs\apk\universal\release\app-universal-release-unsigned.apk",
    )
}

/// The folder `ANDROID_NDK_HOME`, `ANDROID_NDK_ROOT` or `NDK_HOME` names, else the newest NDK
/// in the SDK.
fn ndk() -> Option<PathBuf> {
    ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT", "NDK_HOME"]
        .into_iter()
        .find_map(|v| find_android_ndk(std::env::var_os(v), None))
        .or_else(|| {
            let sdk =
                std::env::var_os("ANDROID_HOME").or_else(|| std::env::var_os("ANDROID_SDK_ROOT"));
            find_android_ndk(None, sdk)
        })
}

/// Builds the release APK and returns its path.
///
/// # Errors
/// A missing SDK, NDK or Java, a failed build, or no APK where Gradle writes it.
pub fn build(root: &Path) -> Result<PathBuf> {
    if std::env::var_os("ANDROID_HOME").is_none() && std::env::var_os("ANDROID_SDK_ROOT").is_none()
    {
        bail!("ANDROID_HOME is not set (GATE-05; `cargo xtask doctor` checks it)");
    }
    if std::env::var_os("JAVA_HOME").is_none() {
        bail!(
            r"JAVA_HOME is not set: Android Studio's runtime is C:\Program Files\Android\Android Studio\jbr"
        );
    }
    let ndk = ndk().context("no Android NDK (GATE-05; `cargo xtask doctor` checks it)")?;
    let apk = apk_path(root);
    if apk.exists() {
        std::fs::remove_file(&apk).with_context(|| format!("remove the old {}", apk.display()))?;
    }
    let args = apk_build_args();
    let mut cmd = crate::ui::pnpm_command(root, &args);
    // The Tauri CLI and cargo-ndk read either name.
    cmd.env("NDK_HOME", &ndk).env("ANDROID_NDK_HOME", &ndk);
    eprintln!("xtask android: pnpm {}", args.join(" "));
    let out = run_in_job_with_timeout(&mut cmd, APK_TIMEOUT)
        .context("the APK build could not start or did not finish within the deadline")?;
    if !out.success {
        let hint = if out.stderr.contains("symbolic link") || out.stdout.contains("symbolic link") {
            "Windows refused the CLI's symlink: turn on Developer Mode (Settings, System, Advanced, \
             For developers).\n"
        } else {
            ""
        };
        bail!("the APK build failed: {hint}{}", out.describe());
    }
    if !apk.is_file() {
        bail!("the build passed but left no APK at {}", apk.display());
    }
    eprintln!("xtask android: ok, {}", apk.display());
    Ok(apk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_build_is_a_release_apk_for_arm64_only() {
        let args = apk_build_args();
        assert_eq!(
            args[..5],
            ["--filter", "@earshot/android", "tauri", "android", "build"]
        );
        assert!(args.windows(2).any(|w| w == ["--target", "aarch64"]));
        assert!(args.contains(&"--apk"));
        // A release APK; whether Tauri's dev proxy is compiled is the crate's default feature
        // (`android_crate_builds_without_tauris_dev_proxy_by_default`), not the profile.
        assert!(!args.iter().any(|a| *a == "--debug" || *a == "-d"));
        assert!(
            apk_path(Path::new(r"C:\repo"))
                .ends_with(r"release\app-universal-release-unsigned.apk")
        );
    }

    #[test]
    fn android_crate_builds_without_tauris_dev_proxy_by_default() {
        // PLAN D26: Tauri compiles its dev-server proxy (its only reqwest caller on Android)
        // exactly when `tauri/custom-protocol` is off, so the net guard's allowance holds only
        // while every default build of the crate turns it on.
        let file = crate::repo_root().join(r"apps\android\src-tauri\Cargo.toml");
        let manifest: toml::Table = std::fs::read_to_string(&file).unwrap().parse().unwrap();
        let features = manifest["features"].as_table().expect("[features]");
        let list = |name: &str| -> Vec<String> {
            features[name]
                .as_array()
                .unwrap_or_else(|| panic!("feature {name}"))
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(list("custom-protocol"), ["tauri/custom-protocol"]);
        assert!(list("default").contains(&"custom-protocol".to_owned()));
        assert_eq!(
            manifest["package"]["name"].as_str(),
            Some("earshot-android")
        );
    }

    #[test]
    fn android_release_manifest_asks_for_no_internet() {
        let main = crate::repo_root()
            .join(r"apps\android\src-tauri\gen\android\app\src\main\AndroidManifest.xml");
        let text = std::fs::read_to_string(&main).unwrap();
        assert!(text.contains("<application"), "{}", main.display());
        assert!(
            !text.contains("<uses-permission android:name=\"android.permission.INTERNET\""),
            "{} asks for INTERNET in every build",
            main.display()
        );
        // Debug builds keep it, for the dev server.
        let debug = crate::repo_root()
            .join(r"apps\android\src-tauri\gen\android\app\src\debug\AndroidManifest.xml");
        assert!(
            std::fs::read_to_string(&debug)
                .unwrap()
                .contains("android.permission.INTERNET")
        );
    }

    #[test]
    fn android_gradle_wrapper_is_pinned_by_sha256() {
        let file = crate::repo_root()
            .join(r"apps\android\src-tauri\gen\android\gradle\wrapper\gradle-wrapper.properties");
        let text = std::fs::read_to_string(&file).unwrap();
        let sum = text
            .lines()
            .find_map(|l| l.strip_prefix("distributionSha256Sum="))
            .unwrap_or_else(|| panic!("{} has no distributionSha256Sum", file.display()));
        assert_eq!(sum.len(), 64, "{sum}");
        assert!(sum.bytes().all(|b| b.is_ascii_hexdigit()), "{sum}");
        assert!(
            text.contains("distributionUrl=https\\://services.gradle.org/distributions/"),
            "{text}"
        );
    }
}
