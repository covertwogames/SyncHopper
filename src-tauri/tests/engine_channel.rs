#![cfg(unix)]
//! Release builds get the sync engine only from the signed channel (copies
//! of rclone signed by Cover Two, published by .github/workflows/engine.yml).
//! These tests serve a fake channel from a local folder. The lists in
//! tests/fixtures/engine were signed with a throwaway key whose private half
//! was deleted; test-key.pub is its public half.

mod common;
use common::TestHost;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use synchopper_lib::app::{App, EngineCheck};
use synchopper_lib::rclone::{self, EngineChannel};

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/engine");

fn fixture(name: &str) -> PathBuf {
    Path::new(FIX).join(name)
}

fn supported() -> bool {
    // The fixture engines are shell scripts listed for linux-amd64.
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
}

struct Server(std::process::Child, u16);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

async fn serve(site: &Path) -> Server {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let child = std::process::Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "--bind", "127.0.0.1"])
        .current_dir(site)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Server(child, port)
}

/// Publishes one of the signed fixture lists, with every fixture file.
fn publish(site: &Path, list: &str) {
    std::fs::create_dir_all(site).unwrap();
    for e in std::fs::read_dir(FIX).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("rclone") {
            std::fs::copy(e.path(), site.join(&name)).unwrap();
        }
    }
    std::fs::copy(fixture(list), site.join("engine.json")).unwrap();
    std::fs::copy(fixture(&format!("{list}.sig")), site.join("engine.json.sig")).unwrap();
}

struct Setup {
    _dir: tempfile::TempDir,
    site: PathBuf,
    data: PathBuf,
    app: Arc<App>,
    host: Arc<TestHost>,
    _server: Server,
}

async fn setup(list: &str) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    let data = dir.path().join("data");
    publish(&site, list);
    let server = serve(&site).await;
    let host = Arc::new(TestHost::default());
    let app = App::new(data.clone(), dir.path().join("koofr").to_string_lossy().to_string(), true, host.clone());
    app.use_engine_channel(EngineChannel {
        manifest_url: format!("http://127.0.0.1:{}/engine.json", server.1),
        pubkey: std::fs::read_to_string(fixture("test-key.pub")).unwrap(),
    });
    Setup { _dir: dir, site, data, app, host, _server: server }
}

/// A working copy installed the way 0.3.0 and earlier did: no record of
/// where it came from.
fn old_unsigned_copy(data: &Path, version: &str) -> Vec<u8> {
    use std::os::unix::fs::PermissionsExt;
    let exe = rclone::managed_path(data);
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    let body = format!("#!/bin/sh\necho 'rclone v{version}'\n").into_bytes();
    std::fs::write(&exe, &body).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    body
}

fn engine_bytes(data: &Path) -> Vec<u8> {
    std::fs::read(rclone::managed_path(data)).unwrap()
}

