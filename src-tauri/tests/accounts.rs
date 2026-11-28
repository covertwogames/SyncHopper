#![cfg(unix)]
//! More than one account: a setup from version 0.4 carrying over, accounts
//! syncing side by side, adding, renaming and removing them, and the saved
//! sign-ins being locked.

mod common;
use common::filen_server::FakeFilen;
use common::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use synchopper_lib::app::{App, Commons, Hub, SetupChoices, SignIn};
use synchopper_lib::config;
use synchopper_lib::core::Shared;
use synchopper_lib::engine::Phase;
use synchopper_lib::colors;
use synchopper_lib::filen;
use synchopper_lib::providers;
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
    let (third, _) = rig.hub.add_account("filen", SignIn { email: "f@example.com".into(), password: "x".into(), ..Default::default() }).await.unwrap();
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
    assert_eq!(snap["providers"].as_array().unwrap().len(), 4);
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
    let (id, _) = rig.hub.add_account("filen", SignIn { email: "f@example.com".into(), password: "x".into(), ..Default::default() }).await.unwrap();
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
    let (id2, _) = rig.hub.add_account("filen", SignIn { email: "f@example.com".into(), password: "x".into(), ..Default::default() }).await.unwrap();
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
    // Nothing listens there, so Filen can't be reached.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    hub.commons.set_filen_gateway(&format!("http://{closed}"));
    let filen = |email: &str, password: &str, code: &str| SignIn { email: email.into(), password: password.into(), code: code.into(), ..Default::default() };
    let err = hub.add_account("filen", filen("", "x", "")).await.unwrap_err();
    assert!(err.contains("email address you use for Filen"), "{err}");
    let err = hub.add_account("filen", filen("a@b.c", "", "")).await.unwrap_err();
    assert_eq!(err, "Enter your Filen password.");
    let err = hub.add_account("filen", filen("nobody@example.com", "secret-password", "123456")).await.unwrap_err();
    assert!(err.starts_with("Could not reach Filen."), "{err}");
    assert!(!err.contains("secret-password"), "{err}");
    let err = hub.add_account("koofr", filen("nobody@example.com", "secret-password", "")).await.unwrap_err();
    assert!(!err.contains("secret-password"), "{err}");
    assert!(hub.shared.accounts().is_empty(), "a failed sign-in leaves no account behind");
    assert!(hub.apps().is_empty());
    let data = dir.path().join("data");
    // The key for the saved sign-ins may be there already: it is made before
    // a sign-in is tried, so that even the try is locked. It opens nothing.
    assert!(!data.join("rclone.conf").exists());
    let left: Vec<String> = names(&data).into_iter().filter(|n| n.starts_with("signin-")).collect();
    assert!(left.is_empty(), "{left:?}");
    let log = std::fs::read_to_string(hub.shared.log_path()).unwrap_or_default();
    assert!(!log.contains("secret-password"));
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

