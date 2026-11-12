//! The device-local records (`local_state`) the runtime writes: the one registry of their
//! keys (T6.26).
//!
//! Every runtime write of a device-local value goes through [`set`] with a [`LocalKey`],
//! and a `LocalKey` is built only here: [`MeetingRecord::key`] for a record that belongs to
//! one meeting, [`DeviceRecord::key`] for one that belongs to the device. Deleting a meeting
//! erases every key that any [`MeetingRecord`] names for it ([`meeting_of`]), so a new kind
//! of meeting record is swept by construction once it is listed here, and
//! `local_state_is_written_only_through_the_registry` fails for a writer that bypasses it.

use earshot_core::identity::EntityId;
use earshot_store::{Store, StoreError};

/// Declares [`MeetingRecord`] with its key prefix and the position of the meeting id among
/// the key's `/`-separated components after the prefix; `ALL` lists every variant.
macro_rules! meeting_records {
    ($( $(#[$doc:meta])* $name:ident => $prefix:literal, meeting at $pos:literal; )*) => {
        /// A kind of device-local record keyed by a meeting.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum MeetingRecord {
            $( $(#[$doc])* $name, )*
        }

        impl MeetingRecord {
            /// Every kind (generated with the enum, so it cannot miss one).
            pub(crate) const ALL: &[MeetingRecord] = &[$(MeetingRecord::$name),*];

            /// The key prefix, ending in `/`.
            pub(crate) const fn prefix(self) -> &'static str {
                match self {
                    $(MeetingRecord::$name => $prefix,)*
                }
            }

            /// Index of the meeting id among the components after the prefix.
            const fn meeting_at(self) -> usize {
                match self {
                    $(MeetingRecord::$name => $pos,)*
                }
            }
        }
    };
}

meeting_records! {
    /// `live-stretches/<meeting>/<track>`: a live session's raw speech-stretch pieces.
    LiveStretches => "live-stretches/", meeting at 0;
    /// `live-speakers/<meeting>/<track>`: live speaker windows (embeddings, biometric).
    LiveSpeakers => "live-speakers/", meeting at 0;
    /// `echo-locks/<meeting>`: the canceller's lock stretches.
    EchoLocks => "echo-locks/", meeting at 0;
    /// `echo-report/<pass>/<meeting>`: what echo cleaning removed in a pass.
    EchoReport => "echo-report/", meeting at 1;
    /// `pass-backend/<pass>/<meeting>/<track>`: which backend ran a transcript pass.
    PassBackend => "pass-backend/", meeting at 1;
    /// `export-fingerprint/<device>/<meeting>`: the last exported note's fingerprint.
    ExportFingerprint => "export-fingerprint/", meeting at 1;
    /// `speaker-identities/<meeting>`: the final pass's speaker centroids with the names it
    /// gave (biometric, T6.25; format in `earshot_store::identities`).
    SpeakerIdentities => "speaker-identities/", meeting at 0;
    /// `live-summaries/<meeting>`: partial summaries made during the meeting (content,
    /// T8.3j; [`crate::live_summaries`]).
    LiveSummaries => "live-summaries/", meeting at 0;
    /// `summary-handovers/<meeting>`: when and how many characters a summary handed to Claude
    /// Code (T7.4ga) or Codex (T7.4gc) (PLAN.md 3.9; no text; [`crate::claude_code`]).
    SummaryHandovers => "summary-handovers/", meeting at 0;
    /// `audio-retention/<meeting>`: when the meeting's audio retention time started (T7.4k;
    /// format in `earshot_store::encode_retention_clock`, [`crate::retention`]).
    AudioRetention => "audio-retention/", meeting at 0;
    /// `summary-kept-local/<meeting>`: the meeting's summary stays on this computer (T12.13;
    /// presence is the answer, [`crate::keep_local`]).
    KeptLocal => "summary-kept-local/", meeting at 0;
    /// `word-choices/<meeting>`: the user's choices on suggested words (content, T12.15ec;
    /// [`crate::word_choices`]).
    WordChoices => "word-choices/", meeting at 0;
    /// `recovered-recording/<meeting>`: how much of an interrupted recording start-up recovery
    /// saved, until the app has said so once (T9.18; no content, [`crate::recover`]).
    RecoveredRecording => "recovered-recording/", meeting at 0;
}

impl MeetingRecord {
    /// The key of this record for `meeting`: the prefix, then `before`, the meeting id and
    /// `after`, separated by `/`.
    ///
    /// # Panics
    /// If `before` does not have the record's number of leading components, or a component
    /// is empty or contains `/` (a bug in the caller).
    pub(crate) fn key(self, before: &[&str], meeting: EntityId, after: &[&str]) -> LocalKey {
        assert_eq!(before.len(), self.meeting_at(), "{self:?} key layout");
        assert!(
            before
                .iter()
                .chain(after)
                .all(|c| !c.is_empty() && !c.contains('/')),
            "{self:?} key component"
        );
        let mut key = self.prefix().to_owned();
        for c in before {
            key.push_str(c);
            key.push('/');
        }
        key.push_str(&meeting.to_string());
        for c in after {
            key.push('/');
            key.push_str(c);
        }
        debug_assert_eq!(self.meeting_of(&key), Some(meeting));
        LocalKey(key)
    }

    /// The meeting `key` names if it is a key of this record.
    pub(crate) fn meeting_of(self, key: &str) -> Option<EntityId> {
        key.strip_prefix(self.prefix())?
            .split('/')
            .nth(self.meeting_at())?
            .parse()
            .ok()
    }
}

/// The meeting a device-local key belongs to, if it is a key of any [`MeetingRecord`].
#[must_use]
pub(crate) fn meeting_of(key: &str) -> Option<EntityId> {
    MeetingRecord::ALL.iter().find_map(|r| r.meeting_of(key))
}

/// A device-local record that belongs to the device, not to a meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceRecord {
    /// `model-check`: the first-run model check's answer.
    ModelCheck,
    /// `settings/speaker-timing`: when live speaker work runs (T6.13d4).
    SpeakerTiming,
    /// `settings/live-summaries`: whether chunks are summarised during the meeting (T8.3j).
    LiveSummaries,
    /// `settings/transcription-model`: the chosen transcription model (T7.4e).
    TranscriptionModel,
    /// `settings/transcription-part`: where transcription runs (T7.4e).
    TranscriptionPart,
    /// `settings/summary-model`: the chosen local summary model (T7.4e).
    SummaryModel,
    /// `settings/summary-part`: where summaries run (T7.4e).
    SummaryPart,
    /// `settings/meeting-languages`: the meeting languages (T7.4e).
    MeetingLanguages,
    /// `settings/summary-writer`: who writes the summary (T7.4g).
    SummaryWriter,
    /// `settings/summary-provider`: the summary endpoint's provider (T7.4g).
    SummaryProvider,
    /// `settings/summary-endpoint`: the summary endpoint URL (T7.4g).
    SummaryEndpoint,
    /// `settings/summary-endpoint-model`: the summary endpoint's model name (T7.4g).
    SummaryEndpointModel,
    /// `settings/summary-claude-code-model`: the model Claude Code is asked for (T7.4ga).
    SummaryClaudeCodeModel,
    /// `settings/summary-codex-model`: the model Codex is asked for (T7.4gc).
    SummaryCodexModel,
    /// `settings/summary-claude-code-effort`: the effort Claude Code is asked for (T12.18).
    SummaryClaudeCodeEffort,
    /// `settings/audio-retention`: how long recordings are kept (T7.4k).
    AudioRetention,
    /// `settings/obsidian-vault`: the Obsidian vault folder (T7.4h).
    ObsidianVault,
    /// `settings/obsidian-folder`: the subfolder inside the vault (T7.4h).
    ObsidianFolder,
    /// `settings/microphone`: the chosen microphone's endpoint id or `default` (T7.4i).
    Microphone,
    /// `settings/microphone-name`: the chosen microphone's name (T7.4i).
    MicrophoneName,
    /// `settings/output`: the chosen output's endpoint id or `default` (T7.4i).
    Output,
    /// `settings/output-name`: the chosen output's name (T7.4i).
    OutputName,
    /// `settings/capture-mode`: the capture mode a new meeting starts with (T7.4i).
    CaptureMode,
    /// `audio-check`: the audio check's results per microphone + output pair (T7.4j).
    AudioCheck,
    /// `benchmark`: the latest benchmark result (T7.4c; timings, settings and backends, no
    /// content; [`crate::benchmark`]).
    Benchmark,
    /// `settings/summary-style`: the user's summary style (T7.4b).
    SummaryStyle,
    /// `first-run`: whether the first-run wizard is done, and its "never" answers (T9.1d;
    /// [`crate::first_run`]).
    FirstRun,
    /// `settings/cuda-summaries`: whether summaries run on CUDA (T8.4af).
    CudaSummaries,
    /// `settings/cuda-transcription`: whether transcription runs on CUDA (T8.4af).
    CudaTranscription,
    /// `settings/provisional-words`: whether a meeting shows provisional words (T9.1mb).
    Previews,
    /// `settings/update-check`: whether Earshot checks for updates by itself: when it starts
    /// (T9.1b) and on its schedule (T12.22).
    UpdateCheck,
    /// `settings/update-schedule`: how often automatic update checks run (T12.22).
    UpdateSchedule,
    /// `update-last-check`: when the last update check finished (T12.22).
    UpdateLastCheck,
    /// `update-notified`: the last version an update notification was shown for (T12.22).
    UpdateNotified,
    /// `settings/vocabulary`: the user's own words for summaries (T12.15a).
    Vocabulary,
}

impl DeviceRecord {
    /// Every kind.
    #[cfg(test)]
    pub(crate) const ALL: &[DeviceRecord] = &[
        DeviceRecord::ModelCheck,
        DeviceRecord::SpeakerTiming,
        DeviceRecord::LiveSummaries,
        DeviceRecord::TranscriptionModel,
        DeviceRecord::TranscriptionPart,
        DeviceRecord::SummaryModel,
        DeviceRecord::SummaryPart,
        DeviceRecord::MeetingLanguages,
        DeviceRecord::SummaryWriter,
        DeviceRecord::SummaryProvider,
        DeviceRecord::SummaryEndpoint,
        DeviceRecord::SummaryEndpointModel,
        DeviceRecord::SummaryClaudeCodeModel,
        DeviceRecord::SummaryCodexModel,
        DeviceRecord::SummaryClaudeCodeEffort,
        DeviceRecord::AudioRetention,
        DeviceRecord::ObsidianVault,
        DeviceRecord::ObsidianFolder,
        DeviceRecord::Microphone,
        DeviceRecord::MicrophoneName,
        DeviceRecord::Output,
        DeviceRecord::OutputName,
        DeviceRecord::CaptureMode,
        DeviceRecord::AudioCheck,
        DeviceRecord::Benchmark,
        DeviceRecord::SummaryStyle,
        DeviceRecord::FirstRun,
        DeviceRecord::CudaSummaries,
        DeviceRecord::CudaTranscription,
        DeviceRecord::Previews,
        DeviceRecord::UpdateCheck,
        DeviceRecord::UpdateSchedule,
        DeviceRecord::UpdateLastCheck,
        DeviceRecord::UpdateNotified,
        DeviceRecord::Vocabulary,
    ];

    /// Its key.
    pub(crate) fn key(self) -> LocalKey {
        LocalKey(
            match self {
                DeviceRecord::ModelCheck => "model-check",
                DeviceRecord::SpeakerTiming => crate::speaker_timing::SPEAKER_TIMING_KEY,
                DeviceRecord::LiveSummaries => crate::live_summaries::LIVE_SUMMARIES_KEY,
                DeviceRecord::TranscriptionModel => crate::model_settings::TRANSCRIPTION_MODEL_KEY,
                DeviceRecord::TranscriptionPart => crate::model_settings::TRANSCRIPTION_PART_KEY,
                DeviceRecord::SummaryModel => crate::model_settings::SUMMARY_MODEL_KEY,
                DeviceRecord::SummaryPart => crate::model_settings::SUMMARY_PART_KEY,
                DeviceRecord::MeetingLanguages => crate::model_settings::MEETING_LANGUAGES_KEY,
                DeviceRecord::SummaryWriter => crate::summary_settings::SUMMARY_WRITER_KEY,
                DeviceRecord::SummaryProvider => crate::summary_settings::SUMMARY_PROVIDER_KEY,
                DeviceRecord::SummaryEndpoint => crate::summary_settings::SUMMARY_ENDPOINT_KEY,
                DeviceRecord::SummaryEndpointModel => {
                    crate::summary_settings::SUMMARY_ENDPOINT_MODEL_KEY
                }
                DeviceRecord::SummaryClaudeCodeModel => {
                    crate::summary_settings::SUMMARY_CLAUDE_CODE_MODEL_KEY
                }
                DeviceRecord::SummaryCodexModel => crate::summary_settings::SUMMARY_CODEX_MODEL_KEY,
                DeviceRecord::SummaryClaudeCodeEffort => {
                    crate::summary_settings::SUMMARY_CLAUDE_CODE_EFFORT_KEY
                }
                DeviceRecord::AudioRetention => crate::retention::AUDIO_RETENTION_KEY,
                DeviceRecord::ObsidianVault => crate::export_settings::VAULT_KEY,
                DeviceRecord::ObsidianFolder => crate::export_settings::FOLDER_KEY,
                DeviceRecord::Microphone => crate::audio_settings::MICROPHONE_KEY,
                DeviceRecord::MicrophoneName => crate::audio_settings::MICROPHONE_NAME_KEY,
                DeviceRecord::Output => crate::audio_settings::OUTPUT_KEY,
                DeviceRecord::OutputName => crate::audio_settings::OUTPUT_NAME_KEY,
                DeviceRecord::CaptureMode => crate::audio_settings::CAPTURE_MODE_KEY,
                DeviceRecord::AudioCheck => crate::audio_check::AUDIO_CHECK_KEY,
                DeviceRecord::Benchmark => crate::benchmark::BENCHMARK_KEY,
                DeviceRecord::SummaryStyle => crate::summary_style::SUMMARY_STYLE_KEY,
                DeviceRecord::FirstRun => crate::first_run::FIRST_RUN_KEY,
                DeviceRecord::CudaSummaries => crate::cuda_switch::CUDA_SUMMARIES_KEY,
                DeviceRecord::CudaTranscription => crate::cuda_switch::CUDA_TRANSCRIPTION_KEY,
                DeviceRecord::Previews => crate::preview_settings::PREVIEWS_KEY,
                DeviceRecord::UpdateCheck => crate::update::AUTO_CHECK_KEY,
                DeviceRecord::UpdateSchedule => crate::update::SCHEDULE_KEY,
                DeviceRecord::UpdateLastCheck => crate::update::LAST_CHECK_KEY,
                DeviceRecord::UpdateNotified => crate::update::NOTIFIED_KEY,
                DeviceRecord::Vocabulary => crate::vocabulary::VOCABULARY_KEY,
            }
            .to_owned(),
        )
    }
}

/// A device-local key built by the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalKey(String);

impl LocalKey {
    /// The key text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Writes a device-local value under a registry key (the runtime's only writer).
///
/// # Errors
/// Store errors.
pub(crate) fn set(store: &mut Store, key: &LocalKey, value: &[u8]) -> Result<(), StoreError> {
    store.set_local_value(key.as_str(), value)
}

/// [`set`] inside a store transaction (the final pass writes its records with its rows).
///
/// # Errors
/// Store errors.
pub(crate) fn set_tx(
    tx: &mut earshot_store::StoreTx<'_>,
    key: &LocalKey,
    value: &[u8],
) -> Result<(), StoreError> {
    tx.set_local_value(key.as_str(), value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> EntityId {
        EntityId::from_bytes([0x01, 0x90, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, n])
    }

    #[test]
    fn local_keys_name_their_meeting_and_nothing_else() {
        let (m, other) = (id(1), id(2));
        for &r in MeetingRecord::ALL {
            let before: Vec<&str> = (0..r.meeting_at()).map(|_| "x").collect();
            let key = r.key(&before, m, &["mic"]);
            assert_eq!(meeting_of(key.as_str()), Some(m), "{key:?}");
            assert_ne!(meeting_of(key.as_str()), Some(other));
        }
        for &d in DeviceRecord::ALL {
            assert_eq!(meeting_of(d.key().as_str()), None, "{d:?}");
        }
        assert_eq!(meeting_of("device_id"), None);
        assert_eq!(meeting_of("live-stretches/not-an-id/mic"), None);
    }

    #[test]
    fn local_record_prefixes_do_not_overlap() {
        for &a in MeetingRecord::ALL {
            assert!(a.prefix().ends_with('/'));
            for &b in MeetingRecord::ALL {
                if a != b {
                    assert!(!b.prefix().starts_with(a.prefix()), "{a:?} / {b:?}");
                }
            }
            for &d in DeviceRecord::ALL {
                assert!(!d.key().as_str().starts_with(a.prefix()), "{a:?} / {d:?}");
            }
        }
    }

    /// Lines of production code in `src` (items under `#[cfg(test)]` and test-only files
    /// left out).
    fn production_lines(src: &str) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        let mut skipping = false;
        let mut depth: i64 = 0;
        let mut opened = false;
        for (n, line) in src.lines().enumerate() {
            let t = line.trim();
            if !skipping && t == "#[cfg(test)]" {
                skipping = true;
                depth = 0;
                opened = false;
                continue;
            }
            if skipping {
                if t.starts_with("#[") && !opened {
                    continue;
                }
                for c in line.chars() {
                    match c {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                if (opened && depth <= 0) || (!opened && t.ends_with(';')) {
                    skipping = false;
                }
                continue;
            }
            out.push((n + 1, line.to_owned()));
        }
        out
    }

    /// Files of the modules `file` declares out of line (`mod name;`), each with whether the
    /// declaration is under `#[cfg(test)]`. `#[path]` is relative to `file`'s folder;
    /// otherwise children of `lib.rs`, `main.rs` and `mod.rs` sit beside it and children of
    /// `foo.rs` in `foo/`.
    fn child_modules(file: &std::path::Path, src: &str) -> Vec<(std::path::PathBuf, bool)> {
        let dir = file.parent().unwrap();
        let stem = file.file_stem().unwrap().to_string_lossy();
        let base = if matches!(stem.as_ref(), "lib" | "main" | "mod") {
            dir.to_owned()
        } else {
            dir.join(stem.as_ref())
        };
        let mut out = Vec::new();
        let (mut test, mut path) = (false, None::<String>);
        for line in src.lines() {
            let t = line.trim();
            if t == "#[cfg(test)]" {
                test = true;
                continue;
            }
            if let Some(p) = t
                .strip_prefix("#[path = \"")
                .and_then(|r| r.strip_suffix("\"]"))
            {
                path = Some(p.to_owned());
                continue;
            }
            if t.starts_with("#[") || t.starts_with("///") {
                continue;
            }
            let decl = t
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub ")
                .strip_prefix("mod ")
                .and_then(|r| r.strip_suffix(';'));
            if let Some(name) = decl {
                let f = match path.take() {
                    Some(p) => dir.join(p),
                    None if base.join(format!("{name}.rs")).exists() => {
                        base.join(format!("{name}.rs"))
                    }
                    None => base.join(name).join("mod.rs"),
                };
                out.push((f, test));
            }
            test = false;
            path = None;
        }
        out
    }

    /// The production source files of a crate: from `src/lib.rs` and `src/main.rs` through
    /// every module declaration not under `#[cfg(test)]`, so a test-only file is left out by
    /// how it is declared, not by its name.
    fn production_files(krate: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut todo: Vec<std::path::PathBuf> = ["lib.rs", "main.rs"]
            .iter()
            .map(|r| krate.join("src").join(r))
            .filter(|p| p.exists())
            .collect();
        let mut out = Vec::new();
        while let Some(f) = todo.pop() {
            let src =
                std::fs::read_to_string(&f).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
            for (child, test) in child_modules(&f, &src) {
                if !test {
                    todo.push(child);
                }
            }
            out.push(f);
        }
        out
    }

    /// Workspace members (folder names, which are the package names) from the root
    /// manifest.
    fn workspace_members(root: &std::path::Path) -> Vec<String> {
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        let list = manifest
            .split_once("members = [")
            .and_then(|(_, r)| r.split_once(']'))
            .expect("workspace members list")
            .0;
        list.split(',')
            .map(|m| m.trim().trim_matches('"'))
            .filter(|m| !m.is_empty())
            .map(|m| m.rsplit('/').next().unwrap().to_owned())
            .collect()
    }

    /// `net_guard::TEST_SUPPORT_PACKAGES`, read from xtask's source (fails closed).
    fn test_support_packages(root: &std::path::Path) -> Vec<String> {
        let src = std::fs::read_to_string(root.join("xtask/src/net_guard.rs")).unwrap();
        let list = src
            .split_once("pub const TEST_SUPPORT_PACKAGES: &[&str] = &[")
            .and_then(|(_, r)| r.split_once("];"))
            .expect("TEST_SUPPORT_PACKAGES in xtask/src/net_guard.rs")
            .0;
        list.split(',')
            .map(|m| m.trim().trim_matches('"').to_owned())
            .filter(|m| !m.is_empty())
            .collect()
    }

    #[test]
    fn local_state_is_written_only_through_the_registry() {
        // Every shipped workspace crate except the store (which owns `local_state`) and
        // xtask; test-support crates are not shipped.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let skip = test_support_packages(&root);
        assert!(!skip.is_empty());
        let members = workspace_members(&root);
        assert!(members.iter().any(|m| m == "earshot-cli"), "{members:?}");
        let mut files = Vec::new();
        for krate in members
            .iter()
            .filter(|m| !matches!(m.as_str(), "xtask" | "earshot-store") && !skip.contains(m))
        {
            files.extend(production_files(&root.join("crates").join(krate)));
        }
        let mine = std::path::Path::new(file!()).file_name().unwrap();
        assert!(
            files.iter().any(|f| f.file_name() == Some(mine)),
            "the scan reaches this module"
        );
        assert!(
            !files.iter().any(|f| f.ends_with("delete_tests.rs")),
            "test-only files are left out"
        );
        let mut bypass = Vec::new();
        for f in &files {
            if f.file_name() == Some(mine) && f.starts_with(root.join("crates/earshot-runtime")) {
                continue;
            }
            let src = std::fs::read_to_string(f).unwrap();
            for (n, line) in production_lines(&src) {
                if line.contains(concat!("set_local", "_value(")) {
                    bypass.push(format!("{}:{n}: {}", f.display(), line.trim()));
                }
            }
        }
        assert!(
            bypass.is_empty(),
            "write device-local values through local::set with a registry key, so meeting \
             deletion sweeps them:\n{}",
            bypass.join("\n")
        );
    }

    #[test]
    fn local_scan_follows_module_declarations_and_leaves_test_modules_out() {
        let lib = std::path::Path::new("/k/src/lib.rs");
        let src = "#[cfg(test)]\n#[path = \"x_tests.rs\"]\nmod tests;\n/// Docs.\npub mod live;\n#[cfg(windows)]\nmod win;\n#[cfg(test)]\nmod testutil;\nuse a::b;\n";
        let got = child_modules(lib, src);
        let want: Vec<(std::path::PathBuf, bool)> = vec![
            ("/k/src/x_tests.rs".into(), true),
            ("/k/src/live/mod.rs".into(), false),
            ("/k/src/win/mod.rs".into(), false),
            ("/k/src/testutil/mod.rs".into(), true),
        ];
        assert_eq!(got, want);
        // Children of a non-mod file live in its own folder.
        let got = child_modules(std::path::Path::new("/k/src/post.rs"), "mod a;\n");
        assert_eq!(got, vec![("/k/src/post/a/mod.rs".into(), false)]);
    }

    #[test]
    fn local_production_line_filter_skips_test_items_only() {
        let src = "fn a() {}\n#[cfg(test)]\nfn t() {\n    x();\n}\nfn b() {}\n#[cfg(test)]\nmod tests;\nfn c() {}\n";
        let lines: Vec<String> = production_lines(src).into_iter().map(|(_, l)| l).collect();
        assert_eq!(lines, ["fn a() {}", "fn b() {}", "fn c() {}"]);
    }
}
