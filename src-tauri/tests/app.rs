//! Tests of the app layer: setup, settings, uploads and the engine download.

mod common;
use common::*;
use std::sync::Arc;
use synchopper_lib::app::{App, SetupChoices};
use synchopper_lib::engine::Phase;

fn choices(rig: &Rig, folders: &[&str], listed: &[&str]) -> SetupChoices {
    SetupChoices {
        feature_sync: true,
        feature_drive: false,
        autostart: true,
        base_folder: rig.pc("Koofr").to_string_lossy().to_string(),
        folders: folders.iter().map(|s| s.to_string()).collect(),
        listed: listed.iter().map(|s| s.to_string()).collect(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn connecting_in_test_mode_reports_space() {
    let rig = Rig::new();
    let about = rig.app.connect("someone@example.com", "unused").await.unwrap();
    assert!(about.total.unwrap_or(0) > 0);
    assert_eq!(rig.app.core.settings().account_email, "someone@example.com");
    let snap = rig.app.snapshot();
    assert_eq!(snap["account"]["email"], "someone@example.com");
    assert!(snap["account"]["total"].as_u64().unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_real_connection_saves_nothing() {
    // Not in test mode, so this goes through the real path: scramble the
    // password, write a pending config, ask Koofr. It must fail (wrong
    // password, or no route to Koofr from here) and leave nothing behind.
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost::default());
    let app = App::new(dir.path().join("data"), "koofr:".into(), false, host);
    app.core.set_engine(|e| e.exe = Some("rclone".into()));
    assert!(app.connect("", "x").await.unwrap_err().contains("email"));
    assert!(app.connect("a@b.c", " ").await.unwrap_err().contains("app password"));
    assert!(app.connect("a@b.c\n[evil]", "x").await.is_err());
    let err = app.connect("nobody@example.com", "definitely-wrong").await.unwrap_err();
    assert!(!err.is_empty());
    assert!(!err.contains("definitely-wrong"), "the password must never be echoed: {err}");
    assert!(!app.core.config_path().exists());
    assert!(!dir.path().join("data").join("rclone.conf.pending").exists());
    assert!(app.core.settings().account_email.is_empty());
    let log = std::fs::read_to_string(app.core.log_path()).unwrap_or_default();
    assert!(!log.contains("definitely-wrong"));
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_creates_the_chosen_folders_and_rerunning_it_adjusts_them() {
    let rig = Rig::new();
    write(&rig.koofr("Documents/doc.txt"), "d");
    write(&rig.koofr("Pictures/pic.jpg"), "p");
    write(&rig.koofr("Videos/big.mkv"), "v");
    assert_eq!(rig.app.list_remote("").await.unwrap(), vec!["Documents", "Pictures", "Videos"]);
    rig.app.core.update(|s| s.setup_complete = false);

    let notes = rig.app.finish_setup(choices(&rig, &["Documents", "Pictures"], &["Documents", "Pictures", "Videos"])).await.unwrap();
    assert!(notes.is_empty(), "{notes:?}");
    let s = rig.app.core.settings();
    assert!(s.setup_complete);
    assert_eq!(s.pairs.len(), 2);
    let app = rig.app.clone();
    wait_for("first syncs", 60, move || app.engine.pair_views().iter().all(|p| p.phase == Phase::Ok)).await;
    assert_eq!(read(&rig.pc("Koofr/Documents/doc.txt")), "d");
    assert_eq!(read(&rig.pc("Koofr/Pictures/pic.jpg")), "p");
    assert!(!rig.pc("Koofr/Videos").exists(), "unticked folders stay online only");

    // A folder added by hand somewhere else.
    let custom = rig.pc("Elsewhere").to_string_lossy().to_string();
    rig.app.engine.add_pair(&custom, "Videos/Sub").unwrap();

    // Run setup again: drop Pictures, keep Documents.
    rig.app.finish_setup(choices(&rig, &["Documents"], &["Documents", "Pictures", "Videos"])).await.unwrap();
    let remotes: Vec<String> = rig.app.core.settings().pairs.iter().map(|p| p.remote.clone()).collect();
    assert_eq!(remotes, vec!["Documents".to_string(), "Videos/Sub".to_string()]);
    assert!(rig.pc("Koofr/Pictures/pic.jpg").exists(), "files stay on the PC when a folder stops syncing");
    assert!(rig.koofr("Pictures/pic.jpg").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_needs_an_account_and_a_place_to_keep_folders() {
    let rig = Rig::new();
    let mut c = choices(&rig, &["Documents"], &["Documents"]);
    c.base_folder = "".into();
    assert!(rig.app.finish_setup(c).await.unwrap_err().contains("where to keep"));
    rig.app.core.update(|s| s.account_email.clear());
    assert!(rig.app.finish_setup(choices(&rig, &[], &[])).await.unwrap_err().contains("Connect"));
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_changes_are_limited_cleaned_and_restorable() {
    let rig = Rig::new();
    let patch = serde_json::json!({
        "use_http2": true,
        "transfers": 4,
        "check_interval_min": 30,
        "upload_limit_mbps": 2.5,
        "conflict_policy": "newest",
        "notify_level": "none",
        "pause_on_battery": true,
        "account_email": "attacker@example.com",
        "setup_complete": false,
        "pairs": [{"id": "x", "local": "/", "remote": ""}],
    });
    rig.app.update_settings(patch).await.unwrap();
    let s = rig.app.core.settings();
    assert!(s.use_http2 && s.pause_on_battery);
    assert_eq!((s.transfers, s.check_interval_min), (4, 30));
    assert_eq!(s.bwlimit().as_deref(), Some("2560k:off"));
    assert_eq!(s.account_email, "tester@example.com", "the screens cannot change the account this way");
    assert!(s.setup_complete && s.pairs.is_empty());

    rig.app.update_settings(serde_json::json!({ "transfers": 999, "notify_level": "loud" })).await.unwrap();
    let s = rig.app.core.settings();
    assert_eq!(s.transfers, 16);
    assert_eq!(s.notify_level, "errors");
    assert!(rig.app.update_settings(serde_json::json!({ "transfers": "many" })).await.is_err());

    assert!(rig.app.core.settings().auto_add_folders, "new folders sync by themselves unless turned off");
    rig.app.update_settings(serde_json::json!({ "feature_sidebar": false, "auto_update_app": false, "auto_add_folders": false })).await.unwrap();
    assert!(!rig.app.core.settings().feature_sidebar && !rig.app.core.settings().auto_update_app);
    assert!(!rig.app.core.settings().auto_add_folders);

    rig.app.restore_defaults().await.unwrap();
    let s = rig.app.core.settings();
    assert!(s.auto_update_app, "update checks are an advanced setting, so restore defaults turns them back on");
    assert!(!s.feature_sidebar, "the sidebar entry is a feature, not an advanced setting");
    assert!(!s.pause_on_battery && s.pause_on_metered);
    assert!(s.use_http2, "how one account talks to its service is that account's setting, not a general one");
    assert_eq!(s.conflict_policy, "keep_both");
    assert_eq!(s.upload_limit_mbps, 0.0);
    assert_eq!(s.check_interval_min, 30, "basic sync options are not part of restore defaults");
    assert!(!s.auto_add_folders, "neither is syncing new folders by themselves");

    // Saved to disk, so it survives a restart.
    let saved = synchopper_lib::config::open(&rig.app.core.data.join("settings.json")).store.view("koofr");
    assert_eq!(saved.check_interval_min, 30);
    assert_eq!(saved.account_email, "tester@example.com");
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_go_straight_to_koofr() {
    let rig = Rig::new();
    write(&rig.pc("loose/video1.mkv"), "one");
    write(&rig.pc("loose/video2.mkv"), "two");
    write(&rig.pc("album/2024/a.jpg"), "a");
    write(&rig.pc("album/2025/b.jpg"), "b");
    std::fs::create_dir_all(rig.pc("album/empty")).unwrap();
    let paths = vec![
        rig.pc("loose/video1.mkv").to_string_lossy().to_string(),
        rig.pc("loose/video2.mkv").to_string_lossy().to_string(),
        rig.pc("album").to_string_lossy().to_string(),
        rig.pc("does-not-exist.bin").to_string_lossy().to_string(),
    ];
    assert_eq!(rig.app.uploads.add(paths.clone(), "/Inbox/Family/"), 3);
    assert_eq!(rig.app.uploads.add(paths[..1].to_vec(), "Inbox/Family"), 0, "an upload already in the list is not added twice");
    let app = rig.app.clone();
    wait_for("uploads to finish", 60, move || app.uploads.active() == 0).await;
    let views = rig.app.uploads.views();
    assert!(views.iter().all(|v| v.state == "done"), "{:?}", views.iter().map(|v| (&v.name, &v.state, &v.error)).collect::<Vec<_>>());
    assert_eq!(read(&rig.koofr("Inbox/Family/video1.mkv")), "one");
    assert_eq!(read(&rig.koofr("Inbox/Family/video2.mkv")), "two");
    assert_eq!(read(&rig.koofr("Inbox/Family/album/2024/a.jpg")), "a");
    assert_eq!(read(&rig.koofr("Inbox/Family/album/2025/b.jpg")), "b");
    assert!(rig.koofr("Inbox/Family/album/empty").is_dir());
    assert!(rig.pc("loose/video1.mkv").exists(), "uploading never removes the original");
    assert!(rig.host.notes.lock().unwrap().iter().any(|(t, b)| t == "Uploads finished" && b.contains("3 uploads")));

    // To the top of the account.
    write(&rig.pc("loose/top.txt"), "top");
    rig.app.uploads.add(vec![rig.pc("loose/top.txt").to_string_lossy().to_string()], "");
    let app = rig.app.clone();
    wait_for("upload to finish", 60, move || app.uploads.active() == 0).await;
    assert_eq!(read(&rig.koofr("top.txt")), "top");

    rig.app.uploads.clear_finished();
    assert!(rig.app.uploads.views().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_upload_says_so_and_can_be_retried() {
    let rig = Rig::new();
    write(&rig.pc("loose/file.txt"), "f");
    // A file sits where the destination folder should be.
    write(&rig.koofr("Blocked"), "i am a file");
    rig.app.uploads.add(vec![rig.pc("loose/file.txt").to_string_lossy().to_string()], "Blocked/Sub");
    let app = rig.app.clone();
    wait_for("upload to fail", 90, move || app.uploads.active() == 0).await;
    let v = &rig.app.uploads.views()[0];
    assert_eq!(v.state, "failed");
    assert!(v.error.as_deref().unwrap_or("").len() > 3);
    assert!(rig.host.notes.lock().unwrap().iter().any(|(t, _)| t.contains("failed")));

    std::fs::remove_file(rig.koofr("Blocked")).unwrap();
    rig.app.uploads.retry(v.id);
    let app = rig.app.clone();
    wait_for("retry to finish", 60, move || app.uploads.active() == 0).await;
    assert_eq!(rig.app.uploads.views()[0].state, "done");
    assert_eq!(read(&rig.koofr("Blocked/Sub/file.txt")), "f");
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_can_be_cancelled() {
    let rig = Rig::new();
    rig.app.core.update(|s| {
        s.upload_limit_mbps = 0.5;
        s.download_limit_mbps = 0.5;
        s.transfers = 1;
    });
    std::fs::create_dir_all(rig.pc("loose")).unwrap();
    std::fs::write(rig.pc("loose/big1.bin"), vec![1u8; 8_000_000]).unwrap();
    std::fs::write(rig.pc("loose/big2.bin"), vec![1u8; 8_000_000]).unwrap();
    rig.app.uploads.add(
        vec![rig.pc("loose/big1.bin").to_string_lossy().to_string(), rig.pc("loose/big2.bin").to_string_lossy().to_string()],
        "Up",
    );
    let app = rig.app.clone();
    wait_for("progress to show", 30, move || app.uploads.views().iter().any(|v| v.state == "uploading" && v.percent > 0.0)).await;
    let states: Vec<String> = rig.app.uploads.views().iter().map(|v| v.state.clone()).collect();
    assert_eq!(states, vec!["uploading", "waiting"], "one at a time when set to 1");
    rig.app.uploads.cancel_all();
    let app = rig.app.clone();
    wait_for("uploads to stop", 30, move || app.uploads.active() == 0).await;
    assert!(rig.app.uploads.views().iter().all(|v| v.state == "cancelled"));
    assert!(!rig.koofr("Up/big1.bin").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn right_click_arguments_queue_uploads() {
    let rig = Rig::new();
    write(&rig.pc("loose/sent.txt"), "sent");
    rig.app.set_upload_dest("From PC");
    let args = vec!["SyncHopper.exe".to_string(), "--upload".to_string(), rig.pc("loose/sent.txt").to_string_lossy().to_string()];
    assert!(rig.app.handle_args(&args));
    assert_eq!(rig.host.shown.lock().unwrap().last().cloned().flatten().as_deref(), Some("upload"));
    let app = rig.app.clone();
    wait_for("upload to finish", 60, move || app.uploads.active() == 0).await;
    assert_eq!(read(&rig.koofr("From PC/sent.txt")), "sent");
    assert!(!rig.app.handle_args(&["SyncHopper.exe".to_string(), "--minimized".to_string()]));

    // Before setup is done nothing is uploaded; the window just opens.
    rig.app.core.update(|s| s.setup_complete = false);
    assert!(!rig.app.handle_args(&args));
    assert_eq!(rig.host.shown.lock().unwrap().last().cloned().flatten(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn previews_count_both_sides() {
    let rig = Rig::new();
    write(&rig.koofr("Docs/a.txt"), "12345");
    write(&rig.koofr("Docs/sub/b.txt"), "123");
    write(&rig.pc("Docs/local.txt"), "1234567");
    let p = rig.app.preview(&rig.pc("Docs").to_string_lossy(), "Docs").await.unwrap();
    assert_eq!((p.remote_count, p.remote_bytes), (2, 8));
    assert_eq!((p.local_count, p.local_bytes), (1, 7));
    assert!(p.free_bytes.unwrap_or(0) > 0);
    // A PC folder that does not exist yet, and a Koofr folder that does not either.
    let p = rig.app.preview(&rig.pc("Not/Yet/Here").to_string_lossy(), "Nope").await.unwrap();
    assert_eq!((p.remote_count, p.local_count), (0, 0));
    assert!(p.free_bytes.unwrap_or(0) > 0);

    rig.app.make_remote_dir("Docs/New Folder").await.unwrap();
    assert!(rig.koofr("Docs/New Folder").is_dir());
    assert!(rig.app.make_remote_dir(" / ").await.is_err());
    assert_eq!(rig.app.list_remote("Docs").await.unwrap(), vec!["New Folder", "sub"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_and_snapshot_hide_private_details() {
    let rig = Rig::new();
    let _ = rig.pair("Docs").await;
    let d = rig.app.diagnostics();
    assert!(d.contains("t***@example.com"), "{d}");
    assert!(!d.contains("tester@example.com"));
    assert!(d.contains("HTTP/2 off"));
    let snap = rig.app.snapshot();
    for key in ["version", "setup_complete", "account", "engine", "settings", "sync", "pairs", "drive", "uploads"] {
        assert!(snap.get(key).is_some(), "snapshot is missing {key}");
    }
    assert!(snap["settings"].get("pairs").is_none());
    assert_eq!(snap["pairs"][0]["name"], "Docs");
    assert_eq!(snap["pairs"][0]["phase"], "ok");
    assert_eq!(snap["drive"]["state"], "off");
    assert_eq!(rig.app.status_line(), "SyncHopper: up to date");
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnecting_forgets_the_account_but_keeps_files() {
    let rig = Rig::new();
    write(&rig.pc("Docs/a.txt"), "a");
    let _ = rig.pair("Docs").await;
    std::fs::write(rig.app.core.config_path(), "[koofr]\n").unwrap();
    rig.app.disconnect().await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let s = rig.app.core.settings();
    assert!(!s.setup_complete && s.account_email.is_empty() && s.pairs.is_empty());
    assert!(!rig.app.core.config_path().exists());
    assert!(rig.pc("Docs/a.txt").exists() && rig.koofr("Docs/a.txt").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn leftover_engine_processes_are_cleaned_up_but_only_ours() {
    let dir = tempfile::tempdir().unwrap();
    let ours = dir.path().join("ours.conf");
    let theirs = dir.path().join("theirs.conf");
    let spawn = |conf: &std::path::Path| {
        std::process::Command::new("rclone")
            .args(["rcd", "--rc-no-auth", "--rc-addr", "127.0.0.1:0", "--config"])
            .arg(conf)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut mine = spawn(&ours);
    let mut other = spawn(&theirs);
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    assert!(mine.try_wait().unwrap().is_none() && other.try_wait().unwrap().is_none(), "both should be running");
    synchopper_lib::platform::kill_orphans(&ours).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(mine.try_wait().unwrap().is_some(), "the process started with our config should be stopped");
    assert!(other.try_wait().unwrap().is_none(), "someone else's rclone must be left alone");
    let _ = other.kill();
    let _ = other.wait();
}
