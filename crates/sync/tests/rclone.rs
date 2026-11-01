//! `RcloneRemote` through the real librclone (host build: `apps/lightcraft-android/scripts/build-librclone.sh host` in the
//! lightcraft-android repo). Set `LIBRCLONE` to the path of `librclone.so`; skipped when it is not set.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Fake, sc, touch};
use lightcraft_cloud_client::home::{Home, HomeConfig};
use lightcraft_cloud_client::{Error, RcloneRemote, Remote};

/// librclone is one Go runtime per process with one config path: tests that point it at their own config take turns.
static LIBRCLONE_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn so() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("LIBRCLONE")?);
    p.is_file().then_some(p)
}

#[test]
fn rclone_remote_lists_gets_and_puts_in_a_local_folder() {
    let _turn = LIBRCLONE_TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(so) = so() else {
        eprintln!("skipped: set LIBRCLONE to a librclone.so");
        return;
    };
    let base = std::env::temp_dir().join(format!("lc-cloud-rclone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let remote_dir = base.join("remote Dir");
    std::fs::create_dir_all(remote_dir.join("mobile/thumbs")).unwrap();
    std::fs::write(remote_dir.join("mobile/manifest.json"), b"{}").unwrap();
    std::fs::write(remote_dir.join("mobile/thumbs/a.jpg"), b"12345").unwrap();
    let conf = base.join("rclone.conf");
    std::fs::write(&conf, "").unwrap();
    let r = RcloneRemote::open(Some(&so), &conf, &remote_dir.to_string_lossy(), None).unwrap();

    let mut top: Vec<_> = r.list("mobile", false).unwrap().into_iter().map(|e| (e.path, e.is_dir, e.size)).collect();
    top.sort();
    assert_eq!(top, vec![("manifest.json".to_string(), false, 2), ("thumbs".to_string(), true, 0)]);
    let mut all: Vec<_> = r.list("", true).unwrap().into_iter().filter(|e| !e.is_dir).map(|e| (e.path, e.size)).collect();
    all.sort();
    assert_eq!(all, vec![("mobile/manifest.json".to_string(), 2), ("mobile/thumbs/a.jpg".to_string(), 5)]);
    assert!(matches!(r.list("nope", false), Err(Error::NotFound(_))));

    let out = base.join("got/manifest.json");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    r.get("mobile/thumbs/a.jpg", &out).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"12345");
    assert!(matches!(r.get("mobile/missing.jpg", &base.join("got/x")), Err(Error::NotFound(_))));

    // a put under a new name, creating the folders
    let up = base.join("up.tmp");
    std::fs::write(&up, b"<xmp/>").unwrap();
    r.put(&up, "inbox/dev/abc-20261026T073320Z-none.xmp").unwrap();
    assert_eq!(std::fs::read(remote_dir.join("inbox/dev/abc-20261026T073320Z-none.xmp")).unwrap(), b"<xmp/>");
    assert_eq!(r.stat("inbox/dev/abc-20261026T073320Z-none.xmp").unwrap().map(|e| e.size), Some(6));
    assert_eq!(r.stat("inbox/dev/none").unwrap(), None);
    // a missing config is reported, not loaded
    assert!(RcloneRemote::open(Some(&so), &base.join("absent.conf"), "x:", None).is_err());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_missing_library_is_a_clear_error() {
    let _turn = LIBRCLONE_TURN.lock().unwrap_or_else(|e| e.into_inner());
    // (a different path than the real one: the process-wide library may already be loaded by the test above, which is fine)
    if RcloneRemote::available(Some(Path::new("/nonexistent/librclone.so"))) {
        return;
    }
    let e = RcloneRemote::open(Some(Path::new("/nonexistent/librclone.so")), Path::new("/x"), "x:", None).err().unwrap();
    let name = if cfg!(windows) { "librclone.dll" } else { "librclone.so" };
    assert!(e.to_string().contains(&format!("{name} not found")), "{e}");
}

#[test]
fn rclone_remote_moves_and_carries_a_whole_publish_and_inbox() {
    let _turn = LIBRCLONE_TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(so) = so() else {
        eprintln!("skipped: set LIBRCLONE to a librclone.so");
        return;
    };
    let base = std::env::temp_dir().join(format!("lc-cloud-rclone-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (src, remote_dir) = (base.join("src"), base.join("remote Dir"));
    std::fs::create_dir_all(&remote_dir).unwrap();
    let conf = base.join("rclone.conf");
    std::fs::write(&conf, "").unwrap();
    let r = RcloneRemote::open(Some(&so), &conf, &remote_dir.to_string_lossy(), None).unwrap();
    // move: server-side, creating folders, replacing; a missing source is NotFound
    std::fs::write(remote_dir.join("a.txt"), b"one").unwrap();
    r.move_file("a.txt", "history/2026-10-31/a.123456.txt").unwrap();
    assert!(!remote_dir.join("a.txt").exists());
    assert_eq!(std::fs::read(remote_dir.join("history/2026-10-31/a.123456.txt")).unwrap(), b"one");
    assert!(matches!(r.move_file("a.txt", "b.txt"), Err(Error::NotFound(_))));

    // the home side over librclone: publish, change, remove, and a phone edit
    touch(&src, "A/one.jpg", b"one", 100);
    touch(&src, "A/two.jpg", b"two", 100);
    touch(&src, "A/one.xmp", &sc().build(), 100);
    let mut home = Home::new(HomeConfig::new(&src, base.join("state"), "rclone-test"), Arc::new(r)).unwrap();
    home.set_logger(|_| {});
    let mut fake = Fake::default();
    let rep = home.sync(&mut fake, false).unwrap();
    assert_eq!((rep.plan.new.len(), rep.originals.copied_new, rep.manifest_uploaded), (2, 3, true));
    assert!(remote_dir.join("mobile/manifest.json").is_file() && remote_dir.join("originals/A/two.jpg").is_file());
    std::fs::remove_file(src.join("A/two.jpg")).unwrap();
    let rep = home.sync(&mut fake, false).unwrap();
    assert_eq!((rep.originals.removed, rep.thumbs.removed, rep.previews.removed), (1, 1, 1));
    assert!(!remote_dir.join("originals/A/two.jpg").exists());
    let hist: Vec<_> = walk(&remote_dir.join("history")).into_iter().filter(|p| p.contains("two.")).collect();
    assert_eq!(hist.len(), 1, "{hist:?}");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(remote_dir.join("mobile/manifest.json")).unwrap()).unwrap();
    let id = manifest["photos"][0]["id"].as_str().unwrap().to_string();
    let base_v = manifest["photos"][0]["sidecarVersion"].as_str().unwrap().to_string();
    let edit = sc().rating(5).build();
    std::fs::create_dir_all(remote_dir.join("inbox/phone")).unwrap();
    std::fs::write(remote_dir.join(format!("inbox/phone/{id}-20990101T000000Z-{base_v}.xmp")), &edit).unwrap();
    let rep = home.sync(&mut fake, false).unwrap();
    assert_eq!(rep.inbox.applied, 1);
    assert_eq!(std::fs::read(src.join("A/one.xmp")).unwrap(), edit);
    assert!(walk(&remote_dir.join("inbox-done")).iter().any(|p| p.ends_with(".xmp")));
    assert!(walk(&remote_dir.join("inbox")).is_empty());
    let _ = std::fs::remove_dir_all(&base);
}

fn walk(d: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        if e.path().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path().to_string_lossy().into_owned());
        }
    }
    out
}