fn bin_names(data: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(data.join("bin")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_pc_gets_the_signed_engine_and_its_license() {
    if !supported() {
        return;
    }
    let t = setup("list-1.76.0.json").await;
    t.app.ensure_engine().await;
    let e = t.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.version.as_str(), e.signed), ("ready", "1.76.0", true), "{e:?}");
    assert_eq!(e.exe.as_deref(), Some(rclone::managed_path(&t.data).as_path()));
    assert_eq!(engine_bytes(&t.data), std::fs::read(fixture("rclone-v1.76.0-linux-amd64")).unwrap());
    assert_eq!(
        std::fs::read(t.data.join("bin").join("rclone-COPYING.txt")).unwrap(),
        std::fs::read(fixture("rclone-COPYING.txt")).unwrap()
    );
    let src = rclone::installed_source(&t.data).unwrap();
    assert_eq!((src.source.as_str(), src.version.as_str()), ("signed", "1.76.0"));
    assert!(t.app.core.settings().engine_checked_at > 0);
    assert_eq!(t.app.snapshot()["engine_channel"], true);
    assert_eq!(t.app.snapshot()["engine"]["signed"], true);
    assert_eq!(bin_names(&t.data), vec!["engine-source.json", "rclone", "rclone-COPYING.txt"]);
    assert!(t.host.notes.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_unsigned_copy_is_replaced_even_with_updates_turned_off() {
    if !supported() {
        return;
    }
    let t = setup("list-1.76.0.json").await;
    t.app.core.update(|s| s.auto_update_engine = false);
    // Same version as the signed one: it is replaced anyway, because only
    // the signed copy is wanted.
    old_unsigned_copy(&t.data, "1.76.0");
    t.app.ensure_engine().await;
    let e = t.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.version.as_str(), e.signed), ("ready", "1.76.0", true), "{e:?}");
    assert_eq!(engine_bytes(&t.data), std::fs::read(fixture("rclone-v1.76.0-linux-amd64")).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_list_with_a_bad_signature_is_ignored() {
    if !supported() {
        return;
    }
    let t = setup("list-1.77.0.json").await;
    // Someone changed the list after it was signed.
    let list = std::fs::read_to_string(t.site.join("engine.json")).unwrap();
    std::fs::write(t.site.join("engine.json"), list.replace("1.77.0\"", "1.99.0\"")).unwrap();

    // Nothing installed yet: no engine, and the reason is shown.
    t.app.ensure_engine().await;
    let e = t.app.core.engine_info();
    assert_eq!(e.state, "error", "{e:?}");
    assert!(e.message.contains("signature"), "{}", e.message);
    assert!(e.exe.is_none());
    assert!(!rclone::managed_path(&t.data).exists());

    // An older copy that works here keeps being used until a signed one
    // can be had, and is left exactly as it was.
    let old = old_unsigned_copy(&t.data, "1.75.0");
    t.app.check_engine(EngineCheck::Manual).await;
    let e = t.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.version.as_str(), e.signed), ("ready", "1.75.0", false), "{e:?}");
    assert_eq!(engine_bytes(&t.data), old);
    // Not the signed copy, so the regular check keeps trying for it, and
    // records nothing as checked.
    assert_eq!(t.app.core.settings().engine_checked_at, 0);
    assert_eq!(t.app.snapshot()["engine"]["signed"], false);

    // A missing signature is no better.
    std::fs::remove_file(t.site.join("engine.json.sig")).unwrap();
    t.app.check_engine(EngineCheck::Background).await;
    assert_eq!(engine_bytes(&t.data), old);
    assert_eq!(t.app.core.engine_info().state, "ready");

    // Once a properly signed list is up, the signed copy replaces it.
    publish(&t.site, "list-1.77.0.json");
    t.app.check_engine(EngineCheck::Background).await;
    let e = t.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.version.as_str(), e.signed), ("ready", "1.77.0", true), "{e:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_does_not_match_the_list_is_thrown_away() {
    if !supported() {
        return;
    }
    let t = setup("list-bad-checksum.json").await;
    let old = old_unsigned_copy(&t.data, "1.75.0");
    t.app.ensure_engine().await;
    let e = t.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.signed), ("ready", false), "{e:?}");
    assert_eq!(engine_bytes(&t.data), old);
    assert_eq!(bin_names(&t.data), vec!["rclone"]);
    assert!(rclone::installed_source(&t.data).is_none());

    // The program is bigger than the list says.
    let t = setup("list-1.77.0.json").await;
    let name = "rclone-v1.77.0-linux-amd64";
    let mut body = std::fs::read(t.site.join(name)).unwrap();
    body.extend_from_slice(b"# extra\n");
    std::fs::write(t.site.join(name), body).unwrap();
    t.app.ensure_engine().await;
    let e = t.app.core.engine_info();
    assert_eq!(e.state, "error", "{e:?}");
    assert!(e.message.contains("thrown away"), "{}", e.message);
    assert!(!rclone::managed_path(&t.data).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn file_names_that_point_elsewhere_are_refused() {
    if !supported() {
        return;
    }
    let t = setup("list-bad-name.json").await;
    t.app.ensure_engine().await;
    let e = t.app.core.engine_info();
    assert_eq!(e.state, "error", "{e:?}");
    assert!(e.message.contains("file name"), "{}", e.message);
    assert!(!rclone::managed_path(&t.data).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_published_for_this_kind_of_pc() {
    if !supported() {
        return;
    }
    let t = setup("list-other-pc.json").await;
    t.app.ensure_engine().await;
    let e = t.app.core.engine_info();
    assert_eq!(e.state, "error", "{e:?}");
    assert!(e.message.contains("this kind of PC"), "{}", e.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn newer_signed_engines_arrive_without_an_app_update() {
    if !supported() {
        return;
    }
    let t = setup("list-1.76.0.json").await;
    t.app.ensure_engine().await;
    assert_eq!(t.app.core.engine_info().version, "1.76.0");

    // A newer one is published. Checked at most once a day.
    publish(&t.site, "list-1.77.0.json");
    t.app.check_engine(EngineCheck::Background).await;
    assert_eq!(t.app.core.engine_info().version, "1.76.0");

    // With the update setting off, a signed copy is left alone.
    t.app.core.update(|s| {
        s.engine_checked_at = 0;
        s.auto_update_engine = false;
    });
    t.app.check_engine(EngineCheck::Background).await;
    assert_eq!(t.app.core.engine_info().version, "1.76.0");

    // A day later, with the setting on: updated while running.
    t.app.core.update(|s| s.auto_update_engine = true);
    t.app.check_engine(EngineCheck::Background).await;
    let e = t.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.version.as_str(), e.signed), ("ready", "1.77.0", true), "{e:?}");
    assert_eq!(engine_bytes(&t.data), std::fs::read(fixture("rclone-v1.77.0-linux-amd64")).unwrap());
    assert_eq!(bin_names(&t.data), vec!["engine-source.json", "rclone", "rclone-COPYING.txt"]);
    assert!(t.app.core.settings().engine_checked_at > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn never_goes_back_to_an_older_engine() {
    if !supported() {
        return;
    }
    let t = setup("list-1.76.0.json").await;
    t.app.ensure_engine().await;
    publish(&t.site, "list-1.70.0.json");
    t.app.core.update(|s| s.engine_checked_at = 0);
    t.app.check_engine(EngineCheck::Background).await;
    let e = t.app.core.engine_info();
    assert_eq!((e.version.as_str(), e.signed), ("1.76.0", true), "{e:?}");
    assert_eq!(engine_bytes(&t.data), std::fs::read(fixture("rclone-v1.76.0-linux-amd64")).unwrap());
    assert!(t.app.core.settings().engine_checked_at > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_damaged_signed_copy_is_fetched_again() {
    if !supported() {
        return;
    }
    let t = setup("list-1.76.0.json").await;
    t.app.ensure_engine().await;
    // Changed on disk (for example by a disk error or other software).
    std::fs::write(rclone::managed_path(&t.data), b"#!/bin/sh\necho 'rclone v1.76.0'\n# changed\n").unwrap();
    t.app.ensure_engine().await;
    let e = t.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.signed), ("ready", true), "{e:?}");
    assert_eq!(engine_bytes(&t.data), std::fs::read(fixture("rclone-v1.76.0-linux-amd64")).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_channel_being_down_does_not_stop_a_signed_copy() {
    if !supported() {
        return;
    }
    let t = setup("list-1.76.0.json").await;
    t.app.ensure_engine().await;
    std::fs::remove_file(t.site.join("engine.json")).unwrap();
    t.app.core.update(|s| s.engine_checked_at = 0);
    // Startup, with an update check due and the list not reachable.
    t.app.ensure_engine().await;
    let e = t.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.version.as_str(), e.signed), ("ready", "1.76.0", true), "{e:?}");
    // Tried, so the regular check waits a while before trying again.
    assert_eq!(t.app.core.settings().engine_checked_at, 0);
}
