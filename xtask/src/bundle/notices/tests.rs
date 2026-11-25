use super::*;

fn tmp(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
        "earshot-bundle-notices-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ids(terms: &[Term]) -> Vec<String> {
    terms.iter().map(Term::text).collect()
}

fn chosen(expr: &str) -> Option<Vec<String>> {
    choose(&parse_licence(expr).unwrap()).map(|t| ids(&t))
}

fn package(dir: &Path, licence: Option<&str>) -> Package {
    Package {
        name: "invented-crate".to_owned(),
        version: "1.2.3".to_owned(),
        licence: licence.map(str::to_owned),
        dir: dir.to_path_buf(),
        url: "https://crates.io/crates/invented-crate/1.2.3".to_owned(),
    }
}

#[test]
fn bundle_notices_read_licence_expressions_as_cargo_and_npm_write_them() {
    assert_eq!(
        parse_licence("MIT").unwrap(),
        Licence::Id("MIT".into(), None)
    );
    let both = Licence::Or(vec![
        Licence::Id("MIT".into(), None),
        Licence::Id("Apache-2.0".into(), None),
    ]);
    // The old slash, with or without spaces, is OR.
    for e in [
        "MIT OR Apache-2.0",
        "MIT/Apache-2.0",
        "MIT / Apache-2.0",
        "(MIT OR Apache-2.0)",
    ] {
        assert_eq!(parse_licence(e).unwrap(), both, "{e}");
    }
    // AND binds tighter than OR.
    assert_eq!(
        parse_licence("MIT OR Apache-2.0 AND ISC").unwrap(),
        Licence::Or(vec![
            Licence::Id("MIT".into(), None),
            Licence::And(vec![
                Licence::Id("Apache-2.0".into(), None),
                Licence::Id("ISC".into(), None),
            ]),
        ])
    );
    assert_eq!(
        parse_licence("Apache-2.0 WITH LLVM-exception").unwrap(),
        Licence::Id("Apache-2.0".into(), Some("LLVM-exception".into()))
    );
    for bad in [
        "",
        "  ",
        "(MIT",
        "MIT OR",
        "AND MIT",
        "MIT)",
        "MIT WITH",
        "MIT OR OR ISC",
    ] {
        assert!(parse_licence(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn bundle_notices_choose_the_least_demanding_allowed_licence() {
    // No holder to name beats one; the first wins a tie.
    assert_eq!(chosen("MIT OR Apache-2.0").unwrap(), ["Apache-2.0"]);
    assert_eq!(chosen("Unlicense OR MIT").unwrap(), ["Unlicense"]);
    // Every part of AND.
    assert_eq!(
        chosen("(MIT OR Apache-2.0) AND Unicode-3.0").unwrap(),
        ["Apache-2.0", "Unicode-3.0"]
    );
    assert_eq!(
        chosen("BSD-3-Clause AND MIT").unwrap(),
        ["BSD-3-Clause", "MIT"]
    );
    // A copyleft alternative is never taken when a permissive one is there.
    assert_eq!(chosen("MIT OR GPL-3.0-only").unwrap(), ["MIT"]);
    assert_eq!(chosen("MPL-2.0 OR MIT").unwrap(), ["MIT"]);
    // File-level copyleft alone is accepted and reported.
    assert_eq!(chosen("MPL-2.0").unwrap(), ["MPL-2.0"]);
    assert_eq!(
        chosen("Apache-2.0 WITH LLVM-exception").unwrap(),
        ["Apache-2.0 WITH LLVM-exception"]
    );
    // Refused: copyleft that reaches the whole, any part of AND outside the lists, unknown ids
    // and exceptions.
    for refused in [
        "GPL-3.0-only",
        "LGPL-2.1-or-later",
        "AGPL-3.0-only",
        "MIT AND GPL-2.0-only",
        "LicenseRef-Proprietary",
        "MIT WITH Some-exception",
        "mit",
    ] {
        assert_eq!(chosen(refused), None, "{refused}");
    }
}

#[test]
fn bundle_notices_every_allowed_licence_is_one_id_and_none_is_copyleft() {
    for id in PERMISSIVE.iter().chain(FILE_COPYLEFT_ACCEPTED) {
        assert_eq!(parse_licence(id).unwrap(), Licence::Id((*id).into(), None));
    }
    for id in PERMISSIVE {
        assert!(
            ![
                "GPL", "LGPL", "AGPL", "MPL", "EPL", "CDDL", "EUPL", "OSL", "CPL"
            ]
            .iter()
            .any(|c| id.to_ascii_uppercase().starts_with(c)),
            "{id}"
        );
    }
    // Every standard text is allowed and whole.
    for (id, text) in CANONICAL {
        assert!(PERMISSIVE.contains(id), "{id}");
        assert!(text.len() > 500, "{id}");
        assert!(!text.contains('\r'), "{id}");
    }
    // The MIT and BSD texts hold no holder of their own.
    for id in ["MIT", "BSD-3-Clause"] {
        assert!(
            !canonical(id)
                .unwrap()
                .to_ascii_lowercase()
                .contains("copyright ("),
            "{id}"
        );
    }
    assert!(canonical("Apache-2.0").unwrap().starts_with(
        "                              Apache License\n                        Version 2.0"
    ));
}

#[test]
fn bundle_notices_use_a_packages_own_licence_files() {
    let dir = tmp("own");
    std::fs::write(
        dir.join("LICENSE-MIT"),
        "Copyright (c) Invented Holder\r\n\r\nText.\r\n",
    )
    .unwrap();
    std::fs::write(dir.join("LICENSE-APACHE"), "Apache text").unwrap();
    std::fs::write(dir.join("license.spdx"), "SPDX metadata").unwrap();
    std::fs::write(dir.join("COPYING"), "").unwrap();
    std::fs::write(dir.join("README.md"), "not a licence").unwrap();
    std::fs::create_dir(dir.join("LICENSES")).unwrap();
    let e = package_entry(&package(&dir, Some("MIT OR Apache-2.0"))).unwrap();
    assert_eq!(
        e.texts,
        ["Apache text", "Copyright (c) Invented Holder\n\nText."]
    );
    assert!(e.notes.is_empty(), "{:?}", e.notes);
    assert!(e.copyleft.is_empty());
    assert_eq!(
        e.line,
        "invented-crate 1.2.3 | MIT OR Apache-2.0 | https://crates.io/crates/invented-crate/1.2.3"
    );
    assert!(
        e.line
            .starts_with(&package_line_start("invented-crate", "1.2.3"))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bundle_notices_use_a_standard_text_and_say_so_when_a_package_ships_none() {
    let dir = tmp("none");
    let e = package_entry(&package(&dir, Some("MIT"))).unwrap();
    assert_eq!(e.texts, [normalise(canonical("MIT").unwrap())]);
    assert!(
        e.notes.iter().any(|n| n.contains("standard text of MIT")),
        "{:?}",
        e.notes
    );
    // No holder is invented.
    assert!(
        e.notes
            .iter()
            .any(|n| n.contains("names its copyright holder")),
        "{:?}",
        e.notes
    );
    // A licence with no holder to name says only the first.
    let e = package_entry(&package(&dir, Some("MIT OR Apache-2.0"))).unwrap();
    assert_eq!(e.texts, [normalise(canonical("Apache-2.0").unwrap())]);
    assert_eq!(e.notes.len(), 1, "{:?}", e.notes);
    // No file and no standard text: fails by name.
    let err = package_entry(&package(&dir, Some("ISC"))).unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("invented-crate 1.2.3") && text.contains("ISC"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bundle_notices_fail_by_name_on_a_missing_or_refused_licence() {
    let dir = tmp("refused");
    std::fs::write(dir.join("LICENSE"), "some text").unwrap();
    for licence in [
        None,
        Some(""),
        Some("  "),
        Some("GPL-3.0-only"),
        Some("MIT AND LGPL-2.1-only"),
    ] {
        let err = package_entry(&package(&dir, licence)).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("invented-crate 1.2.3"), "{licence:?}: {text}");
    }
    // An expression that does not parse names the package too.
    let err = package_entry(&package(&dir, Some("(MIT"))).unwrap_err();
    assert!(format!("{err:#}").contains("invented-crate 1.2.3"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bundle_notices_say_where_file_level_copyleft_source_is() {
    let dir = tmp("mpl");
    std::fs::write(dir.join("LICENSE"), "Mozilla Public License Version 2.0").unwrap();
    let e = package_entry(&package(&dir, Some("MPL-2.0"))).unwrap();
    assert_eq!(e.copyleft, ["MPL-2.0"]);
    assert!(
        e.notes.iter().any(|n| n.contains("MPL-2.0")
            && n.contains("https://crates.io/crates/invented-crate/1.2.3")),
        "{:?}",
        e.notes
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bundle_notices_read_the_shipped_tree_without_the_workspace() {
    let root = Path::new(r"C:\Repo");
    let tree = "earshot-desktop v0.4.2 (C:\\Repo\\apps\\desktop\\src-tauri)|\n\
                earshot-core v0.4.2 (c:\\repo\\crates\\earshot-core)|\n\
                serde v1.0.229|MIT OR Apache-2.0\n\
                toml v1.1.8+spec-1.1.0|MIT OR Apache-2.0\n\
                serde v1.0.229|MIT OR Apache-2.0 (*)\n\
                serde v1.0.229 (*)|MIT OR Apache-2.0\n\
                no-licence v0.1.0|\n\
                \n";
    let crates = parse_tree(tree, root).unwrap();
    assert_eq!(
        crates,
        [
            TreeCrate {
                name: "no-licence".into(),
                version: "0.1.0".into(),
                licence: None
            },
            TreeCrate {
                name: "serde".into(),
                version: "1.0.229".into(),
                licence: Some("MIT OR Apache-2.0".into())
            },
            TreeCrate {
                name: "toml".into(),
                version: "1.1.8+spec-1.1.0".into(),
                licence: Some("MIT OR Apache-2.0".into())
            },
        ]
    );
    // Another source: refused, its licence files are not where the notices look.
    let git = format!("{tree}forked v0.1.0 (https://github.com/x/y#abc)|MIT\n");
    assert!(format!("{:#}", parse_tree(&git, root).unwrap_err()).contains("forked 0.1.0"));
    let outside = format!("{tree}near v0.1.0 (C:\\Repository\\near)|MIT\n");
    assert!(parse_tree(&outside, root).is_err());
    // Not rooted at the app, or empty: refused, never an empty list.
    assert!(parse_tree("serde v1.0.229|MIT\n", root).is_err());
    assert!(parse_tree("", root).is_err());
    assert!(parse_tree("earshot-desktop v0.4.2 (C:\\Repo\\a)|\nserde|MIT\n", root).is_err());
}

#[test]
fn bundle_notices_tree_uses_the_builds_features_from_the_lockfile_offline() {
    let features = vec!["vulkan-summaries".to_owned(), "onnx-cuda".to_owned()];
    let args = tree_args("x86_64-pc-windows-msvc", &features);
    let at = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(at("-p"), "earshot-desktop");
    assert_eq!(at("-e"), "normal,no-proc-macro");
    assert_eq!(at("--target"), "x86_64-pc-windows-msvc");
    let build = crate::build::app_build_args(Some("x86_64-pc-windows-msvc"), &features);
    assert_eq!(
        at("--features"),
        build[build.iter().position(|a| a == "--features").unwrap() + 1]
    );
    assert!(at("--features").contains("tauri/custom-protocol"));
    assert!(args.contains(&"--locked".to_owned()));
    assert!(args.contains(&"--offline".to_owned()));
}

#[test]
fn bundle_notices_find_a_crate_in_the_registry_sources() {
    let home = tmp("home");
    let a = home
        .join("registry")
        .join("src")
        .join("b-index")
        .join("x-1.0.0");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(a.join("Cargo.toml"), "").unwrap();
    // A folder without a manifest is not the crate.
    let stray = home
        .join("registry")
        .join("src")
        .join("a-index")
        .join("x-1.0.0");
    std::fs::create_dir_all(&stray).unwrap();
    assert_eq!(crate_dir(&home, "x", "1.0.0").unwrap(), a);
    let err = crate_dir(&home, "x", "2.0.0").unwrap_err();
    assert!(format!("{err:#}").contains("x 2.0.0"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn bundle_notices_list_every_text_once_and_name_each_package() {
    let a = Entry {
        line: "a 1.0.0 | MIT | u".into(),
        texts: vec!["shared".into(), "own".into()],
        notes: vec!["A note.".into()],
        copyleft: vec![],
    };
    let b = Entry {
        line: "b 2.0.0 | MIT | u".into(),
        texts: vec!["shared".into(), "OWN LICENCE".into()],
        notes: vec![],
        copyleft: vec![],
    };
    let sections = vec![Section {
        heading: "Crates".into(),
        intro: "Intro.".into(),
        entries: vec![a, b],
    }];
    let text = render("# Title", "OWN LICENCE\r\n", &sections);
    assert_eq!(
        text,
        render("# Title", "OWN LICENCE\r\n", &sections),
        "same input, same text"
    );
    assert!(text.starts_with("# Title\n"));
    assert!(
        text.contains("a 1.0.0 | MIT | u\n    Licence texts: 2, 3\n    A note.\n"),
        "{text}"
    );
    // Earshot's own licence is text 1, even when a package repeats it.
    assert!(
        text.contains("b 2.0.0 | MIT | u\n    Licence texts: 2, 1\n"),
        "{text}"
    );
    assert_eq!(text.matches("\nshared\n").count(), 1, "{text}");
    assert!(text.contains("### Text 1\n\nOWN LICENCE\n"));
    assert!(!text.contains("### Text 4"));
    assert!(!text.contains('\r'));
    let expected = vec![
        ("a".to_owned(), "1.0.0".to_owned()),
        ("b".to_owned(), "2.0.0".to_owned()),
        ("c".to_owned(), "3.0.0".to_owned()),
    ];
    assert_eq!(missing_names(&text, &expected), ["c 3.0.0"]);
    // A version that merely starts the same is not the package.
    let near = vec![("a".to_owned(), "1.0".to_owned())];
    assert_eq!(missing_names(&text, &near), ["a 1.0"]);
}

#[test]
fn bundle_notices_hold_earshots_licence_without_its_copyright_line() {
    let licence = "MIT License\r\n\r\nCopyright (c) 2026 Invented Owner\r\n\r\nPermission is \
                   hereby granted.\r\n\r\nTHE SOFTWARE IS PROVIDED \"AS IS\".\r\n";
    let own = own_licence(licence);
    assert_eq!(
        own,
        format!(
            "MIT License\n\nPermission is hereby granted.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\".\
             \n\n{OWN_COPYRIGHT}"
        )
    );
    // The real file keeps its terms whole.
    let real = std::fs::read_to_string(crate::repo_root().join("LICENSE")).unwrap();
    let own = own_licence(&real);
    assert!(!own.contains("Copyright ("), "{own}");
    for line in normalise(&real)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("Copyright"))
    {
        assert!(own.contains(line.trim_end()), "{line}");
    }
}

#[test]
fn bundle_notices_quote_vendored_licences_and_fail_when_one_moves() {
    let dir = tmp("vendored");
    let whisper = dir.join("whisper-rs-sys");
    std::fs::create_dir_all(whisper.join("whisper.cpp")).unwrap();
    std::fs::write(
        whisper.join("whisper.cpp").join("LICENSE"),
        "whisper licence\r\n",
    )
    .unwrap();
    let sqlite = dir.join("libsqlite3-sys");
    std::fs::create_dir_all(sqlite.join("sqlite3")).unwrap();
    std::fs::write(
        sqlite.join("sqlite3").join("sqlite3.c"),
        "/* The author disclaims copyright to this source code. */",
    )
    .unwrap();
    let p = |name: &str, d: &Path| Package {
        name: name.into(),
        version: "1.0.0".into(),
        licence: Some("MIT".into()),
        dir: d.to_path_buf(),
        url: format!("https://crates.io/crates/{name}/1.0.0"),
    };
    let entries =
        vendored_entries(&[p("whisper-rs-sys", &whisper), p("libsqlite3-sys", &sqlite)]).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].texts, ["whisper licence"]);
    assert!(
        entries[0].line.contains("whisper.cpp/LICENSE"),
        "{}",
        entries[0].line
    );
    assert!(entries[1].texts.is_empty());
    assert!(entries[1].notes[0].contains("disclaims copyright"));
    // Nothing for a crate that is not in the tree.
    assert!(vendored_entries(&[]).unwrap().is_empty());
    // Moved or changed: the build stops.
    std::fs::remove_file(whisper.join("whisper.cpp").join("LICENSE")).unwrap();
    assert!(vendored_entries(&[p("whisper-rs-sys", &whisper)]).is_err());
    std::fs::write(sqlite.join("sqlite3").join("sqlite3.c"), "/* MIT */").unwrap();
    assert!(vendored_entries(&[p("libsqlite3-sys", &sqlite)]).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

fn write_json(dir: &Path, json: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("package.json"), json).unwrap();
}

#[test]
fn bundle_notices_read_every_form_of_an_npm_licence() {
    let of = |json: &str| serde_json::from_str::<PackageJson>(json).unwrap().licence();
    assert_eq!(of(r#"{"license": "MIT"}"#).as_deref(), Some("MIT"));
    assert_eq!(
        of(r#"{"license": {"type": "ISC", "url": "u"}}"#).as_deref(),
        Some("ISC")
    );
    assert_eq!(
        of(r#"{"licenses": [{"type": "MIT"}, {"type": "Apache-2.0"}]}"#).as_deref(),
        Some("MIT OR Apache-2.0")
    );
    assert_eq!(of(r"{}"), None);
    assert_eq!(of(r#"{"license": 3}"#), None);
}

#[test]
fn bundle_notices_follow_npm_production_dependencies_as_node_resolves_them() {
    let root = tmp("npm");
    write_json(
        &root.join("apps").join("desktop"),
        r#"{"name": "@earshot/desktop", "dependencies": {"a": "1"},
            "devDependencies": {"dev-only": "1"}}"#,
    );
    write_json(
        &root.join("packages").join("ui"),
        r#"{"name": "@earshot/ui", "dependencies": {"@scope/c": "1"},
            "peerDependencies": {"a": "1", "absent-peer": "1"}}"#,
    );
    let modules = root.join("node_modules");
    write_json(
        &modules.join("a"),
        r#"{"name": "a", "version": "1.0.0", "license": "MIT",
            "dependencies": {"b": "1"}, "optionalDependencies": {"not-installed": "1"}}"#,
    );
    std::fs::write(modules.join("a").join("LICENSE"), "a's licence").unwrap();
    // Nested, as an older layout or a second version puts it: found from `a` first.
    write_json(
        &modules.join("a").join("node_modules").join("b"),
        r#"{"name": "b", "version": "2.0.0", "license": "MIT"}"#,
    );
    write_json(
        &modules.join("b"),
        r#"{"name": "b", "version": "1.0.0", "license": "MIT"}"#,
    );
    write_json(
        &modules.join("@scope").join("c"),
        r#"{"name": "@scope/c", "version": "3.0.0", "license": "Apache-2.0"}"#,
    );
    write_json(
        &modules.join("dev-only"),
        r#"{"name": "dev-only", "version": "1.0.0", "license": "GPL-3.0-only"}"#,
    );
    let found = npm_packages(&root).unwrap();
    let names: Vec<(String, String)> = found
        .iter()
        .map(|p| (p.name.clone(), p.version.clone()))
        .collect();
    assert_eq!(
        names,
        [
            ("@scope/c".to_owned(), "3.0.0".to_owned()),
            ("a".to_owned(), "1.0.0".to_owned()),
            ("b".to_owned(), "2.0.0".to_owned()),
        ]
    );
    assert_eq!(found[1].url, "https://www.npmjs.com/package/a/v/1.0.0");
    // Entries follow the crates' rules: own files, else the standard text and a note.
    let a = package_entry(&found[1]).unwrap();
    assert_eq!(a.texts, ["a's licence"]);
    let b = package_entry(&found[2]).unwrap();
    assert!(
        b.notes
            .iter()
            .any(|n| n.contains("names its copyright holder"))
    );
    // A production dependency that is not installed stops the build.
    write_json(
        &modules.join("@scope").join("c"),
        r#"{"name": "@scope/c", "version": "3.0.0", "license": "Apache-2.0",
            "dependencies": {"gone": "1"}}"#,
    );
    let err = format!("{:#}", npm_packages(&root).unwrap_err());
    assert!(
        err.contains("gone") && err.contains("pnpm install"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bundle_notices_name_the_other_files_from_their_manifests() {
    let root = crate::repo_root();
    let tools = tmp("other");
    std::fs::create_dir_all(tools.join("NSIS")).unwrap();
    std::fs::write(
        tools.join("NSIS").join("COPYING"),
        "Invented NSIS terms\r\n",
    )
    .unwrap();
    let x64 = other_entries(
        &root,
        crate::accept::m7::X64_WINDOWS,
        &["vulkan-summaries".to_owned()],
        &tools,
    )
    .unwrap();
    let lines: Vec<&str> = x64.iter().map(|e| e.line.as_str()).collect();
    let has = |entries: &[Entry], s: &str| entries.iter().any(|e| e.line.contains(s));
    for want in [
        "vcruntime140_1.dll",
        "vulkan-1.dll",
        "uninstall.exe",
        "nsis_tauri_utils.dll",
        "onnxruntime.dll",
    ] {
        assert!(has(&x64, want), "{want}: {lines:?}");
    }
    assert!(!has(&x64, "onnxruntime_providers_qnn"), "{lines:?}");
    let uninstall = x64
        .iter()
        .find(|e| e.line.starts_with("uninstall.exe"))
        .unwrap();
    assert_eq!(uninstall.texts, ["Invented NSIS terms"]);
    let arm = other_entries(
        &root,
        crate::build::ARM64_WINDOWS,
        &["qnn".to_owned()],
        &tools,
    )
    .unwrap();
    assert!(has(&arm, "onnxruntime_providers_qnn"));
    assert!(!has(&arm, "vulkan-1.dll"));
    assert!(!has(&arm, "vcruntime140_1.dll"));
    // Without the placed NSIS there is nothing to quote: the build stops.
    let empty = tmp("other-empty");
    assert!(other_entries(&root, crate::accept::m7::X64_WINDOWS, &[], &empty).is_err());
    let _ = std::fs::remove_dir_all(&tools);
    let _ = std::fs::remove_dir_all(&empty);
}

/// The real shipped tree of this host's architecture: every crate allowed and given a text, every
/// vendored licence found, the same text twice, no path of this machine, every crate named. Needs
/// the release build's crates in [`cargo_home`] (any earlier `cargo xtask build`, or `cargo xtask
/// ci` for the shell's own `CARGO_HOME`).
#[test]
fn bundle_notices_the_real_shipped_tree_passes_and_is_named_whole() {
    let root = crate::repo_root();
    let target = super::super::effective_target(None);
    let features = crate::build::shipped_features(Some(target), &[]);
    // The bundle's placed NSIS stands in: only its COPYING is read.
    let tools = tmp("tools");
    std::fs::create_dir_all(tools.join("NSIS")).unwrap();
    std::fs::write(tools.join("NSIS").join("COPYING"), "Invented NSIS terms").unwrap();
    let (text, copyleft) = notices(&root, target, &features, &tools).unwrap();
    let (again, _) = notices(&root, target, &features, &tools).unwrap();
    assert_eq!(text, again, "the notices are deterministic");
    let names = expected_names(&root, target, &features).unwrap();
    assert!(names.len() > 100, "{}", names.len());
    assert_eq!(missing_names(&text, &names), Vec::<String>::new());
    // The window's packages are among them.
    for npm in [
        "react",
        "react-dom",
        "@fluentui/react-components",
        "@tauri-apps/api",
    ] {
        assert!(names.iter().any(|(n, _)| n == npm), "{npm}");
    }
    assert!(!names.iter().any(|(n, _)| n.starts_with("@earshot/")));
    for file in [
        "uninstall.exe",
        "onnxruntime.dll",
        "nsis_tauri_utils.dll",
        "msvcp140.dll",
    ] {
        assert!(text.contains(file), "{file}");
    }
    assert!(text.contains("Invented NSIS terms"));
    let _ = std::fs::remove_dir_all(&tools);
    for v in VENDORED {
        assert!(
            names.iter().any(|(n, _)| n == v.krate),
            "{} is no longer shipped: drop it from VENDORED",
            v.krate
        );
        assert!(text.contains(v.file), "{}", v.file);
    }
    assert!(text.contains("MIT License"), "Earshot's own licence");
    assert!(text.contains(OWN_COPYRIGHT));
    // What bundle-check's scan of the install will find in it: nothing.
    let found = super::super::paths::scan_bytes(
        Path::new(NOTICES_FILE),
        text.as_bytes(),
        &super::super::paths::Needles::of_this_user().unwrap(),
    );
    assert!(found.is_none(), "{found:?}");
    assert!(!text.contains(&root.display().to_string()));
    println!(
        "{} bytes, {} crates, file-level copyleft: {copyleft:?}",
        text.len(),
        names.len()
    );
    // To read the text: EARSHOT_NOTICES_OUT=<file> cargo test -p xtask bundle_notices_the_real
    if let Some(out) = std::env::var_os("EARSHOT_NOTICES_OUT") {
        std::fs::write(out, &text).unwrap();
    }
}
