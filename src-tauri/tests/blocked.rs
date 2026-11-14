#![cfg(unix)]
//! When the system refuses to run the sync engine (on Windows: Smart App
//! Control, other application control policies, antivirus), SyncHopper says
//! so once, for everything, instead of reporting every folder as failed.
//! Here a block is simulated by taking away permission to run the engine.

mod common;
use common::*;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use synchopper_lib::app::EngineCheck;
use synchopper_lib::config::FileState;
use synchopper_lib::engine::Phase;
use synchopper_lib::rclone;

fn runnable(path: &Path, yes: bool) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(if yes { 0o755 } else { 0o644 })).unwrap();
}

fn notes(rig: &Rig) -> Vec<(String, String)> {
    rig.host.notes.lock().unwrap().clone()
}

fn blocked_notes(rig: &Rig) -> usize {
    notes(rig).iter().filter(|(t, _)| t == "SyncHopper can't sync").count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_engine_is_reported_once_and_no_folder_or_file_is_blamed() {
    let rig = Rig::new();
    let Some(engine) = fake_engine(&rig, false) else {
        eprintln!("skipped: needs rclone");
        return;
    };
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));

    // A file that synced before, changed in Koofr, and that Koofr can't
    // send right now, waiting for its retry. Its retry runs the engine.
    write(&rig.koofr("Docs/cloud.txt"), "v1");
    rig.sync(&id).await;
    assert_eq!(read(&rig.pc("Docs/cloud.txt")), "v1", "{}", rig.dump(&id));
    tick().await;
    write(&rig.koofr("Docs/cloud.txt"), "v2");
    set_flag(&rig, "fail.list", Some("cloud.txt|*koofr.Object|failed to open source object: Invalid response status! Got 500, expected [200 206]\n"));
    rig.sync(&id).await;
    assert_eq!(rig.view(&id).files.len(), 1, "{}", rig.dump(&id));
    // The test engine doesn't leave rclone's records behind, so give the
    // file the shared state a real sync would have recorded.
    rig.app.core.update_pair(&id, |p| p.skipped[0].base_pc = Some(FileState { size: 2, mtime_ns: 1 }));
    let waiting_file = rig.app.core.pair(&id).unwrap().skipped[0].clone();

    // Now the engine is blocked, right when that file's retry comes up.
    runnable(&engine, false);
    make_retries_due(&rig, &id);
    rig.sync(&id).await;

    let e = rig.app.core.engine_info();
    assert_eq!(e.state, "blocked", "{e:?}");
    assert_eq!(e.message, rclone::BLOCKED_TEXT);
    assert!(e.exe.is_none());
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "the folder keeps its state: {}", rig.dump(&id));
    assert!(v.error.is_none(), "{}", rig.dump(&id));
    // The waiting file is left exactly as it was: the block isn't its fault.
    let after = rig.app.core.pair(&id).unwrap().skipped[0].clone();
    assert_eq!((after.tries, after.kind.as_str(), after.detail.as_str()), (waiting_file.tries, waiting_file.kind.as_str(), waiting_file.detail.as_str()));
    assert_eq!(blocked_notes(&rig), 1, "{:?}", notes(&rig));
    assert!(notes(&rig).iter().all(|(t, _)| !t.contains("could not sync")), "{:?}", notes(&rig));
    assert_eq!(rig.app.status_line(), "SyncHopper: can't sync right now");

    // More syncs, and a new folder, while blocked: nothing new is reported.
    rig.sync(&id).await;
    let pics = rig.app.engine.add_pair(&rig.pc("Pics").to_string_lossy(), "Pics").unwrap().id;
    std::fs::create_dir_all(rig.pc("Pics")).unwrap();
    rig.app.engine.sync_now(Some(&pics));
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&pics), Phase::Idle, "{}", rig.dump(&pics));
    assert!(rig.view(&pics).error.is_none(), "{}", rig.dump(&pics));
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(blocked_notes(&rig), 1, "{:?}", notes(&rig));
    assert!(notes(&rig).iter().all(|(t, _)| !t.contains("could not sync")), "{:?}", notes(&rig));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_engine_is_retried_gently_and_recovers() {
    let rig = Rig::new();
    let data = rig.app.core.data.clone();
    let exe = rclone::managed_path(&data);
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/engine/rclone-v1.76.0-linux-amd64"), &exe).unwrap();
    runnable(&exe, false);

    rig.app.ensure_engine().await;
    let e = rig.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.message.as_str()), ("blocked", rclone::BLOCKED_TEXT), "{e:?}");
    assert!(e.exe.is_none());
    assert_eq!(blocked_notes(&rig), 1);
    // The window offers Try again for this state.
    assert_eq!(rig.app.snapshot()["engine"]["state"], "blocked");

    // Starting again, or the regular check, doesn't nag.
    rig.app.ensure_engine().await;
    rig.app.check_engine(EngineCheck::Background).await;
    assert_eq!(rig.app.core.engine_info().state, "blocked");
    assert_eq!(blocked_notes(&rig), 1);

    // The block is lifted (for example the antivirus now allows it). The
    // regular check doesn't run the same copy again right away, because on
    // Windows every try can pop up a security warning...
    runnable(&exe, true);
    rig.app.check_engine(EngineCheck::Background).await;
    assert_eq!(rig.app.core.engine_info().state, "blocked");

    // ...but Try again does.
    rig.app.check_engine(EngineCheck::Manual).await;
    let e = rig.app.core.engine_info();
    assert_eq!((e.state.as_str(), e.version.as_str()), ("ready", "1.76.0"), "{e:?}");
    assert_eq!(e.exe.as_deref(), Some(exe.as_path()));
    assert!(e.message.is_empty());

    // It syncs again.
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/a.txt")), "a");
    assert_eq!(rig.app.status_line(), "SyncHopper: up to date");

    // A later block is worth telling about again.
    runnable(&exe, false);
    write(&rig.pc("Docs/b.txt"), "b");
    rig.sync(&id).await;
    assert_eq!(rig.app.core.engine_info().state, "blocked");
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(blocked_notes(&rig), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_engine_is_not_mistaken_for_a_blocked_one() {
    let rig = Rig::with_engine("/nowhere/rclone");
    write(&rig.pc("Docs/a.txt"), "a");
    let pair = rig.app.engine.add_pair(&rig.pc("Docs").to_string_lossy(), "Docs").unwrap();
    rig.app.engine.drain().await;
    assert_ne!(rig.app.core.engine_info().state, "blocked");
    assert_eq!(rig.phase(&pair.id), Phase::Error, "{}", rig.dump(&pair.id));
    assert_eq!(blocked_notes(&rig), 0);
}
