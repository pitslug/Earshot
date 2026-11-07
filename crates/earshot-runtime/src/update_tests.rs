//! Tests against a loopback server: the rules of T9.1b's accept.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use earshot_core::identity::ManualClock;
use earshot_llm_replay::tcp;
use earshot_update::{public_key, sign};

use super::*;

const SEED: [u8; 32] = [11; 32];
const OTHER_SEED: [u8; 32] = [12; 32];
const INSTALLER: &[u8] = b"pretend this is an installer";

/// What a path answers.
#[derive(Clone)]
enum Reply {
    Ok(Vec<u8>),
    Status(u16),
    Redirect(String),
}

/// A loopback server that records every path asked for.
struct Server {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

fn read_path(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let path = first.split_whitespace().nth(1)?.to_owned();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 || line == "\r\n" {
            return Some(path);
        }
    }
}

fn serve(routes: HashMap<String, Reply>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Some(path) = read_path(&stream) else {
                tcp::lingering_close(
                    stream,
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            };
            log.lock().unwrap().push(path.clone());
            let bytes = match routes.get(&path).cloned().unwrap_or(Reply::Status(404)) {
                Reply::Ok(body) => [
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes(),
                    body,
                ]
                .concat(),
                Reply::Status(code) => {
                    format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .into_bytes()
                }
                Reply::Redirect(to) => format!(
                    "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .into_bytes(),
            };
            tcp::lingering_close(stream, &bytes);
        }
    });
    Server { port, seen }
}