/// Every file in `dir` and below.
fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(all_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// What rclone's scrambling of a value hides.
fn reveal(obscured: &str) -> String {
    let out = std::process::Command::new("rclone").args(["reveal", obscured]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_filen_account_keeps_its_keys_and_never_its_password() {
    // Test mode with a stand-in for Filen: the sign-in is the real one,
    // and the account's files are a folder on this PC.
    let fake = FakeFilen::start().await;
    let rig = HubRig::new();
    rig.hub.commons.set_filen_gateway(&fake.url);
    let a = fake.account("v3-2fa");
    let s = |k: &str| a[k].as_str().unwrap().to_string();
    let (email, password, code) = (s("email"), s("password"), s("two_factor"));
    let signin = |code: &str| SignIn { email: email.clone(), password: password.clone(), code: code.into(), ..Default::default() };

    // Filen asks for the code first, then turns down a wrong one. Neither
    // keeps anything.
    let err = rig.hub.add_account("filen", signin("")).await.unwrap_err();
    assert!(err.contains(filen::CODE_WORDS) && err.starts_with("Your Filen account asks"), "{err}");
    let err = rig.hub.add_account("filen", signin("111111")).await.unwrap_err();
    assert!(err.contains(filen::CODE_WORDS) && err.starts_with("Filen did not accept that"), "{err}");
    assert!(rig.hub.shared.accounts().is_empty());
    assert!(!rig.path("data/rclone.conf").exists());

    let (id, _) = rig.hub.add_account("filen", signin(&code)).await.unwrap();
    let app = rig.hub.app(&id).unwrap();
    assert_eq!(app.core.settings().account_email, email);

    // The saved sign-in is locked and holds the keys, as Filen's desktop
    // app gives them to rclone.
    let conf = rig.path("data/rclone.conf");
    assert!(rclone::config_locked(&conf));
    let rc = rig.hub.shared.rclone(Vec::new()).unwrap();
    let text = rc.signins().await.unwrap();
    let remote = rig.hub.shared.account(&id).unwrap().remote;
    let body = rclone::section(&text, &remote).unwrap();
    let field = |k: &str| body.lines().find_map(|l| l.split_once(" = ").filter(|(n, _)| *n == k).map(|(_, v)| v.to_string())).unwrap_or_default();
    let e = &a["expect"];
    assert_eq!(field("type"), "filen");
    assert_eq!(field("email"), email);
    assert_eq!(reveal(&field("password")), "INTERNAL", "rclone is told to use the keys");
    assert_eq!(reveal(&field("api_key")), e["api_key"].as_str().unwrap());
    let master_keys: Vec<&str> = e["master_keys"].as_array().unwrap().iter().map(|k| k.as_str().unwrap()).collect();
    assert_eq!(field("master_keys"), master_keys.join("|"));
    assert_eq!(field("private_key"), e["private_key"].as_str().unwrap());
    assert_eq!(field("public_key"), e["public_key"].as_str().unwrap());
    assert_eq!(field("auth_version"), "3");
    assert_eq!(field("base_folder_uuid"), e["base_folder"].as_str().unwrap());
    assert_eq!(body.lines().count(), 9, "nothing else: {body}");

    // The password is nowhere on this PC: not in any of SyncHopper's files,
    // the log included. Nor is the key worked out from it.
    let kek = a["derived_key"].as_str().unwrap();
    for f in all_files(&rig.path("data")) {
        let bytes = std::fs::read(&f).unwrap();
        let t = String::from_utf8_lossy(&bytes);
        assert!(!t.contains(&password), "the password is in {}", f.display());
        assert!(!t.contains(kek), "the key made from the password is in {}", f.display());
    }
    assert!(!text.contains(&password) && !text.contains(kek), "nor in the saved sign-ins once unlocked");
    let left: Vec<String> = names(&rig.path("data")).into_iter().filter(|n| n.starts_with("signin-") || n.ends_with(".new")).collect();
    assert!(left.is_empty(), "{left:?}");

    // Reconnecting: a wrong password leaves the saved sign-in as it was;
    // the right one replaces it.
    let err = rig.hub.reconnect(&id, SignIn { email: email.clone(), password: "wrong".into(), code: code.clone(), ..Default::default() }).await.unwrap_err();
    assert_eq!(err, "Filen did not accept the sign-in. Filen says: Invalid email or password.");
    assert_eq!(rclone::section(&rc.signins().await.unwrap(), &remote).as_deref(), Some(body.as_str()));
    rig.hub.reconnect(&id, signin(&code)).await.unwrap();
    let again = rclone::section(&rc.signins().await.unwrap(), &remote).unwrap();
    assert_ne!(again, body, "new scrambling of the same values");
    assert!(again.contains(&format!("master_keys = {}", master_keys.join("|"))));
    assert!(rclone::config_locked(&conf));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_filen_sign_in_saved_with_its_password_by_a_test_build_is_taken_out() {
    // How 0.4.90 (a test build) saved a Filen sign-in: the password and an
    // API key. Starting this version takes that sign-in out, and leaves the
    // others, including a Filen account saved with its keys.
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("bin")).unwrap();
    let engine = String::from_utf8_lossy(&std::process::Command::new("sh").args(["-c", "command -v rclone"]).output().unwrap().stdout).trim().to_string();
    std::fs::copy(&engine, data.join("bin/rclone")).unwrap();
    let account = |id: &str, provider: &str, name: &str, slot: usize| {
        serde_json::json!({ "id": id, "provider": provider, "name": name, "remote": id, "sidebar_slot": slot, "setup_complete": true, "account_email": "a@b.c" })
    };
    let store = serde_json::json!({ "format": 2, "general": {}, "accounts": [
        account("koofr", "koofr", "Koofr", 0), account("shacctold1", "filen", "Filen", 1), account("shacctnew2", "filen", "Filen 2", 2)
    ] });
    std::fs::write(data.join("settings.json"), store.to_string()).unwrap();
    std::fs::write(
        data.join("rclone.conf"),
        "[koofr]\ntype = koofr\nprovider = koofr\nuser = a@b.c\npassword = DcF7Gw8hf\n\n\
         [shacctold1]\ntype = filen\nemail = a@b.c\npassword = cGFzc3dvcmQ\napi_key = a2V5\n\n\
         [shacctnew2]\ntype = filen\nemail = a@b.c\npassword = SU5URVJOQUw\napi_key = a2V5\nmaster_keys = abc\nprivate_key = x\npublic_key = y\nauth_version = 2\nbase_folder_uuid = z\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("koofr")).unwrap();
    let rig = HubRig::open(dir);
    rig.hub.start().await;
    let rc = rig.hub.shared.rclone(Vec::new()).unwrap();
    let text = rc.signins().await.unwrap();
    assert_eq!(rclone::section_names(&text), vec!["koofr", "shacctnew2"]);
    assert!(!text.contains("cGFzc3dvcmQ"));
    assert!(rclone::config_locked(&rig.path("data/rclone.conf")));
    let raw = std::fs::read_to_string(rig.path("data/rclone.conf")).unwrap();
    assert!(!raw.contains("cGFzc3dvcmQ"), "the old password is not left in the file either");
    let log = std::fs::read_to_string(rig.hub.shared.log_path()).unwrap();
    assert_eq!(log.matches("Took out a Filen sign-in saved by an earlier test version").count(), 1, "{log}");
    // Both accounts are still there; the one without a sign-in asks to be
    // reconnected the next time it syncs.
    assert_eq!(rig.hub.shared.accounts().len(), 3);
    assert!(rclone::is_auth_error("couldn't find section in config file") || rclone::is_auth_error("didn't find section in config file"));

    // Starting again finds nothing more to take out.
    let HubRig { dir, hub, .. } = rig;
    drop(hub);
    let rig = HubRig::open(dir);
    rig.hub.start().await;
    let log = std::fs::read_to_string(rig.hub.shared.log_path()).unwrap();
    assert_eq!(log.matches("Took out a Filen sign-in").count(), 1);
    assert_eq!(rclone::section_names(&rig.hub.shared.rclone(Vec::new()).unwrap().signins().await.unwrap()), vec!["koofr", "shacctnew2"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn unlocked_copies_left_behind_by_a_crash_are_removed_at_start() {
    // Left behind by the last run, before this one started.
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    write(&data.join("rclone.conf.new"), "[koofr]\ntype = koofr\npassword = plain\n");
    write(&data.join("signin-shacct1.conf"), "[shacct1]\ntype = filen\nmaster_keys = plain\n");
    write(&data.join("signin.key"), "kept");
    write(&data.join("signin-notes.txt"), "kept");
    std::fs::create_dir_all(dir.path().join("koofr")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let rig = HubRig::open(dir);
    // A sign-in being tried out right now, in this run.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write(&data.join("signin-shacct2.conf"), "[shacct2]\ntype = koofr\n");
    rig.hub.commons.secure_signins().await;
    let left = names(&data);
    assert!(!left.contains(&"rclone.conf.new".to_string()) && !left.contains(&"signin-shacct1.conf".to_string()), "{left:?}");
    assert!(left.contains(&"signin.key".to_string()) && left.contains(&"signin-notes.txt".to_string()), "{left:?}");
    assert!(left.contains(&"signin-shacct2.conf".to_string()), "a sign-in under way now is left alone: {left:?}");
    let log = std::fs::read_to_string(rig.hub.shared.log_path()).unwrap();
    assert!(log.contains("Removed 2 unlocked copies of sign-ins left behind"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_filen_sign_in_the_engine_cant_use_keeps_nothing() {
    // Not in test mode: Filen (the stand-in) accepts the sign-in and hands
    // over the keys, then the engine tries them on Filen itself, which this
    // machine can't reach. Nothing may be kept.
    let fake = FakeFilen::start().await;
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let hub = Hub::new(data.clone(), None, Arc::new(TestHost::default()));
    hub.shared.set_engine(|e| {
        e.state = "ready".into();
        e.exe = Some("rclone".into());
    });
    hub.commons.set_filen_gateway(&fake.url);
    let a = fake.account("v2");
    let s = |k: &str| a[k].as_str().unwrap().to_string();
    let err = hub.add_account("filen", SignIn { email: s("email"), password: s("password"), ..Default::default() }).await.unwrap_err();
    assert!(err.starts_with("Could not reach Filen") || err.starts_with("Signed in to Filen, but the sync engine could not open the account"), "{err}");
    assert!(fake.requests().iter().any(|r| r.path == "/v3/user/baseFolder"), "the sign-in itself went through");
    assert!(hub.shared.accounts().is_empty(), "no account is kept");
    assert!(!data.join("rclone.conf").exists(), "no sign-in is saved");
    let left: Vec<String> = names(&data).into_iter().filter(|n| n.starts_with("signin-")).collect();
    assert!(left.is_empty(), "the trial file is gone: {left:?}");
    for f in all_files(&data) {
        let t = String::from_utf8_lossy(&std::fs::read(&f).unwrap()).to_string();
        assert!(!t.contains(&s("password")) && !t.contains(&s("derived_key")), "{}", f.display());
    }
}

// ----- sign-ins are never on the disk unlocked ------------------------------

/// While `work` runs, keeps reading every file under `dir` and returns the
/// names of any that held one of `secrets` at any moment.
async fn files_that_ever_held<F: std::future::Future>(dir: &Path, secrets: &[&str], work: F) -> (Vec<String>, F::Output) {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (flag, root, wanted) = (stop.clone(), dir.to_path_buf(), secrets.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let watcher = std::thread::spawn(move || {
        let mut hits = std::collections::BTreeSet::new();
        let mut rounds = 0u64;
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            for f in all_files(&root) {
                if let Ok(bytes) = std::fs::read(&f) {
                    let text = String::from_utf8_lossy(&bytes);
                    if wanted.iter().any(|w| text.contains(w.as_str())) {
                        hits.insert(f.file_name().unwrap().to_string_lossy().to_string());
                    }
                }
            }
            rounds += 1;
        }
        (hits.into_iter().collect::<Vec<_>>(), rounds)
    });
    let out = work.await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let (hits, rounds) = watcher.join().unwrap();
    assert!(rounds > 20, "the watcher only looked {rounds} times");
    (hits, out)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_locked_file_is_the_engines_own_format_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let pass = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let text = "[koofr]\ntype = koofr\nuser = a@b.c\npassword = Gr\u{fc}\u{df}e\n\n[shacct1]\ntype = alias\nremote = /tmp\n";
    // What SyncHopper locks, the engine opens.
    let ours = dir.path().join("ours.conf");
    let locked = rclone::lock_text(pass, text).unwrap();
    assert!(locked.starts_with("# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\n") && !locked.contains("koofr"));
    assert_ne!(rclone::lock_text(pass, text).unwrap(), locked, "a new random start every time");
    std::fs::write(&ours, &locked).unwrap();
    assert!(rclone::config_locked(&ours));
    let show = |file: &Path, key: &str| {
        let out = std::process::Command::new("rclone")
            .args(["--config", &file.to_string_lossy(), "config", "show"])
            .env("RCLONE_CONFIG_PASS", key)
            .env("RCLONE_ASK_PASSWORD", "false")
            .output()
            .unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string())
    };
    let (ok, shown) = show(&ours, pass);
    assert!(ok, "{shown}");
    assert_eq!(rclone::section(&shown, "koofr"), rclone::section(text, "koofr"));
    assert_eq!(rclone::section(&shown, "shacct1"), rclone::section(text, "shacct1"));
    assert!(!show(&ours, "another-key-entirely-0123456789abcdef").0, "the wrong key opens nothing");
    assert_eq!(rclone::unlock_text(pass, &locked).as_deref(), Some(text));
    assert_eq!(rclone::unlock_text("wrong", &locked), None);
    assert_eq!(rclone::unlock_text(pass, text), None, "a plain file is not a locked one");

    // What the engine locks, SyncHopper opens.
    let theirs = dir.path().join("theirs.conf");
    std::fs::write(&theirs, text).unwrap();
    let mut child = std::process::Command::new("rclone")
        .args(["--config", &theirs.to_string_lossy(), "config", "encryption", "set"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(format!("{pass}\n{pass}\n").as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    let by_engine = std::fs::read_to_string(&theirs).unwrap();
    assert!(rclone::config_locked(&theirs));
    let opened = rclone::unlock_text(pass, &by_engine).expect("SyncHopper opens what the engine locked");
    assert_eq!(rclone::section(&opened, "koofr"), rclone::section(text, "koofr"));
    assert_eq!(rclone::section(&opened, "shacct1"), rclone::section(text, "shacct1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn saving_and_trying_out_a_sign_in_never_puts_it_on_the_disk_unlocked() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();

    // Saving, adding to and removing from the saved sign-ins.
    let commons = real_commons(&data);
    let (hits, _) = files_that_ever_held(&data, &["SECRET-ONE", "SECRET-TWO", "type = "], async {
        commons.save_signin("koofr", Some("type = koofr\nuser = a@b.c\npassword = SECRET-ONE")).await.unwrap();
        commons.save_signin("shacct1", Some("type = pcloud\ntoken = SECRET-TWO")).await.unwrap();
        commons.save_signin("koofr", None).await.unwrap();
        commons.save_signin("koofr", Some("type = koofr\nuser = a@b.c\npassword = SECRET-ONE")).await.unwrap();
    })
    .await;
    assert!(hits.is_empty(), "these files held a sign-in unlocked at some moment: {hits:?}");
    let rc = commons.shared.rclone(Vec::new()).unwrap();
    let text = rc.signins().await.unwrap();
    assert!(text.contains("SECRET-ONE") && text.contains("SECRET-TWO"), "and yet both are saved, for the engine to read with the key");
    assert!(!data.join("rclone.conf.new").exists());

    // A sign-in from 0.4, saved plain, is locked at start. The plain file
    // itself is what was there; no second plain copy is made of it.
    let old = dir.path().join("old");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("rclone.conf"), OLD_FILE).unwrap();
    let commons = real_commons(&old);
    let (hits, _) = files_that_ever_held(&old, &["DcF7Gw8hf"], commons.secure_signins()).await;
    assert!(hits.iter().all(|h| h == "rclone.conf"), "{hits:?}");
    assert!(rclone::config_locked(&old.join("rclone.conf")));

    // A whole sign-in, tried out in its own file first: Filen through the
    // stand-in, then the engine tries the keys on Filen itself, which takes
    // a moment and fails from here. The trial file exists all that time.
    let fake = FakeFilen::start().await;
    let third = dir.path().join("third");
    let hub = Hub::new(third.clone(), None, Arc::new(TestHost::default()));
    hub.shared.set_engine(|e| {
        e.state = "ready".into();
        e.exe = Some("rclone".into());
    });
    hub.commons.set_filen_gateway(&fake.url);
    let a = fake.account("v2");
    let s = |k: &str| a[k].as_str().unwrap().to_string();
    let expect = &a["expect"];
    let secrets = [expect["api_key"].as_str().unwrap(), expect["master_keys"][0].as_str().unwrap(), "master_keys = ", "type = filen"];
    let saw_trial = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (flag, folder) = (saw_trial.clone(), third.clone());
    let look = std::thread::spawn(move || {
        for _ in 0..4000 {
            if names(&folder).iter().any(|n| n.starts_with("signin-") && n.ends_with(".conf")) {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    });
    let (hits, result) = files_that_ever_held(&third, &secrets, hub.add_account("filen", SignIn { email: s("email"), password: s("password"), ..Default::default() })).await;
    look.join().unwrap();
    assert!(result.is_err(), "Filen itself can't be reached from here");
    assert!(saw_trial.load(std::sync::atomic::Ordering::SeqCst), "the trial file was there to be looked at");
    assert!(hits.is_empty(), "these files held the Filen keys unlocked at some moment: {hits:?}");
    assert!(third.join("signin.key").is_file(), "the key was made before the sign-in was tried");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_the_engine_writes_itself_stays_locked_too() {
    // Browser sign-ins (pCloud, kDrive) are written by the engine once the
    // browser comes back. This runs the same call with a kind of account
    // that needs no browser.
    let dir = tempfile::tempdir().unwrap();
    let pass = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let trial = dir.path().join("signin-x.conf");
    let probe = rclone::Rclone { exe: "rclone".into(), config: trial.clone(), cache: dir.path().join("cache"), extra: Vec::new(), pass: Some(pass.into()) };
    probe.write_signins("").unwrap();
    assert!(rclone::config_locked(&trial));
    let cancel = synchopper_lib::util::Cancel::default();
    let target = dir.path().to_string_lossy().to_string();
    probe.browser_sign_in("shacctx", "alias", &[("remote", &target)], &cancel, |_| {}).await.unwrap();
    assert!(rclone::config_locked(&trial), "the engine kept the file locked");
    let raw = std::fs::read_to_string(&trial).unwrap();
    assert!(!raw.contains("alias") && !raw.contains(&target));
    let text = probe.signins().await.unwrap();
    assert_eq!(rclone::section(&text, "shacctx").unwrap(), format!("type = alias\nremote = {target}"));
}

// ----- kDrive ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_starts_a_kdrive_sign_in_without_asking_anything_more() {
    // As far as it can go without Infomaniak: the engine takes the kDrive's
    // number, opens its sign-in page on this PC and waits, and that page
    // sends the browser on to Infomaniak's own sign-in.
    let dir = tempfile::tempdir().unwrap();
    let trial = dir.path().join("signin-k.conf");
    let pass = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let probe = rclone::Rclone { exe: "rclone".into(), config: trial.clone(), cache: dir.path().join("cache"), extra: Vec::new(), pass: Some(pass.into()) };
    probe.write_signins("").unwrap();
    let cancel = synchopper_lib::util::Cancel::default();
    let link = Arc::new(std::sync::Mutex::new(None::<String>));
    let (seen, stop) = (link.clone(), cancel.clone());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(40),
        probe.browser_sign_in("shacctk", providers::KDRIVE.kind, &[("drive_id", "654321")], &cancel, move |l| {
            *seen.lock().unwrap() = Some(l.to_string());
            // Ask the page where it sends the browser, then give up waiting.
            let (l, stop) = (l.to_string(), stop.clone());
            let seen = seen.clone();
            tokio::spawn(async move {
                let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
                if let Ok(res) = client.get(&l).send().await {
                    let to = res.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    *seen.lock().unwrap() = Some(format!("{l} -> {to}"));
                }
                stop.cancel();
            });
        }),
    )
    .await
    .expect("the engine did not wait on a question");
    assert_eq!(result, Err(rclone::SIGN_IN_CANCELLED.to_string()));
    let seen = link.lock().unwrap().clone().expect("the engine gave a sign-in link");
    let (page, to) = seen.split_once(" -> ").expect("the sign-in page answered");
    assert!(page.starts_with("http://127.0.0.1:53682/auth?state="), "{page}");
    assert!(to.starts_with("https://login.infomaniak.com/authorize?") && to.contains("scope=drive") && to.contains("redirect_uri=http%3A%2F%2Flocalhost%3A53682%2F") && to.contains("code_challenge="), "{to}");
    assert!(rclone::config_locked(&trial), "the trial file is still locked");
    // The engine notes the kind of account and the kDrive's number before
    // the browser comes back, and no sign-in. SyncHopper throws this trial
    // file away unless the sign-in finishes.
    let noted = probe.signins().await.unwrap();
    assert_eq!(rclone::section(&noted, "shacctk").as_deref(), Some("type = kdrive\ndrive_id = 654321"), "{noted}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kdrive_account_needs_its_number_and_keeps_it_for_reconnecting() {
    let rig = HubRig::new();
    let drive = |d: &str| SignIn { drive: d.into(), ..Default::default() };
    let err = rig.hub.add_account("kdrive", SignIn::default()).await.unwrap_err();
    assert_eq!(err, "Paste the address of your kDrive from your browser. It has the kDrive's number in it, after /drive/.");
    assert_eq!(rig.hub.add_account("kdrive", drive("my drive")).await.unwrap_err(), err);
    assert!(rig.hub.shared.accounts().is_empty());

    let (id, _) = rig.hub.add_account("kdrive", drive("https://ksuite.infomaniak.com/1234567/kdrive/app/drive/654321/files/5")).await.unwrap();
    let account = rig.hub.shared.account(&id).unwrap();
    assert_eq!((account.provider.as_str(), account.name.as_str(), account.kdrive.as_str(), account.drive_letter.as_str()), ("kdrive", "kDrive", "654321", "I"));
    let app = rig.hub.app(&id).unwrap();
    assert_eq!(app.snapshot()["kdrive"], "654321");
    assert_eq!(app.core.cloud(), "kDrive");
    app.finish_setup(rig.choices("pc/kDrive", &[])).await.unwrap();

    // Reconnecting with the box left as it is keeps the same kDrive; a new
    // address changes it; nonsense is turned down and changes nothing.
    rig.hub.reconnect(&id, SignIn::default()).await.unwrap();
    assert_eq!(rig.hub.shared.account(&id).unwrap().kdrive, "654321");
    assert_eq!(rig.hub.reconnect(&id, drive("nonsense")).await.unwrap_err(), err);
    assert_eq!(rig.hub.shared.account(&id).unwrap().kdrive, "654321");
    rig.hub.reconnect(&id, drive("777")).await.unwrap();
    assert_eq!(rig.hub.shared.account(&id).unwrap().kdrive, "777");
    // Other services never ask for one.
    let (other, _) = rig.hub.add_account("pcloud", SignIn::default()).await.unwrap();
    assert_eq!(rig.hub.shared.account(&other).unwrap().kdrive, "");
    let listed: Vec<String> = rig.hub.snapshot()["providers"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(listed, vec!["koofr", "pcloud", "filen", "kdrive"]);
}

// ----- account colors ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn accounts_get_colors_in_order_and_can_change_them() {
    let rig = HubRig::new();
    let koofr = rig.add("koofr", "pc/Koofr", &[]).await;
    let pcloud = rig.add("pcloud", "pc/pCloud", &[]).await;
    let filen = rig.add("filen", "pc/Filen", &[]).await;
    let color = |id: &str| rig.hub.shared.account(id).unwrap().color;
    assert_eq!((color(koofr.id()), color(pcloud.id()), color(filen.id())), ("teal".to_string(), "blue".to_string(), "orange".to_string()));
    assert_eq!(pcloud.snapshot()["color"], "blue");
    let palette: Vec<(String, String)> = rig.hub.snapshot()["colors"].as_array().unwrap().iter().map(|c| (c["id"].as_str().unwrap().to_string(), c["hex"].as_str().unwrap().to_string())).collect();
    assert_eq!(palette.len(), 8);
    assert_eq!(palette[0], ("teal".to_string(), "#0b7a75".to_string()));
    assert_eq!(palette.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(), colors::ALL.iter().map(|c| c.id).collect::<Vec<_>>());

    rig.hub.set_account_color(pcloud.id(), "purple").await.unwrap();
    assert_eq!(color(pcloud.id()), "purple");
    let err = rig.hub.set_account_color(pcloud.id(), "mauve").await.unwrap_err();
    assert_eq!(err, "SyncHopper doesn't have that color.");
    assert!(rig.hub.set_account_color("nobody", "red").await.is_err());
    assert_eq!(color(pcloud.id()), "purple");
    // Two accounts may share a color if that is what is picked.
    rig.hub.set_account_color(filen.id(), "teal").await.unwrap();
    assert_eq!(color(filen.id()), "teal");
    // The next account takes the first color nobody has: blue was given up.
    let (next, _) = rig.hub.add_account("koofr", SignIn { email: "second@example.com".into(), password: "x".into(), ..Default::default() }).await.unwrap();
    assert_eq!(color(&next), "blue");

    // Saved, and there after a restart.
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(rig.path("data/settings.json")).unwrap()).unwrap();
    let on_disk: Vec<&str> = saved["accounts"].as_array().unwrap().iter().map(|a| a["color"].as_str().unwrap()).collect();
    assert_eq!(on_disk, vec!["teal", "purple", "teal", "blue"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_saved_before_colors_get_theirs_in_the_order_they_were_added() {
    // A settings file as the 0.4.91 test build left it: three accounts and
    // no colors.
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let account = |id: &str, provider: &str, name: &str, slot: usize| serde_json::json!({ "id": id, "provider": provider, "name": name, "remote": id, "sidebar_slot": slot, "setup_complete": true });
    let mut later = account("shacct4", "koofr", "Koofr 2", 3);
    later["color"] = "blue".into();
    let mut odd = account("shacct5", "pcloud", "pCloud 2", 4);
    odd["color"] = "chartreuse".into();
    let store = serde_json::json!({ "format": 2, "general": {}, "accounts": [
        account("koofr", "koofr", "Koofr", 0), account("shacct2", "pcloud", "pCloud", 1), account("shacct3", "filen", "Filen", 2), later, odd
    ] });
    std::fs::write(data.join("settings.json"), store.to_string()).unwrap();
    let opened = config::open(&data.join("settings.json"));
    assert!(!opened.migrated);
    let got: Vec<(&str, &str)> = opened.store.accounts.iter().map(|a| (a.name.as_str(), a.color.as_str())).collect();
    // The one that already had blue keeps it, so the others go around it.
    assert_eq!(got, vec![("Koofr", "teal"), ("pCloud", "orange"), ("Filen", "purple"), ("Koofr 2", "blue"), ("pCloud 2", "pink")]);
    // The setup 0.4.1 leaves: its one account is teal, the app's own color.
    let old = dir.path().join("old.json");
    std::fs::write(&old, r#"{"setup_complete":true,"account_email":"a@b.c"}"#).unwrap();
    let opened = config::open(&old);
    assert_eq!(opened.store.accounts[0].color, "teal");
}
