#![cfg(unix)]
//! More than one account: a setup from version 0.4 carrying over, accounts
//! syncing side by side, adding, renaming and removing them, and the saved
//! sign-ins being locked.

mod common;
use common::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use synchopper_lib::app::{App, Commons, Hub, SetupChoices, SignIn};
use synchopper_lib::config;
use synchopper_lib::core::Shared;
use synchopper_lib::engine::Phase;
use synchopper_lib::rclone;

/// The whole app with plain folders standing in for the cloud accounts. The
/// first account's files are in `koofr`; later accounts get a folder each
/// next to it.
struct HubRig {
    dir: tempfile::TempDir,
    hub: Arc<Hub>,
    host: Arc<TestHost>,
}

impl HubRig {
    fn new() -> HubRig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("koofr")).unwrap();
        HubRig::open(dir)
    }

    /// Opens (or opens again, as after a restart) the app on a folder.
    fn open(dir: tempfile::TempDir) -> HubRig {
        let host = Arc::new(TestHost::default());
        let hub = Hub::new(dir.path().join("data"), Some(dir.path().join("koofr").to_string_lossy().to_string()), host.clone());
        hub.shared.set_engine(|e| {
            e.state = "ready".into();
            e.version = "test".into();
            e.exe = Some(PathBuf::from("rclone"));
        });
        hub.shared.change(|s| s.general.notify_level = "all".into());
        HubRig { dir, hub, host }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Where an account's files are "in the cloud".
    fn cloud(&self, id: &str) -> PathBuf {
        if id == "koofr" {
            self.path("koofr")
        } else {
            self.path(&format!("koofr-{id}"))
        }
    }

    fn choices(&self, base: &str, folders: &[&str]) -> SetupChoices {
        SetupChoices {
            feature_sync: true,
            feature_drive: false,
            autostart: true,
            base_folder: self.path(base).to_string_lossy().to_string(),
            folders: folders.iter().map(|s| s.to_string()).collect(),
            listed: folders.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Signs in to a new account and finishes its setup with `folders`.
    async fn add(&self, provider: &str, base: &str, folders: &[&str]) -> Arc<App> {
        let (id, _) = self.hub.add_account(provider, SignIn { email: "tester@example.com".into(), ..Default::default() }).await.unwrap();
        let app = self.hub.app(&id).unwrap();
        app.finish_setup(self.choices(base, folders)).await.unwrap();
        app
    }
}

async fn settled(app: &Arc<App>) {
    let a = app.clone();
    wait_for("folders to finish syncing", 60, move || !a.engine.busy() && a.engine.pair_views().iter().all(|p| p.phase == Phase::Ok)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_setup_from_version_0_4_carries_over_and_keeps_syncing_without_starting_over() {
    // A setup as version 0.4.1 left it: one account, one synced folder that
    // has synced, and a settings file in the old flat shape.
    let dir = tempfile::tempdir().unwrap();
    let pc = dir.path().join("pc").join("Koofr");
    let cloud = dir.path().join("koofr");
    write(&cloud.join("Docs/keep.txt"), "keep");
    write(&cloud.join("Docs/goes.txt"), "goes");
    write(&cloud.join("Docs/edit.txt"), "old");
    let data = dir.path().join("data");
    let pair_id;
    {
        let host = Arc::new(TestHost::default());
        let app = App::new(data.clone(), cloud.to_string_lossy().to_string(), true, host);
        app.core.set_engine(|e| {
            e.state = "ready".into();
            e.exe = Some("rclone".into());
        });
        app.core.update(|s| {
            s.setup_complete = true;
            s.account_email = "robert@example.com".into();
            s.base_folder = pc.to_string_lossy().to_string();
            s.transfers = 3;
            s.notify_level = "none".into();
        });
        let pair = app.engine.add_pair(&pc.join("Docs").to_string_lossy(), "Docs").unwrap();
        app.engine.drain().await;
        assert_eq!(read(&pc.join("Docs/goes.txt")), "goes");
        pair_id = pair.id;
        // Write the settings the way 0.4.1 did: flat, no accounts.
        let flat = serde_json::to_string_pretty(&app.core.settings()).unwrap();
        assert!(!flat.contains("\"accounts\""));
        std::fs::write(data.join("settings.json"), flat).unwrap();
    }
    // While SyncHopper is closed: a file is deleted in the cloud and another
    // is edited on the PC.
    std::fs::remove_file(cloud.join("Docs/goes.txt")).unwrap();
    tick().await;
    write(&pc.join("Docs/edit.txt"), "new from the pc");

    // The new version opens it.
    let host = Arc::new(TestHost::default());
    let hub = Hub::new(data.clone(), Some(cloud.to_string_lossy().to_string()), host);
    hub.shared.set_engine(|e| {
        e.state = "ready".into();
        e.exe = Some("rclone".into());
    });
    let accounts = hub.shared.accounts();
    assert_eq!(accounts.len(), 1);
    let a = &accounts[0];
    assert_eq!((a.id.as_str(), a.provider.as_str(), a.name.as_str(), a.remote.as_str()), ("koofr", "koofr", "Koofr", "koofr"));
    assert!(a.setup_complete);
    assert_eq!(a.account_email, "robert@example.com");
    assert_eq!(a.transfers, 3);
    assert_eq!(a.base_folder, pc.to_string_lossy());
    assert_eq!(a.pairs.len(), 1);
    assert_eq!(a.pairs[0].id, pair_id);
    assert!(!a.pairs[0].needs_resync, "the folder's sync record is still good");
    assert_eq!(hub.shared.general().notify_level, "none");
    assert!(data.join(config::LEGACY_BACKUP).exists(), "the old settings file is kept");
    let saved = std::fs::read_to_string(data.join("settings.json")).unwrap();
    assert!(saved.contains("\"accounts\"") && saved.contains("\"format\": 2"), "the file is rewritten in the new shape right away");

    // It syncs from where the old version left off. A sync that started
    // over would bring the deleted file back instead of deleting it here.
    let app = hub.app("koofr").unwrap();
    app.start_services().await;
    app.engine.sync_now(None);
    settled(&app).await;
    assert!(!pc.join("Docs/goes.txt").exists(), "the delete made in the cloud reached the PC");
    assert!(!cloud.join("Docs/goes.txt").exists());
    assert_eq!(read(&cloud.join("Docs/edit.txt")), "new from the pc");
    assert_eq!(read(&pc.join("Docs/keep.txt")), "keep");
    let log = std::fs::read_to_string(hub.shared.log_path()).unwrap_or_default();
    assert!(log.contains("moved into the new shape"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_accounts_sync_side_by_side_and_keep_to_their_own_folders() {
    let rig = HubRig::new();
    write(&rig.cloud("koofr").join("Documents/k.txt"), "from koofr");
    let koofr = rig.add("koofr", "pc/Koofr", &["Documents"]).await;
    assert_eq!(koofr.id(), "koofr", "the first Koofr account keeps the name it always had");

    // A second account, with a different service. Its files are elsewhere.
    let (id, about) = rig.hub.add_account("pcloud", SignIn::default()).await.unwrap();
    assert!(id.starts_with(rclone::REMOTE_PREFIX), "{id}");
    assert!(about.total.unwrap_or(0) > 0);
    let pcloud = rig.hub.app(&id).unwrap();
    write(&rig.cloud(&id).join("Documents/p.txt"), "from pcloud");
    write(&rig.cloud(&id).join("Photos/pic.jpg"), "pic");
    assert_eq!(pcloud.list_remote("").await.unwrap(), vec!["Documents", "Photos"]);
    assert!(rig.hub.ready().len() == 1, "an account being set up is not ready yet");
    pcloud.finish_setup(rig.choices("pc/SyncHopper/pCloud", &["Documents", "Photos"])).await.unwrap();
    settled(&koofr).await;
    settled(&pcloud).await;

    // Each account's files are in its own folder, and nowhere else.
    assert_eq!(read(&rig.path("pc/Koofr/Documents/k.txt")), "from koofr");
    assert_eq!(read(&rig.path("pc/SyncHopper/pCloud/Documents/p.txt")), "from pcloud");
    assert_eq!(read(&rig.path("pc/SyncHopper/pCloud/Photos/pic.jpg")), "pic");
    assert!(!rig.path("pc/Koofr/Documents/p.txt").exists());
    assert!(!rig.cloud("koofr").join("Documents/p.txt").exists());
    assert!(!rig.cloud(&id).join("Documents/k.txt").exists());

    // A change in one account syncs there and only there.
    write(&rig.path("pc/SyncHopper/pCloud/Documents/new.txt"), "n");
    pcloud.engine.sync_now(None);
    settled(&pcloud).await;
    assert_eq!(read(&rig.cloud(&id).join("Documents/new.txt")), "n");
    assert!(!rig.cloud("koofr").join("Documents/new.txt").exists());

    // What the account is and what its service needs.
    let accounts = rig.hub.shared.accounts();
    let p = accounts.iter().find(|a| a.id == id).unwrap();
    assert_eq!((p.provider.as_str(), p.name.as_str()), ("pcloud", "pCloud"));
    assert!(p.use_http2, "only Koofr starts with HTTP/2 off");
    assert!(!accounts[0].use_http2);
    assert_ne!(p.drive_letter, accounts[0].drive_letter);
    assert_eq!((accounts[0].sidebar_slot, p.sidebar_slot), (Some(0), Some(1)));
    assert_eq!(pcloud.core.cloud(), "pCloud");
    assert_eq!(pcloud.core.settings().pairs.len(), 2);
    assert_eq!(koofr.core.settings().pairs.len(), 1);

    // General settings are one set; an account's own are its own.
    pcloud.update_settings(serde_json::json!({ "notify_level": "none", "transfers": 2, "use_http2": false, "limit_requests": true })).await.unwrap();
    assert_eq!(koofr.core.settings().notify_level, "none");
    assert_eq!(koofr.core.settings().transfers, 8);
    let ps = pcloud.core.settings();
    assert_eq!(ps.transfers, 2);
    assert!(ps.use_http2 && !ps.limit_requests, "choices only Koofr has are not taken for other services");

    // A folder belongs to one account.
    let inside_koofr = rig.path("pc/Koofr/Documents/Sub").to_string_lossy().to_string();
    let err = pcloud.engine.add_pair(&inside_koofr, "Sub").unwrap_err();
    assert!(err.contains("Koofr"), "{err}");
    let koofr_home = rig.path("pc/Koofr/Loose").to_string_lossy().to_string();
    let err = pcloud.engine.add_pair(&koofr_home, "Loose").unwrap_err();
    assert!(err.contains("Koofr keeps its synced folders in"), "{err}");
    // And an account can't keep its folders where another one does.
    let (third, _) = rig.hub.add_account("filen", SignIn { email: "f@example.com".into(), password: "x".into(), api_key: "k".into() }).await.unwrap();
    let filen = rig.hub.app(&third).unwrap();
    let err = filen.finish_setup(rig.choices("pc/Koofr", &["Anything"])).await.unwrap_err();
    assert!(err.contains("overlaps the folders Koofr keeps"), "{err}");

    // What the window gets.
    let snap = rig.hub.snapshot();
    let list = snap["accounts"].as_array().unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[1]["cloud"], "pCloud");
    assert_eq!(list[1]["pairs"].as_array().unwrap().len(), 2);
    assert_eq!(list[2]["setup_complete"], false);
    assert_eq!(snap["providers"].as_array().unwrap().len(), 3);
    assert_eq!(rig.hub.status_line(), "SyncHopper: up to date");
    let report = rig.hub.diagnostics();
    assert!(report.contains("Accounts: 3") && report.contains("Account: pCloud") && !report.contains("tester@example.com"), "{report}");
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_get_names_of_their_own_and_can_be_renamed() {
    let rig = HubRig::new();
    let first = rig.add("koofr", "pc/Koofr", &[]).await;
    let second = rig.add("koofr", "pc/SyncHopper/Koofr 2", &[]).await;
    assert_eq!(first.core.label(), "Koofr");
    assert_eq!(second.core.label(), "Koofr 2");
    assert_ne!(first.id(), second.id());

    rig.hub.rename_account(second.id(), "  Work  ").await.unwrap();
    assert_eq!(second.core.label(), "Work");
    assert!(rig.hub.rename_account(second.id(), "koofr").await.unwrap_err().contains("already has that name"));
    assert!(rig.hub.rename_account(second.id(), "   ").await.is_err());
    assert!(rig.hub.rename_account(second.id(), &"x".repeat(41)).await.is_err());
    assert!(rig.hub.rename_account("nobody", "Name").await.is_err());
    assert_eq!(second.core.cloud(), "Koofr", "messages keep naming the service, whatever the account is called");

    // With more than one account, notifications and log lines say which.
    second.core.notify(synchopper_lib::core::Notice::Problem, "Something", "body");
    assert!(rig.host.notes.lock().unwrap().iter().any(|(t, _)| t == "Work: Something"));
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_an_account_leaves_its_files_and_the_other_accounts_alone() {
    let rig = HubRig::new();
    write(&rig.cloud("koofr").join("Documents/k.txt"), "k");
    let koofr = rig.add("koofr", "pc/Koofr", &["Documents"]).await;
    let (id, _) = rig.hub.add_account("filen", SignIn { email: "f@example.com".into(), password: "x".into(), api_key: "k".into() }).await.unwrap();
    let filen = rig.hub.app(&id).unwrap();
    write(&rig.cloud(&id).join("Notes/n.txt"), "n");
    filen.finish_setup(rig.choices("pc/SyncHopper/Filen", &["Notes"])).await.unwrap();
    settled(&koofr).await;
    settled(&filen).await;
    let pair_dir = filen.core.pair_dir(&filen.core.settings().pairs[0].id);
    assert!(pair_dir.exists());

    rig.hub.remove_account(&id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(rig.hub.app(&id).is_err());
    assert_eq!(rig.hub.shared.accounts().len(), 1);
    assert!(!pair_dir.exists(), "its sync records are gone");
    assert_eq!(read(&rig.path("pc/SyncHopper/Filen/Notes/n.txt")), "n", "files stay on the PC");
    assert_eq!(read(&rig.cloud(&id).join("Notes/n.txt")), "n", "and in the account");

    // The other account never noticed.
    assert_eq!(koofr.core.settings().pairs.len(), 1);
    write(&rig.path("pc/Koofr/Documents/after.txt"), "a");
    koofr.engine.sync_now(None);
    settled(&koofr).await;
    assert_eq!(read(&rig.cloud("koofr").join("Documents/after.txt")), "a");

    // The service's own name is free again for the next account.
    let again = rig.add("filen", "pc/SyncHopper/Filen again", &[]).await;
    assert_eq!(again.core.label(), "Filen");
    assert_ne!(again.id(), id, "an account's place in the engine is never reused");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_whose_setup_was_never_finished_is_gone_after_a_restart() {
    let rig = HubRig::new();
    let _koofr = rig.add("koofr", "pc/Koofr", &[]).await;
    let (id, _) = rig.hub.add_account("pcloud", SignIn::default()).await.unwrap();
    assert_eq!(rig.hub.shared.accounts().len(), 2);
    // Signing in to another one drops the unfinished one right away.
    let (id2, _) = rig.hub.add_account("filen", SignIn { email: "f@example.com".into(), password: "x".into(), api_key: "k".into() }).await.unwrap();
    assert!(rig.hub.app(&id).is_err());
    assert_eq!(rig.hub.shared.accounts().len(), 2);

    // Close and open again: starting up drops it too.
    let HubRig { dir, hub, .. } = rig;
    drop(hub);
    std::fs::create_dir_all(dir.path().join("data/bin")).unwrap();
    let engine = String::from_utf8_lossy(&std::process::Command::new("sh").args(["-c", "command -v rclone"]).output().unwrap().stdout).trim().to_string();
    std::fs::copy(&engine, dir.path().join("data/bin/rclone")).unwrap();
    let rig = HubRig::open(dir);
    assert_eq!(rig.hub.shared.accounts().len(), 2, "still in the file until SyncHopper starts");
    rig.hub.start().await;
    let accounts = rig.hub.shared.accounts();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, "koofr");
    assert!(rig.hub.app(&id2).is_err());
    assert_eq!(rig.hub.ready().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_right_click_upload_goes_straight_in_with_one_account_and_asks_with_two() {
    let rig = HubRig::new();
    let koofr = rig.add("koofr", "pc/Koofr", &[]).await;
    write(&rig.path("loose/a.bin"), "a");
    let file = rig.path("loose/a.bin").to_string_lossy().to_string();
    assert!(rig.hub.handle_args(&["synchopper".into(), "--upload".into(), file.clone()]));
    let k = koofr.clone();
    wait_for("the upload", 30, move || k.uploads.active() == 0 && !k.uploads.views().is_empty()).await;
    assert_eq!(read(&rig.cloud("koofr").join("a.bin")), "a");
    assert!(rig.hub.snapshot()["pending"].is_null());

    let _second = rig.add("pcloud", "pc/SyncHopper/pCloud", &[]).await;
    assert!(rig.hub.handle_args(&["synchopper".into(), "--upload".into(), file.clone()]));
    assert!(rig.hub.handle_args(&["synchopper".into(), "--upload".into(), "/another".into()]));
    let snap = rig.hub.snapshot();
    assert_eq!(snap["pending"]["kind"], "upload");
    assert_eq!(snap["pending"]["paths"].as_array().unwrap().len(), 2, "a big selection arrives in pieces and is kept together");
    rig.hub.clear_pending();
    assert!(rig.hub.snapshot()["pending"].is_null());

    // A folder to sync is offered to the account whose own folder it is in.
    std::fs::create_dir_all(rig.path("pc/SyncHopper/pCloud/New stuff")).unwrap();
    rig.hub.request_add_folder(&rig.path("pc/SyncHopper/pCloud/New stuff").to_string_lossy());
    let snap = rig.hub.snapshot();
    assert!(snap["accounts"][0]["add_request"].is_null());
    assert_eq!(snap["accounts"][1]["add_request"]["remote"], "New stuff");
    // Anywhere else, the first account is suggested and the window can move it.
    std::fs::create_dir_all(rig.path("elsewhere/Thing")).unwrap();
    rig.hub.request_add_folder(&rig.path("elsewhere/Thing").to_string_lossy());
    let snap = rig.hub.snapshot();
    assert_eq!(snap["accounts"][0]["add_request"]["remote"], "Thing");
    assert!(snap["accounts"][1]["add_request"].is_null());
}

// ----- saved sign-ins ------------------------------------------------------

fn real_commons(data: &Path) -> Arc<Commons> {
    let shared = Shared::open(data.to_path_buf(), false, Arc::new(TestHost::default()));
    shared.set_engine(|e| {
        e.state = "ready".into();
        e.exe = Some("rclone".into());
    });
    Commons::new(shared, rclone::DOWNLOAD_BASE.to_string())
}

const OLD_FILE: &str = "[koofr]\ntype = koofr\nprovider = koofr\nuser = a@b.c\npassword = DcF7Gw8hf_s2HGwGNMNPEKoKWme1BQxM6Y7M\n";

#[tokio::test(flavor = "multi_thread")]
async fn sign_ins_saved_by_an_earlier_version_are_locked_and_still_work() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let conf = data.join("rclone.conf");
    std::fs::write(&conf, OLD_FILE).unwrap();
    let commons = real_commons(&data);
    assert!(!rclone::config_locked(&conf));
    commons.secure_signins().await;
    assert!(rclone::config_locked(&conf), "the file is locked now");
    let raw = std::fs::read_to_string(&conf).unwrap();
    assert!(!raw.contains("a@b.c") && !raw.contains("DcF7Gw8hf"), "nothing of the sign-in can be read from the file");
    assert!(data.join("signin.key").is_file());
    let key = commons.shared.config_pass().expect("the key is in use");
    assert!(!raw.contains(&key));

    // The engine reads it with the key, and not without.
    let rc = commons.shared.rclone(Vec::new()).unwrap();
    let text = rc.signins().await.unwrap();
    assert_eq!(rclone::section(&text, "koofr").unwrap(), OLD_FILE.lines().skip(1).collect::<Vec<_>>().join("\n"));
    let no_key = rclone::Rclone { pass: None, ..rc.clone() };
    let err = no_key.signins().await.unwrap_err();
    assert!(rclone::is_locked_signins(&err), "{err}");
    assert_eq!(rclone::friendly_error(&synchopper_lib::providers::KOOFR, &err), rclone::LOCKED_SIGNINS_TEXT);

    // Adding and removing accounts keeps it locked and keeps the others.
    commons.save_signin("shacct1a", Some("type = filen\nemail = f@example.com\npassword = xx\napi_key = yy")).await.unwrap();
    assert!(rclone::config_locked(&conf));
    let text = rc.signins().await.unwrap();
    assert_eq!(rclone::section_names(&text), vec!["koofr", "shacct1a"]);
    assert_eq!(rclone::section(&text, "shacct1a").unwrap(), "type = filen\nemail = f@example.com\npassword = xx\napi_key = yy");
    commons.save_signin("koofr", None).await.unwrap();
    assert_eq!(rclone::section_names(&rc.signins().await.unwrap()), vec!["shacct1a"]);
    assert!(!data.join("rclone.conf.new").exists());

    // A restart finds the key again.
    let again = real_commons(&data);
    again.secure_signins().await;
    assert_eq!(again.shared.config_pass().as_deref(), Some(key.as_str()));
    // The last account gone: no file left.
    again.save_signin("shacct1a", None).await.unwrap();
    assert!(!conf.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_install_locks_the_first_sign_in_it_saves() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let commons = real_commons(&data);
    commons.secure_signins().await;
    assert!(commons.shared.config_pass().is_none() && !data.join("signin.key").exists(), "nothing to lock yet");
    commons.save_signin("koofr", Some("type = koofr\nprovider = koofr\nuser = a@b.c\npassword = zz")).await.unwrap();
    assert!(rclone::config_locked(&data.join("rclone.conf")));
    assert!(commons.shared.config_pass().is_some());
    let rc = commons.shared.rclone(Vec::new()).unwrap();
    assert_eq!(rclone::section_names(&rc.signins().await.unwrap()), vec!["koofr"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_ins_that_cannot_be_read_here_ask_for_a_reconnect_and_start_over() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("rclone.conf"), OLD_FILE).unwrap();
    let commons = real_commons(&data);
    commons.secure_signins().await;
    // The app's files turn up on another PC: the key can't be read there.
    std::fs::remove_file(data.join("signin.key")).unwrap();
    let moved = real_commons(&data);
    moved.secure_signins().await;
    assert!(moved.shared.config_pass().is_none());
    let log = std::fs::read_to_string(moved.shared.log_path()).unwrap();
    assert!(log.contains("Each account needs to be reconnected"), "{log}");
    let err = moved.shared.rclone(Vec::new()).unwrap().about("koofr:").await.unwrap_err();
    assert!(rclone::is_locked_signins(&err), "{err}");
    assert_eq!(synchopper_lib::engine::issue_kind(&err), "auth", "a folder shows it as a sign-in problem");

    // Reconnecting an account starts a new file with a new key.
    moved.save_signin("koofr", Some("type = koofr\nprovider = koofr\nuser = new@b.c\npassword = zz")).await.unwrap();
    let rc = moved.shared.rclone(Vec::new()).unwrap();
    assert!(rc.pass.is_some());
    assert!(rclone::section(&rc.signins().await.unwrap(), "koofr").unwrap().contains("new@b.c"));
    // An account that was only in the old file now reads as not signed in.
    let err = rc.about("shacct9:").await.unwrap_err();
    assert_eq!(synchopper_lib::engine::issue_kind(&err), "auth", "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_sign_in_that_fails_saves_nothing_for_any_service() {
    // Not in test mode: the real path, which must fail here (no such
    // accounts, or no route to the services) and leave nothing behind.
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(TestHost::default());
    let hub = Hub::new(dir.path().join("data"), None, host);
    hub.shared.set_engine(|e| {
        e.state = "ready".into();
        e.exe = Some("rclone".into());
    });
    assert!(hub.add_account("nothing", SignIn::default()).await.is_err());
    let filen = |email: &str, password: &str, key: &str| SignIn { email: email.into(), password: password.into(), api_key: key.into() };
    let err = hub.add_account("filen", filen("", "x", "k")).await.unwrap_err();
    assert!(err.contains("email address you use for Filen"), "{err}");
    let err = hub.add_account("filen", filen("a@b.c", " ", "k")).await.unwrap_err();
    assert_eq!(err, "Enter your Filen password.");
    let err = hub.add_account("filen", filen("a@b.c", "x", " \n ")).await.unwrap_err();
    assert_eq!(err, "Enter your Filen API key.");
    let err = hub.add_account("filen", filen("nobody@example.com", "secret-password", "secret-key")).await.unwrap_err();
    assert!(!err.contains("secret-password") && !err.contains("secret-key"), "{err}");
    let err = hub.add_account("koofr", filen("nobody@example.com", "secret-password", "")).await.unwrap_err();
    assert!(!err.contains("secret-password"), "{err}");
    assert!(hub.shared.accounts().is_empty(), "a failed sign-in leaves no account behind");
    assert!(hub.apps().is_empty());
    let data = dir.path().join("data");
    assert!(!data.join("rclone.conf").exists() && !data.join("signin.key").exists());
    let left: Vec<String> = names(&data).into_iter().filter(|n| n.starts_with("signin-")).collect();
    assert!(left.is_empty(), "{left:?}");
    let log = std::fs::read_to_string(hub.shared.log_path()).unwrap_or_default();
    assert!(!log.contains("secret-password") && !log.contains("secret-key"));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_real_accounts_sync_with_their_sign_ins_locked() {
    // Not in test mode: the engine reaches each account by its name in the
    // saved sign-ins, exactly as it does for a real service. The two
    // "services" here are engine aliases for folders, so no network is
    // needed, and everything else is the real path.
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let (cloud1, cloud2) = (dir.path().join("cloud1"), dir.path().join("cloud2"));
    write(&cloud1.join("Docs/k.txt"), "from the first");
    write(&cloud2.join("Docs/p.txt"), "from the second");
    std::fs::write(
        data.join("rclone.conf"),
        format!("[koofr]\ntype = alias\nremote = {}\n\n[shacctab12]\ntype = alias\nremote = {}\n", cloud1.display(), cloud2.display()),
    )
    .unwrap();
    let account = |id: &str, provider: &str, name: &str, slot: usize| {
        serde_json::json!({ "id": id, "provider": provider, "name": name, "remote": id, "sidebar_slot": slot, "setup_complete": true, "account_email": "a@b.c" })
    };
    let store = serde_json::json!({ "format": 2, "general": { "notify_level": "all" }, "accounts": [account("koofr", "koofr", "Koofr", 0), account("shacctab12", "pcloud", "pCloud", 1)] });
    std::fs::write(data.join("settings.json"), store.to_string()).unwrap();

    let host = Arc::new(TestHost::default());
    let hub = Hub::new(data.clone(), None, host);
    hub.shared.set_engine(|e| {
        e.state = "ready".into();
        e.exe = Some("rclone".into());
    });
    hub.commons.secure_signins().await;
    let conf = data.join("rclone.conf");
    assert!(rclone::config_locked(&conf), "the sign-ins are locked before anything runs");
    assert!(!std::fs::read_to_string(&conf).unwrap().contains("alias"));

    let first = hub.app("koofr").unwrap();
    let second = hub.app("shacctab12").unwrap();
    assert_eq!((first.core.remote("Docs"), second.core.remote("Docs")), ("koofr:Docs".to_string(), "shacctab12:Docs".to_string()));
    let pc1 = dir.path().join("pc/Koofr/Docs");
    let pc2 = dir.path().join("pc/SyncHopper/pCloud/Docs");
    let p1 = first.engine.add_pair(&pc1.to_string_lossy(), "Docs").unwrap();
    let p2 = second.engine.add_pair(&pc2.to_string_lossy(), "Docs").unwrap();
    // Each account has its own queue, so both run at once.
    tokio::join!(first.engine.drain(), second.engine.drain());
    let phase = |app: &Arc<App>, id: &str| app.engine.pair_views().into_iter().find(|p| p.id == id).map(|p| (p.phase, p.error, p.details));
    assert_eq!(phase(&first, &p1.id).unwrap().0, Phase::Ok, "{:?}", phase(&first, &p1.id));
    assert_eq!(phase(&second, &p2.id).unwrap().0, Phase::Ok, "{:?}", phase(&second, &p2.id));
    assert_eq!(read(&pc1.join("k.txt")), "from the first");
    assert_eq!(read(&pc2.join("p.txt")), "from the second");
    assert!(!pc1.join("p.txt").exists() && !pc2.join("k.txt").exists());

    // Changes go up to the right account, and the engine can still ask
    // each one how much space it has.
    tick().await;
    write(&pc1.join("up1.txt"), "1");
    write(&pc2.join("up2.txt"), "2");
    first.engine.sync_now(None);
    second.engine.sync_now(None);
    tokio::join!(first.engine.drain(), second.engine.drain());
    assert_eq!(read(&cloud1.join("Docs/up1.txt")), "1");
    assert_eq!(read(&cloud2.join("Docs/up2.txt")), "2");
    assert!(!cloud1.join("Docs/up2.txt").exists() && !cloud2.join("Docs/up1.txt").exists());
    second.refresh_about().await;
    assert!(second.snapshot()["account"]["total"].as_u64().unwrap_or(0) > 0);
    assert!(rclone::config_locked(&conf), "still locked after syncing");

    // Taking one account out leaves the other's sign-in in place and working.
    hub.remove_account("shacctab12").await.unwrap();
    let rc = hub.shared.rclone(Vec::new()).unwrap();
    assert_eq!(rclone::section_names(&rc.signins().await.unwrap()), vec!["koofr"]);
    assert!(rclone::config_locked(&conf));
    tick().await;
    write(&pc1.join("later.txt"), "l");
    first.engine.sync_now(None);
    first.engine.drain().await;
    assert_eq!(read(&cloud1.join("Docs/later.txt")), "l");
    assert_eq!(read(&cloud2.join("Docs/p.txt")), "from the second", "the removed account's files are untouched");
    assert_eq!(read(&pc2.join("p.txt")), "from the second");
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_made_at_the_same_moment_all_reach_the_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let host = Arc::new(TestHost::default());
    let hub = Hub::new(data.clone(), Some(dir.path().join("cloud").to_string_lossy().to_string()), host);
    let shared = hub.shared.clone();
    let mut workers = Vec::new();
    for worker in 0..8u32 {
        let shared = shared.clone();
        workers.push(std::thread::spawn(move || {
            for step in 0..60u32 {
                shared.change(|s| s.general.update_notified = format!("{worker}.{step}"));
            }
        }));
    }
    for w in workers {
        w.join().unwrap();
    }
    // What is on disk is the last change made, not an earlier one that
    // happened to be written later, and no save failed along the way.
    let on_disk: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(data.join("settings.json")).unwrap()).unwrap();
    assert_eq!(on_disk["general"]["update_notified"].as_str().unwrap(), shared.general().update_notified);
    let log = std::fs::read_to_string(shared.log_path()).unwrap_or_default();
    assert!(!log.contains("Could not save settings"), "{log}");
    assert!(!data.join("settings.json.tmp").exists());
}
