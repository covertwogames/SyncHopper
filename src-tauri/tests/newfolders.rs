//! What happens to things put straight into the Koofr folder on this PC:
//! new folders start syncing (or SyncHopper asks first when Koofr already has
//! one by that name), renamed synced folders are followed and renamed in
//! Koofr too, loose files are pointed out, and "Sync folder to Koofr" from
//! File Explorer opens the right dialog or says why it can't.

mod common;
use common::*;
use synchopper_lib::engine::Phase;

/// A rig whose Koofr folder on this PC is its "pc" folder, looked at once so
/// what is already there counts as known.
async fn rig_with_base() -> Rig {
    let rig = Rig::new();
    let base = rig.dir.path().join("pc").to_string_lossy().to_string();
    rig.app.core.update(|s| s.base_folder = base);
    rig.app.folders.scan().await;
    rig
}

fn pair_for(rig: &Rig, name: &str) -> Option<synchopper_lib::config::Pair> {
    let local = rig.pc(name).to_string_lossy().to_string();
    rig.app.core.settings().pairs.into_iter().find(|p| p.local == local)
}

fn notices(rig: &Rig) -> Vec<(String, String)> {
    rig.app.folders.notices().into_iter().map(|n| (n.kind, n.message)).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn what_is_already_there_is_left_alone() {
    let rig = Rig::new();
    write(&rig.pc("Old stuff/a.txt"), "a");
    write(&rig.pc("loose.txt"), "x");
    let base = rig.dir.path().join("pc").to_string_lossy().to_string();
    rig.app.core.update(|s| s.base_folder = base);
    rig.app.folders.scan().await;
    rig.app.folders.scan().await;
    assert!(rig.app.core.settings().pairs.is_empty());
    assert!(notices(&rig).is_empty());
    assert!(rig.app.core.settings().base_known_ready);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_folder_starts_syncing_by_itself() {
    let rig = rig_with_base().await;
    write(&rig.pc("Taxes/2026.pdf"), "numbers");
    rig.app.folders.scan().await;
    let pair = pair_for(&rig, "Taxes").expect("the new folder should be synced");
    assert_eq!(pair.remote, "Taxes");
    rig.app.engine.drain().await;
    assert_eq!(read(&rig.koofr("Taxes/2026.pdf")), "numbers");
    assert_eq!(rig.view(&pair.id).phase, Phase::Ok);
    assert!(rig.host.notes.lock().unwrap().iter().any(|(t, _)| t == "Now syncing \"Taxes\""));
    // Looking again changes nothing.
    rig.app.folders.scan().await;
    assert_eq!(rig.app.core.settings().pairs.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_koofr_already_has_is_checked_with_the_user_first() {
    let rig = rig_with_base().await;
    write(&rig.koofr("Photos/beach.jpg"), "sand");
    write(&rig.pc("Photos/dog.jpg"), "woof");
    rig.app.folders.scan().await;
    assert!(pair_for(&rig, "Photos").is_none(), "nothing is merged without asking");
    assert_eq!(names(&rig.koofr("Photos")), vec!["beach.jpg"]);
    let n = notices(&rig);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].0, "clash");
    assert!(n[0].1.contains("already has a folder called \"Photos\""));
    assert!(rig.host.notes.lock().unwrap().iter().any(|(t, _)| t == "\"Photos\" isn't synced yet"));

    // "Sync it" opens the dialog filled in, with the reason shown.
    rig.app.request_add_folder(&rig.pc("Photos").to_string_lossy());
    let req = rig.app.snapshot()["add_request"].clone();
    assert_eq!(req["remote"], "Photos");
    assert_eq!(req["local"], rig.pc("Photos").to_string_lossy().to_string());
    assert!(req["blocked"].is_null());
    assert!(req["note"].as_str().unwrap().contains("already has a folder"));
    rig.app.clear_add_request();
    assert!(rig.app.snapshot()["add_request"].is_null());

    // Once the user adds it, the notice goes away.
    rig.app.engine.add_pair(&rig.pc("Photos").to_string_lossy(), "Photos").unwrap();
    assert!(notices(&rig).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_setting_off_new_folders_are_only_pointed_out() {
    let rig = rig_with_base().await;
    rig.app.core.update(|s| s.auto_add_folders = false);
    write(&rig.pc("Recipes/soup.txt"), "soup");
    rig.app.folders.scan().await;
    assert!(pair_for(&rig, "Recipes").is_none());
    let n = notices(&rig);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].0, "not_synced");
    assert!(rig.host.notes.lock().unwrap().iter().any(|(t, _)| t == "\"Recipes\" isn't synced yet"));
    // It is only pointed out once.
    let count = rig.host.notes.lock().unwrap().len();
    rig.app.folders.scan().await;
    assert_eq!(rig.host.notes.lock().unwrap().len(), count);
}

#[tokio::test(flavor = "multi_thread")]
async fn loose_files_are_pointed_out() {
    let rig = rig_with_base().await;
    write(&rig.pc("receipt.pdf"), "paid");
    write(&rig.pc("desktop.ini"), "explorer junk");
    rig.app.folders.scan().await;
    assert!(rig.app.core.settings().pairs.is_empty());
    let n = notices(&rig);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].0, "files");
    assert!(n[0].1.contains("\"receipt.pdf\""), "{}", n[0].1);
    assert!(!n[0].1.contains("desktop.ini"));
    // Moving the file into a folder clears it.
    std::fs::create_dir_all(rig.pc("Taxes")).unwrap();
    std::fs::rename(rig.pc("receipt.pdf"), rig.pc("Taxes/receipt.pdf")).unwrap();
    assert!(notices(&rig).iter().all(|(kind, _)| kind != "files"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_folder_is_renamed_in_koofr_too() {
    let rig = rig_with_base().await;
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    rig.app.folders.scan().await;
    assert_eq!(read(&rig.koofr("Docs/a.txt")), "a");

    std::fs::rename(rig.pc("Docs"), rig.pc("Papers")).unwrap();
    // The sync that notices it gone first doesn't call it a problem.
    rig.sync(&id).await;
    assert_ne!(rig.phase(&id), Phase::Error, "{}", rig.dump(&id));
    rig.app.folders.scan().await;
    let pair = rig.app.core.pair(&id).unwrap();
    assert_eq!(pair.local, rig.pc("Papers").to_string_lossy());
    assert_eq!(pair.remote, "Papers");
    assert_eq!(read(&rig.koofr("Papers/a.txt")), "a");
    assert!(!rig.koofr("Docs").exists());
    rig.app.engine.drain().await;
    write(&rig.pc("Papers/b.txt"), "b");
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Papers/b.txt")), "b");
    assert_eq!(rig.app.core.settings().pairs.len(), 1, "no second copy under the new name");
    assert!(notices(&rig).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rename_to_a_name_koofr_already_has_keeps_the_old_koofr_name() {
    let rig = rig_with_base().await;
    write(&rig.pc("Docs/a.txt"), "a");
    write(&rig.koofr("Papers/other.txt"), "someone else's");
    let id = rig.pair("Docs").await;
    rig.app.folders.scan().await;
    std::fs::rename(rig.pc("Docs"), rig.pc("Papers")).unwrap();
    rig.app.folders.scan().await;
    let pair = rig.app.core.pair(&id).unwrap();
    assert_eq!(pair.local, rig.pc("Papers").to_string_lossy());
    assert_eq!(pair.remote, "Docs");
    assert_eq!(names(&rig.koofr("Papers")), vec!["other.txt"], "Koofr's own Papers folder is untouched");
    let n = notices(&rig);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].0, "rename_kept");
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/a.txt")), "a");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rename_to_a_name_koofr_refuses_keeps_the_old_koofr_name() {
    let rig = rig_with_base().await;
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    rig.app.folders.scan().await;
    std::fs::rename(rig.pc("Docs"), rig.pc("Docs \u{1F4DA}")).unwrap();
    rig.app.folders.scan().await;
    let pair = rig.app.core.pair(&id).unwrap();
    assert_eq!(pair.local, rig.pc("Docs \u{1F4DA}").to_string_lossy());
    assert_eq!(pair.remote, "Docs");
    let n = notices(&rig);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].0, "rename_kept");
    assert!(n[0].1.starts_with("Koofr doesn't allow emoji"), "{}", n[0].1);
    assert!(rig.host.notes.lock().unwrap().iter().any(|(t, _)| t == "\"Docs \u{1F4DA}\" was not renamed in Koofr"));
    write(&rig.pc("Docs \u{1F4DA}/b.txt"), "b");
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/b.txt")), "b");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_folder_named_with_emoji_is_explained_not_synced() {
    let rig = rig_with_base().await;
    write(&rig.pc("Party \u{1F389}/a.txt"), "a");
    rig.app.folders.scan().await;
    assert!(rig.app.core.settings().pairs.is_empty());
    let n = notices(&rig);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].0, "failed");
    assert!(n[0].1.contains("Koofr doesn't allow emoji"), "{}", n[0].1);
    // Renamed, it syncs.
    std::fs::rename(rig.pc("Party \u{1F389}"), rig.pc("Party")).unwrap();
    rig.app.folders.scan().await;
    assert!(pair_for(&rig, "Party").is_some());
    assert!(notices(&rig).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_that_stopped_syncing_is_not_added_back() {
    let rig = rig_with_base().await;
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    rig.app.folders.scan().await;
    rig.app.engine.remove_pair(&id);
    rig.app.folders.scan().await;
    assert!(rig.app.core.settings().pairs.is_empty());
    assert!(notices(&rig).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_added_when_the_whole_koofr_folder_already_syncs() {
    let rig = rig_with_base().await;
    let base = rig.dir.path().join("pc").to_string_lossy().to_string();
    rig.app.engine.add_pair(&base, "Everything").unwrap();
    write(&rig.pc("New/x.txt"), "x");
    rig.app.folders.scan().await;
    assert_eq!(rig.app.core.settings().pairs.len(), 1);
    assert!(notices(&rig).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_folder_waits_when_koofr_cannot_be_checked() {
    let rig = rig_with_base().await;
    rig.app.core.set_engine(|e| e.exe = Some("/nonexistent/rclone".into()));
    write(&rig.pc("Later/x.txt"), "x");
    rig.app.folders.scan().await;
    assert!(pair_for(&rig, "Later").is_none());
    assert!(notices(&rig).is_empty());
    rig.app.core.set_engine(|e| e.exe = Some("rclone".into()));
    rig.app.folders.scan().await;
    assert!(pair_for(&rig, "Later").is_some(), "it is picked up once Koofr can be checked");
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_folder_to_koofr_explains_what_it_cannot_do() {
    let rig = rig_with_base().await;
    write(&rig.pc("Docs/sub/a.txt"), "a");
    rig.pair("Docs").await;
    let ask = |path: String| {
        rig.app.request_add_folder(&path);
        let req = rig.app.snapshot()["add_request"].clone();
        rig.app.clear_add_request();
        req
    };
    let blocked = |path: String| ask(path)["blocked"].as_str().unwrap_or_default().to_string();
    assert!(blocked(rig.pc("Docs").to_string_lossy().to_string()).contains("already synced"));
    assert!(blocked(rig.pc("Docs/sub").to_string_lossy().to_string()).contains("inside \"Docs\""));
    assert!(blocked(rig.dir.path().join("pc").to_string_lossy().to_string()).contains("This is your Koofr folder"));
    assert!(blocked(rig.pc("Nope").to_string_lossy().to_string()).contains("doesn't exist"));
    #[cfg(target_os = "linux")]
    assert!(blocked("/etc".to_string()).contains("system folder"));

    // A folder outside the Koofr folder is fine.
    let outside = rig.dir.path().join("elsewhere/Projects");
    std::fs::create_dir_all(&outside).unwrap();
    let req = ask(outside.to_string_lossy().to_string());
    assert!(req["blocked"].is_null(), "{req}");
    assert_eq!(req["remote"], "Projects");

    // Turned off: says so.
    rig.app.core.update(|s| s.feature_sync = false);
    assert!(blocked(outside.to_string_lossy().to_string()).contains("turned off"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_explorer_command_line_reaches_the_window() {
    let rig = rig_with_base().await;
    let outside = rig.dir.path().join("elsewhere/Music");
    std::fs::create_dir_all(&outside).unwrap();
    let args = vec!["synchopper".to_string(), "--add-folder".to_string(), outside.to_string_lossy().to_string()];
    assert!(rig.app.handle_args(&args));
    assert_eq!(rig.app.snapshot()["add_request"]["remote"], "Music");
    assert!(!rig.host.shown.lock().unwrap().is_empty(), "the window is brought up");
}

#[tokio::test(flavor = "multi_thread")]
async fn system_folders_can_not_be_synced() {
    #[cfg(target_os = "linux")]
    {
        let rig = Rig::new();
        let err = rig.app.engine.add_pair("/usr/share/doc", "Docs").unwrap_err();
        assert!(err.contains("system folder"), "{err}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_synced_folder_moved_into_the_koofr_folder_is_followed_not_copied() {
    let rig = rig_with_base().await;
    let outside = rig.dir.path().join("elsewhere/Music");
    write(&outside.join("song.mp3"), "la");
    let pair = rig.app.engine.add_pair(&outside.to_string_lossy(), "Music").unwrap();
    rig.app.engine.drain().await;
    std::fs::rename(&outside, rig.pc("Music")).unwrap();
    rig.app.folders.scan().await;
    let moved = rig.app.core.pair(&pair.id).unwrap();
    assert_eq!(moved.local, rig.pc("Music").to_string_lossy());
    assert_eq!(moved.remote, "Music");
    assert_eq!(rig.app.core.settings().pairs.len(), 1);
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&pair.id), Phase::Ok, "{}", rig.dump(&pair.id));
}
