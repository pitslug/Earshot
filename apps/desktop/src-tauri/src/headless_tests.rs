//! Refusals of the hidden replay mode. Every name contains `headless`.

use super::*;

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn full() -> Vec<OsString> {
    args(&[
        "--replay",
        "fixture",
        "--llm-url",
        "http://127.0.0.1:5000/v1",
        "--data",
        "scratch",
        "--vault",
        "vault",
        "--model",
        "base.en",
        "--exit-after-export",
    ])
}

#[test]
fn headless_is_only_asked_for_by_the_replay_flag() {
    assert_eq!(parse(&[]), None);
    assert_eq!(parse(&args(&["--autostart"])), None);
    assert_eq!(parse(&args(&["--exit-after-export"])), None);
    assert!(parse(&args(&["--replay"])).is_some());
}

#[test]
fn headless_reads_every_argument() {
    assert_eq!(
        parse(&full()).unwrap().unwrap(),
        Args {
            replay: "fixture".into(),
            llm_url: Some("http://127.0.0.1:5000/v1".into()),
            llama_dir: None,
            cuda_transcription: false,
            cuda_switch: false,
            cuda_meeting_first: false,
            data: "scratch".into(),
            vault: "vault".into(),
            model: "base.en".into(),
        }
    );
}

#[test]
fn headless_refuses_missing_or_unknown_arguments() {
    for missing in ["--llm-url", "--data", "--vault", "--model"] {
        let mut a = full();
        let i = a.iter().position(|x| x == missing).unwrap();
        a.drain(i..=i + 1);
        let e = parse(&a).unwrap().unwrap_err();
        assert!(e.contains(missing), "{e}");
    }
    let mut a = full();
    a.retain(|x| x != "--exit-after-export");
    assert!(
        parse(&a)
            .unwrap()
            .unwrap_err()
            .contains("--exit-after-export")
    );
    let mut a = full();
    a.push("--other".into());
    assert!(parse(&a).unwrap().unwrap_err().contains("--other"));
    let mut a = full();
    a.push("--model".into());
    assert!(parse(&a).unwrap().unwrap_err().contains("needs a value"));
}

#[test]
fn headless_llm_url_must_be_on_loopback() {
    for good in [
        "http://127.0.0.1:5000/v1",
        "http://localhost:5000/v1",
        "http://127.0.0.1",
        "https://localhost/v1",
    ] {
        assert!(is_loopback_url(good), "{good}");
    }
    for bad in [
        "https://api.anthropic.com/v1",
        "http://127.0.0.1.example.com/v1",
        "http://127.0.0.1@example.com/v1",
        "http://example.com/127.0.0.1",
        "http://10.0.0.2:5000/v1",
        "ftp://127.0.0.1/v1",
        "127.0.0.1:5000",
        "",
    ] {
        assert!(!is_loopback_url(bad), "{bad}");
    }
    let mut a = full();
    let i = a.iter().position(|x| x == "--llm-url").unwrap();
    a[i + 1] = "https://api.anthropic.com/v1".into();
    assert!(parse(&a).unwrap().unwrap_err().contains("loopback"));
}

#[test]
fn headless_never_runs_on_the_apps_own_data() {
    let own = Path::new(r"C:\Users\x\AppData\Local\Earshot");
    assert!(check_scratch(Path::new(r"C:\t\scratch"), Some(own)).is_ok());
    assert!(check_scratch(Path::new(r"C:\t\scratch"), None).is_ok());
    // A sibling whose name only starts the same is not the app's.
    assert!(check_scratch(Path::new(r"C:\Users\x\AppData\Local\Earshot2"), Some(own)).is_ok());
    assert!(check_scratch(own, Some(own)).is_err());
    assert!(check_scratch(Path::new(r"c:\users\X\appdata\local\earshot\"), Some(own)).is_err());
    assert!(check_scratch(Path::new("C:/Users/x/AppData/Local/Earshot"), Some(own)).is_err());
    assert!(
        check_scratch(
            Path::new(r"C:\Users\x\AppData\Local\Earshot\sub"),
            Some(own)
        )
        .is_err(),
        "inside it"
    );
    assert!(
        check_scratch(Path::new(r"C:\Users\x\AppData"), Some(own)).is_err(),
        "holding it"
    );
}

#[test]
fn headless_resolves_relative_spellings_of_the_same_folder() {
    let dir = crate::testutil::TempDir::new("headless-resolved");
    let own = resolved(&dir.0);
    let child = dir.0.join("child");
    std::fs::create_dir_all(&child).unwrap();
    let spelled = child.join("..").join(".");
    assert!(check_scratch(&resolved(&spelled), Some(&own)).is_err());
}

#[test]
fn headless_only_writes_into_an_empty_vault() {
    let dir = crate::testutil::TempDir::new("headless-vault");
    let vault = dir.0.join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    assert!(check_vault(&vault).is_ok());
    std::fs::write(vault.join("note.md"), "mine").unwrap();
    assert!(check_vault(&vault).unwrap_err().contains("empty"));
    assert!(check_vault(&dir.0.join("missing")).is_err());
}

#[test]
fn headless_track_names_are_the_lines_accept_reads() {
    assert_eq!(track_name(LiveTrack::You), "mic");
    assert_eq!(track_name(LiveTrack::Them), "reference");
}

#[test]
fn headless_can_write_the_summary_locally_from_another_folder_of_llama_dlls() {
    let base: Vec<OsString> = [
        "--replay",
        "r",
        "--data",
        "d",
        "--vault",
        "v",
        "--model",
        "m",
        "--exit-after-export",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let with = |extra: &[&str]| {
        let mut a = base.clone();
        a.extend(extra.iter().map(OsString::from));
        parse(&a).unwrap()
    };
    let local = with(&["--local-summary", "--llama-dir", "C:/set"]).unwrap();
    assert_eq!(local.llm_url, None);
    assert_eq!(local.llama_dir, Some(PathBuf::from("C:/set")));
    assert!(!local.cuda_transcription);
    let switch = with(&["--local-summary", "--cuda-switch"]).unwrap();
    assert!(switch.cuda_switch);
    assert!(!switch.cuda_meeting_first);
    assert!(with(&["--local-summary", "--cuda-meeting-first"]).is_err());
    let first = with(&["--local-summary", "--cuda-switch", "--cuda-meeting-first"]).unwrap();
    assert!(first.cuda_meeting_first);
    assert!(with(&["--local-summary", "--cuda-switch", "--cuda-transcription"]).is_err());
    assert!(with(&["--local-summary", "--cuda-switch", "--llama-dir", "x"]).is_err());
    let cuda = with(&["--local-summary", "--cuda-transcription"]).unwrap();
    assert!(cuda.cuda_transcription);
    // A summary needs one writer: never both, and never neither.
    assert!(with(&["--local-summary", "--llm-url", "http://127.0.0.1:1/v1"]).is_err());
    assert!(with(&[]).unwrap_err().contains("--llm-url"));
    // Off loopback stays refused.
    assert!(with(&["--llm-url", "http://example.com/v1"]).is_err());
}