#[test]
fn list_remotes_gives_names_only_and_purge_dir_deletes_a_folder() {
    let _turn = LIBRCLONE_TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(so) = so() else {
        eprintln!("skipped: set LIBRCLONE to a librclone.so");
        return;
    };
    let base = std::env::temp_dir().join(format!("lc-cloud-rclone-names-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let conf = base.join("rclone.conf");
    std::fs::write(&conf, "[zeta]\ntype = local\n\n[alpha]\ntype = crypt\nremote = alpha-base:\npassword = SECRET-TOKEN\n").unwrap();
    let names = lightcraft_cloud_client::list_remotes(Some(&so), &conf).unwrap();
    assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    assert!(lightcraft_cloud_client::list_remotes(Some(&so), &base.join("absent.conf")).is_err());

    let remote_dir = base.join("r");
    std::fs::create_dir_all(remote_dir.join("history/2020-01-01/A")).unwrap();
    std::fs::write(remote_dir.join("history/2020-01-01/A/x.xmp"), b"x").unwrap();
    std::fs::write(remote_dir.join("history/keep.txt"), b"x").unwrap();
    let r = RcloneRemote::open(Some(&so), &conf, &remote_dir.to_string_lossy(), None).unwrap();
    assert!(r.purge_dir("").is_err());
    r.purge_dir("history/2020-01-01").unwrap();
    assert!(!remote_dir.join("history/2020-01-01").exists() && remote_dir.join("history/keep.txt").exists());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_second_config_with_the_same_remote_name_wins_in_the_same_process() {
    let _turn = LIBRCLONE_TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(so) = so() else {
        eprintln!("skipped: set LIBRCLONE to a librclone.so");
        return;
    };
    let base = std::env::temp_dir().join(format!("lc-cloud-rclone-reconnect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for (d, file) in [("one", "from-one.txt"), ("two", "from-two.txt")] {
        std::fs::create_dir_all(base.join(d)).unwrap();
        std::fs::write(base.join(d).join(file), b"x").unwrap();
    }
    // the same remote name in two config files, as after Disconnect and Connect with another folder behind `lightcraft:`
    let conf = |d: &str| {
        let c = base.join(format!("{d}.conf"));
        std::fs::write(&c, format!("[lightcraft]\ntype = alias\nremote = {}\n", base.join(d).display())).unwrap();
        c
    };
    let names = |r: &RcloneRemote| r.list("", false).unwrap().into_iter().map(|e| e.path).collect::<Vec<_>>();
    let one = RcloneRemote::open(Some(&so), &conf("one"), "lightcraft:", None).unwrap();
    assert_eq!(names(&one), vec!["from-one.txt"]);
    let two = RcloneRemote::open(Some(&so), &conf("two"), "lightcraft:", None).unwrap();
    assert_eq!(names(&two), vec!["from-two.txt"]);
    // the same file rewritten to name another folder (not a different file) counts as well
    std::fs::write(base.join("one.conf"), format!("[lightcraft]\ntype = alias\nremote = {}/\n", base.join("two").display())).unwrap();
    let again = RcloneRemote::open(Some(&so), &base.join("one.conf"), "lightcraft:", None).unwrap();
    assert_eq!(names(&again), vec!["from-two.txt"]);
    let _ = std::fs::remove_dir_all(&base);
}

/// A Google Drive remote whose OAuth token has expired is refreshed by the library itself (Go's own HTTP stack, in the DLL on
/// Windows) and the new token is saved back to the config file. The "Google" here is a local server that answers the token request,
/// so nothing real is involved: no account, no secret. (After the refresh rclone goes on to ask the real Drive API with the new fake
/// token, which fails or is refused; that call's result is not looked at.)
#[test]
fn an_expired_oauth_token_is_refreshed_through_the_library_and_saved_to_the_config() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    let _turn = LIBRCLONE_TURN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(so) = so() else {
        eprintln!("skipped: set LIBRCLONE to a librclone.so");
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let (seen, stop) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut c) = conn else { continue };
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                c.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                // (the head, then as much body as Content-Length says)
                loop {
                    let n = c.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        if n == 0 {
                            break;
                        }
                        continue;
                    };
                    let want = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    if body.len() >= want || n == 0 {
                        seen.lock().unwrap().push(format!("{}\n{body}", head.lines().next().unwrap_or("")));
                        break;
                    }
                }
                let reply = r#"{"access_token":"fresh-access-token-from-the-test-server","token_type":"Bearer","refresh_token":"the-refresh-token","expires_in":3600}"#;
                let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        })
    };
    let base = std::env::temp_dir().join(format!("lc-cloud-rclone-oauth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let conf = base.join("rclone.conf");
    let token = r#"{"access_token":"stale-access-token","token_type":"Bearer","refresh_token":"the-refresh-token","expiry":"2001-01-01T00:00:00Z"}"#;
    std::fs::write(
        &conf,
        format!(
            "[gd]\ntype = drive\nclient_id = test-client-id\nclient_secret = test-client-secret\nscope = drive\ntoken = {token}\nauth_url = http://127.0.0.1:{port}/auth\ntoken_url = http://127.0.0.1:{port}/token\n"
        ),
    )
    .unwrap();
    let r = RcloneRemote::open(Some(&so), &conf, "gd:", None).unwrap();
    let _ = r.list("", false); // (fails: there is no Drive behind the fake token; what matters is what the library did first)
    stop.store(true, Ordering::SeqCst);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    server.join().unwrap();

    let seen = seen.lock().unwrap().clone();
    let req = seen.iter().find(|r| r.starts_with("POST /token")).unwrap_or_else(|| panic!("the library never asked the token server: {seen:?}"));
    assert!(req.contains("grant_type=refresh_token") && req.contains("refresh_token=the-refresh-token"), "{req}");
    let saved = std::fs::read_to_string(&conf).unwrap();
    assert!(saved.contains("fresh-access-token-from-the-test-server"), "the refreshed token was not saved to the config file");
    assert!(!saved.contains("stale-access-token"), "the stale token is still in the config file");
    let _ = std::fs::remove_dir_all(&base);
}