impl Server {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn sha(bytes: &[u8]) -> String {
    earshot_update::to_hex(&Sha256::digest(bytes))
}

fn release(version: &str, installer: &[u8]) -> Release {
    Release {
        product: earshot_update::PRODUCT.into(),
        arch: "x64".into(),
        version: version.into(),
        installer: earshot_update::installer_name(version, "x64"),
        size: installer.len() as u64,
        sha256: sha(installer),
    }
}

fn source(port: u16, key_of: &[u8; 32]) -> UpdateSource {
    let base = format!("http://127.0.0.1:{port}");
    let mut allowlist = Allowlist::new();
    allowlist.allow_url(&Url::parse(&base).unwrap()).unwrap();
    UpdateSource {
        version_url: Url::parse(&format!("{base}/latest/version.json")).unwrap(),
        downloads: format!("{base}/download"),
        allowlist,
        key: public_key(key_of),
        arch: "x64".into(),
    }
}

fn updater(src: UpdateSource) -> Updater {
    Updater::new(src, Arc::new(ManualClock::new(1_000))).unwrap()
}

/// A server that holds `release`, signed with `seed`, and the installer bytes.
fn publish(release: &Release, seed: &[u8; 32], installer: &[u8]) -> Server {
    let mut routes = HashMap::new();
    routes.insert(
        "/latest/version.json".to_owned(),
        Reply::Ok(sign(release, seed).unwrap()),
    );
    routes.insert(
        format!("/download/v{}/{}", release.version, release.installer),
        Reply::Ok(installer.to_vec()),
    );
    serve(routes)
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("earshot-updater-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn offered(result: CheckResult) -> UpdateOffer {
    match result {
        CheckResult::Available(offer) => offer,
        CheckResult::UpToDate => panic!("expected an offer"),
    }
}

#[test]
fn update_a_good_release_is_offered_downloaded_and_verified() {
    let server = publish(&release("0.2.0", INSTALLER), &SEED, INSTALLER);
    let up = updater(source(server.port, &SEED));
    let offer = offered(up.check("0.1.0").unwrap());
    assert_eq!(offer.version(), "0.2.0");
    assert_eq!(offer.size(), INSTALLER.len() as u64);

    let dir = temp("good");
    let mut last = (0, 0);
    let path = up
        .download(&offer, &dir, &CancelToken::new(), &mut |d, t| last = (d, t))
        .unwrap();
    assert_eq!(path, dir.join("Earshot_0.2.0_x64-setup.exe"));
    assert_eq!(std::fs::read(&path).unwrap(), INSTALLER);
    assert_eq!(last, (offer.size(), offer.size()));
    verify_installer(&offer, &path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn update_an_installer_larger_than_the_version_file_limit_still_downloads() {
    // The version file's limit is the client's buffered limit; a download streams past it.
    let big = vec![7u8; usize::try_from(earshot_update::MAX_FILE_BYTES).unwrap() * 5];
    let server = publish(&release("0.2.0", &big), &SEED, &big);
    let up = updater(source(server.port, &SEED));
    let offer = offered(up.check("0.1.0").unwrap());
    let dir = temp("big");
    let path = up
        .download(&offer, &dir, &CancelToken::new(), &mut |_, _| {})
        .unwrap();
    verify_installer(&offer, &path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn update_every_request_of_a_download_is_in_the_audit_log() {
    let server = publish(&release("0.2.0", INSTALLER), &SEED, INSTALLER);
    let up = updater(source(server.port, &SEED));
    let offer = offered(up.check("0.1.0").unwrap());
    let dir = temp("audit");
    up.download(&offer, &dir, &CancelToken::new(), &mut |_, _| {})
        .unwrap();
    assert_eq!(up.audit_log().entries().len(), 2);
    assert_eq!(
        server.seen(),
        [
            "/latest/version.json".to_owned(),
            "/download/v0.2.0/Earshot_0.2.0_x64-setup.exe".to_owned()
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn update_a_check_asks_for_the_version_file_and_nothing_else() {
    let server = publish(&release("0.2.0", INSTALLER), &SEED, INSTALLER);
    let up = updater(source(server.port, &SEED));
    up.check("0.1.0").unwrap();
    assert_eq!(server.seen(), ["/latest/version.json"]);
    let entries = up.audit_log().entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
}

#[test]
fn update_a_bad_signature_is_refused_with_nothing_downloaded() {
    let server = publish(&release("0.2.0", INSTALLER), &OTHER_SEED, INSTALLER);
    let up = updater(source(server.port, &SEED));
    assert!(matches!(
        up.check("0.1.0"),
        Err(UpdateError::Refused(VerifyError::BadSignature))
    ));
    assert_eq!(
        server.seen().len(),
        1,
        "only the version file was asked for"
    );
}

#[test]
fn update_an_older_or_equal_version_is_not_offered() {
    for (latest, running) in [("0.1.0", "0.1.0"), ("0.1.0", "0.2.0"), ("0.9.9", "0.10.0")] {
        let server = publish(&release(latest, INSTALLER), &SEED, INSTALLER);
        let up = updater(source(server.port, &SEED));
        assert_eq!(
            up.check(running).unwrap(),
            CheckResult::UpToDate,
            "{latest} on {running}"
        );
    }
}

#[test]
fn update_a_running_version_with_no_reading_is_offered_nothing() {
    let server = publish(&release("9.0.0", INSTALLER), &SEED, INSTALLER);
    let up = updater(source(server.port, &SEED));
    assert_eq!(up.check("0.1.0-dev").unwrap(), CheckResult::UpToDate);
}

#[test]
fn update_a_changed_installer_on_the_server_is_refused_and_not_kept() {
    // The version file is signed for INSTALLER; the host serves other bytes of the same length.
    let other = b"pretend this is an installeR";
    assert_eq!(other.len(), INSTALLER.len());
    let server = publish(&release("0.2.0", INSTALLER), &SEED, other);
    let up = updater(source(server.port, &SEED));
    let offer = offered(up.check("0.1.0").unwrap());
    let dir = temp("changed");
    let result = up.download(&offer, &dir, &CancelToken::new(), &mut |_, _| {});
    assert!(
        matches!(result, Err(UpdateError::Download(_))),
        "{result:?}"
    );
    assert!(
        !offer.installer_path(&dir).exists(),
        "never renamed into place"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn update_an_installer_changed_on_disk_is_refused_before_it_runs() {
    let server = publish(&release("0.2.0", INSTALLER), &SEED, INSTALLER);
    let up = updater(source(server.port, &SEED));
    let offer = offered(up.check("0.1.0").unwrap());
    let dir = temp("on-disk");
    let path = up
        .download(&offer, &dir, &CancelToken::new(), &mut |_, _| {})
        .unwrap();
    std::fs::write(&path, b"pretend this is an installeR").unwrap();
    assert!(matches!(
        verify_installer(&offer, &path),
        Err(UpdateError::InstallerChanged)
    ));
    std::fs::write(&path, b"short").unwrap();
    assert!(matches!(
        verify_installer(&offer, &path),
        Err(UpdateError::InstallerChanged)
    ));
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        verify_installer(&offer, &path),
        Err(UpdateError::Unreadable(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn update_a_redirect_to_another_host_is_refused() {
    let elsewhere = serve(HashMap::from([(
        "/version.json".to_owned(),
        Reply::Ok(sign(&release("0.2.0", INSTALLER), &SEED).unwrap()),
    )]));
    let server = serve(HashMap::from([(
        "/latest/version.json".to_owned(),
        Reply::Redirect(format!("http://127.0.0.1:{}/version.json", elsewhere.port)),
    )]));
    let up = updater(source(server.port, &SEED));
    assert!(matches!(up.check("0.1.0"), Err(UpdateError::Network(_))));
    assert!(elsewhere.seen().is_empty(), "the other host saw no request");
}

#[test]
fn update_a_release_without_the_file_says_so() {
    let server = serve(HashMap::new());
    let up = updater(source(server.port, &SEED));
    assert!(matches!(up.check("0.1.0"), Err(UpdateError::NotPublished)));
}

#[test]
fn update_another_status_is_an_error_and_offers_nothing() {
    let server = serve(HashMap::from([(
        "/latest/version.json".to_owned(),
        Reply::Status(500),
    )]));
    let up = updater(source(server.port, &SEED));
    assert!(matches!(up.check("0.1.0"), Err(UpdateError::Status(500))));
}

#[test]
fn update_a_huge_version_file_is_not_read() {
    let huge = vec![b'x'; usize::try_from(earshot_update::MAX_FILE_BYTES).unwrap() + 1];
    let server = serve(HashMap::from([(
        "/latest/version.json".to_owned(),
        Reply::Ok(huge),
    )]));
    let up = updater(source(server.port, &SEED));
    assert!(matches!(up.check("0.1.0"), Err(UpdateError::Network(_))));
}

#[test]
fn update_the_real_source_contacts_only_this_repo_and_its_download_host() {
    let src = UpdateSource::releases_with_key(public_key(&SEED)).unwrap();
    assert_eq!(src.version_url.host_str(), Some("github.com"));
    let hop0: Vec<_> = src.allowlist.entries().map(|(h, _)| h.to_owned()).collect();
    assert_eq!(hop0, ["github.com"]);
    let later: Vec<_> = src
        .allowlist
        .redirect_entries()
        .map(|(h, _)| h.to_owned())
        .collect();
    assert_eq!(later, ["release-assets.githubusercontent.com"]);
    assert!(
        src.version_url
            .as_str()
            .starts_with("https://github.com/pitslug/Earshot/releases/latest/download/")
    );
}

#[test]
fn update_this_build_has_a_source_because_it_has_the_key() {
    assert!(UpdateSource::releases().is_ok());
}

#[test]
fn update_the_installer_runs_without_questions_and_restarts_the_app() {
    assert_eq!(installer_args(), ["/P", "/UPDATE", "/R"]);
}

#[test]
fn update_leftovers_go_and_nothing_else_does() {
    let dir = temp("leftovers");
    for name in [
        "Earshot_0.2.0_x64-setup.exe",
        "Earshot_0.2.1_x64-setup.exe.part",
        "notes.txt",
        "other-setup.exe",
    ] {
        std::fs::write(dir.join(name), "x").unwrap();
    }
    remove_leftovers(&dir);
    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, ["notes.txt", "other-setup.exe"]);
    std::fs::remove_file(dir.join("notes.txt")).unwrap();
    std::fs::remove_file(dir.join("other-setup.exe")).unwrap();
    std::fs::write(dir.join("Earshot_1.0.0_x64-setup.exe"), "x").unwrap();
    remove_leftovers(&dir);
    assert!(!dir.exists(), "an empty folder goes too");
    remove_leftovers(&dir);
}

#[test]
fn update_auto_check_is_off_until_turned_on() {
    let data = temp("auto");
    let db = DataDir::new(data.join("Earshot"));
    db.create().unwrap();
    let mut store = Store::open(&db.db_path()).unwrap();
    assert!(!auto_check(&store).unwrap());
    set_auto_check(&mut store, true).unwrap();
    assert!(auto_check(&store).unwrap());
    set_auto_check(&mut store, false).unwrap();
    assert!(!auto_check(&store).unwrap());
    store.set_local_value(AUTO_CHECK_KEY, b"maybe").unwrap();
    assert!(!auto_check(&store).unwrap(), "only `on` means on");
    drop(store);
    let _ = std::fs::remove_dir_all(&data);
}
