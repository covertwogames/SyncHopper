//! End-to-end tests of the sync engine against the real rclone program,
//! with a plain folder standing in for Koofr.

mod common;
use common::*;
use synchopper_lib::engine::{Approval, Job, Phase};

#[tokio::test(flavor = "multi_thread")]
async fn first_sync_copies_both_ways_and_deletes_nothing() {
    let rig = Rig::new();
    write(&rig.pc("Docs/local.txt"), "from pc");
    write(&rig.pc("Docs/sub/deep.txt"), "deep");
    write(&rig.koofr("Docs/cloud.txt"), "from koofr");
    let id = rig.pair("Docs").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    assert_eq!(read(&rig.koofr("Docs/local.txt")), "from pc");
    assert_eq!(read(&rig.koofr("Docs/sub/deep.txt")), "deep");
    assert_eq!(read(&rig.pc("Docs/cloud.txt")), "from koofr");
    assert!(rig.view(&id).last_sync > 0);
    assert!(!rig.app.core.pair(&id).unwrap().needs_resync);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_koofr_folder_is_created_on_first_sync() {
    let rig = Rig::new();
    write(&rig.pc("Brand New/a.txt"), "a");
    let id = rig.pair("Brand New").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    assert_eq!(read(&rig.koofr("Brand New/a.txt")), "a");
}

#[tokio::test(flavor = "multi_thread")]
async fn later_changes_and_small_deletes_travel_both_ways() {
    let rig = Rig::new();
    for n in 1..=6 {
        write(&rig.pc(&format!("Docs/f{n}.txt")), "x");
    }
    let id = rig.pair("Docs").await;
    tick().await;
    write(&rig.pc("Docs/new-local.txt"), "nl");
    write(&rig.koofr("Docs/new-cloud.txt"), "nc");
    write(&rig.pc("Docs/f1.txt"), "edited on pc");
    std::fs::remove_file(rig.pc("Docs/f2.txt")).unwrap();
    std::fs::remove_file(rig.koofr("Docs/f3.txt")).unwrap();
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    assert_eq!(read(&rig.koofr("Docs/new-local.txt")), "nl");
    assert_eq!(read(&rig.pc("Docs/new-cloud.txt")), "nc");
    assert_eq!(read(&rig.koofr("Docs/f1.txt")), "edited on pc");
    assert!(!rig.koofr("Docs/f2.txt").exists(), "delete on pc should reach koofr");
    assert!(!rig.pc("Docs/f3.txt").exists(), "delete in koofr should reach pc");
    assert_eq!(rig.view(&id).summary, "5 changes synced");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_delete_waits_for_approval_and_can_be_allowed() {
    let rig = Rig::new();
    for n in 1..=4 {
        write(&rig.pc(&format!("Docs/f{n}.txt")), "x");
    }
    let id = rig.pair("Docs").await;
    for n in 1..=3 {
        std::fs::remove_file(rig.pc(&format!("Docs/f{n}.txt"))).unwrap();
    }
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Approval);
    assert_eq!(rig.view(&id).approval, Some(Approval { deleted: 3, total: 4, side: "pc".into(), everything: false }));
    assert_eq!(names(&rig.koofr("Docs")).len(), 4, "nothing may be deleted before approval");
    assert!(rig.host.notes.lock().unwrap().iter().any(|(t, _)| t.contains("needs your OK")));

    // A plain "sync now" must not slip past the approval.
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Approval);
    assert_eq!(names(&rig.koofr("Docs")).len(), 4);

    rig.app.engine.approve_deletes(&id);
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    assert_eq!(names(&rig.koofr("Docs")), vec!["f4.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_delete_can_be_undone_instead() {
    let rig = Rig::new();
    for n in 1..=5 {
        write(&rig.pc(&format!("Docs/f{n}.txt")), "x");
    }
    let id = rig.pair("Docs").await;
    for n in 1..=4 {
        std::fs::remove_file(rig.koofr(&format!("Docs/f{n}.txt"))).unwrap();
    }
    rig.sync(&id).await;
    assert_eq!(rig.view(&id).approval, Some(Approval { deleted: 4, total: 5, side: "koofr".into(), everything: false }), "{}", rig.dump(&id));
    assert_eq!(names(&rig.pc("Docs")).len(), 5);
    rig.app.engine.restore_deleted(&id);
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(names(&rig.koofr("Docs")).len(), 5, "files should be copied back to koofr");
    assert_eq!(names(&rig.pc("Docs")).len(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_emptied_pc_folder_is_never_passed_on_automatically() {
    let rig = Rig::new();
    for n in 1..=3 {
        write(&rig.pc(&format!("Docs/f{n}.txt")), "x");
    }
    let id = rig.pair("Docs").await;
    for n in 1..=3 {
        std::fs::remove_file(rig.pc(&format!("Docs/f{n}.txt"))).unwrap();
    }
    rig.sync(&id).await;
    assert_eq!(rig.view(&id).approval, Some(Approval { deleted: 3, total: 3, side: "pc".into(), everything: true }), "{}", rig.dump(&id));
    assert_eq!(names(&rig.koofr("Docs")).len(), 3);
    let alerts = rig.host.notes.lock().unwrap().iter().filter(|(t, _)| t.contains("needs your OK")).count();

    // Approving is not possible for an emptied folder, and checking again
    // neither deletes anything nor nags a second time.
    rig.app.engine.approve_deletes(&id);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Approval);
    assert_eq!(names(&rig.koofr("Docs")).len(), 3);
    assert_eq!(rig.host.notes.lock().unwrap().iter().filter(|(t, _)| t.contains("needs your OK")).count(), alerts);

    // If the user empties the other side by hand, syncing carries on.
    for n in 1..=3 {
        std::fs::remove_file(rig.koofr(&format!("Docs/f{n}.txt"))).unwrap();
    }
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert!(rig.view(&id).approval.is_none());
    write(&rig.pc("Docs/again.txt"), "again");
    rig.sync(&id).await;
    assert_eq!(read(&rig.koofr("Docs/again.txt")), "again");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_emptied_folder_can_be_copied_back() {
    let rig = Rig::new();
    for n in 1..=3 {
        write(&rig.pc(&format!("Docs/f{n}.txt")), "x");
    }
    let id = rig.pair("Docs").await;
    for n in 1..=3 {
        std::fs::remove_file(rig.pc(&format!("Docs/f{n}.txt"))).unwrap();
    }
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Approval);
    rig.app.engine.restore_deleted(&id);
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(names(&rig.pc("Docs")).len(), 3);
    assert_eq!(names(&rig.koofr("Docs")).len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn conflicts_keep_both_copies_by_default() {
    let rig = Rig::new();
    write(&rig.pc("Docs/report.txt"), "v1");
    write(&rig.pc("Docs/other.txt"), "o");
    let id = rig.pair("Docs").await;
    tick().await;
    write(&rig.koofr("Docs/report.txt"), "older edit in koofr");
    tick().await;
    write(&rig.pc("Docs/report.txt"), "newer edit on pc");
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    assert_eq!(read(&rig.pc("Docs/report.txt")), "newer edit on pc");
    assert_eq!(read(&rig.koofr("Docs/report.txt")), "newer edit on pc");
    assert_eq!(read(&rig.pc("Docs/report.conflict1.txt")), "older edit in koofr");
    assert_eq!(read(&rig.koofr("Docs/report.conflict1.txt")), "older edit in koofr");
    assert_eq!(rig.view(&id).conflicts, vec!["report.txt".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn conflicts_newest_wins_when_chosen() {
    let rig = Rig::new();
    rig.app.core.update(|s| s.conflict_policy = "newest".into());
    write(&rig.pc("Docs/report.txt"), "v1");
    write(&rig.pc("Docs/other.txt"), "o");
    let id = rig.pair("Docs").await;
    tick().await;
    write(&rig.pc("Docs/report.txt"), "older edit on pc");
    tick().await;
    write(&rig.koofr("Docs/report.txt"), "newer edit in koofr");
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    assert_eq!(read(&rig.pc("Docs/report.txt")), "newer edit in koofr");
    assert_eq!(names(&rig.pc("Docs")), vec!["other.txt", "report.txt"]);
    assert_eq!(names(&rig.koofr("Docs")), vec!["other.txt", "report.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn ignored_files_stay_put_and_list_changes_take_effect() {
    let rig = Rig::new();
    write(&rig.pc("Docs/keep.txt"), "k");
    write(&rig.pc("Docs/~$keep.docx"), "lock");
    write(&rig.pc("Docs/scratch.tmp"), "t");
    write(&rig.pc("Docs/notes.bak"), "b");
    let id = rig.pair("Docs").await;
    assert_eq!(names(&rig.koofr("Docs")), vec!["keep.txt", "notes.bak"]);

    // Stop ignoring .tmp, start ignoring .bak.
    let patch = serde_json::json!({ "ignore_patterns": ["~$*", "*.bak"] });
    rig.app.update_settings(patch).await.unwrap();
    assert!(rig.app.core.pair(&id).unwrap().needs_resync);
    write(&rig.pc("Docs/second.bak"), "b2");
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    assert!(rig.koofr("Docs/scratch.tmp").exists(), "no longer ignored, so it syncs");
    assert!(!rig.koofr("Docs/second.bak").exists(), "now ignored");
    assert!(rig.koofr("Docs/notes.bak").exists(), "ignoring never deletes what is already there");
    assert!(!rig.koofr("Docs/~$keep.docx").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_folders_sync_cleanly_and_pick_up_first_files() {
    let rig = Rig::new();
    std::fs::create_dir_all(rig.pc("Empty")).unwrap();
    let id = rig.pair("Empty").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    write(&rig.koofr("Empty/first.txt"), "1");
    rig.sync(&id).await;
    assert_eq!(read(&rig.pc("Empty/first.txt")), "1");
    write(&rig.pc("Empty/second.txt"), "2");
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    assert_eq!(read(&rig.koofr("Empty/second.txt")), "2");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_pc_folder_is_an_error_and_koofr_is_untouched() {
    let rig = Rig::new();
    write(&rig.pc("Docs/a.txt"), "a");
    write(&rig.pc("Docs/b.txt"), "b");
    let id = rig.pair("Docs").await;
    std::fs::rename(rig.pc("Docs"), rig.pc("Docs-moved")).unwrap();
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Error);
    assert!(rig.view(&id).error.unwrap().contains("missing on this PC"));
    assert_eq!(names(&rig.koofr("Docs")), vec!["a.txt", "b.txt"]);
    // Putting it back heals the folder without help.
    std::fs::rename(rig.pc("Docs-moved"), rig.pc("Docs")).unwrap();
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vanished_koofr_folder_is_not_silently_recreated_and_pc_files_stay() {
    let rig = Rig::new();
    write(&rig.pc("Docs/a.txt"), "a");
    write(&rig.pc("Docs/b.txt"), "b");
    let id = rig.pair("Docs").await;
    std::fs::rename(rig.koofr("Docs"), rig.koofr("Renamed")).unwrap();
    for _ in 0..3 {
        rig.sync(&id).await;
        let v = rig.view(&id);
        assert!(v.phase == Phase::Approval || v.phase == Phase::Error, "{}", rig.dump(&id));
        if let Some(a) = &v.approval {
            assert!(a.everything && a.side == "koofr");
        }
        assert!(!rig.koofr("Docs").exists(), "must not re-upload on its own");
        assert_eq!(names(&rig.pc("Docs")), vec!["a.txt", "b.txt"], "pc files must be left alone");
    }
    // Copying back is the explicit way to build it again.
    rig.app.engine.restore_deleted(&id);
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(names(&rig.koofr("Docs")), vec!["a.txt", "b.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_stopped_midway_recovers_on_the_next_run() {
    let rig = Rig::new();
    rig.app.core.update(|s| {
        s.upload_limit_mbps = 1.0;
        s.download_limit_mbps = 1.0;
    });
    write(&rig.pc("Big/seed.txt"), "s");
    let id = rig.pair("Big").await;
    std::fs::write(rig.pc("Big/big1.bin"), vec![7u8; 6_000_000]).unwrap();
    std::fs::write(rig.pc("Big/big2.bin"), vec![9u8; 6_000_000]).unwrap();
    rig.app.engine.sync_now(Some(&id));
    let engine = rig.app.engine.clone();
    let runner = tokio::spawn(async move { engine.drain().await });
    let app = rig.app.clone();
    let pid = id.clone();
    let rig_dump = { let a = rig.app.clone(); let i = id.clone(); move || { let v = a.engine.pair_views().into_iter().find(|p| p.id == i).unwrap(); format!("{:?} {:?} {:?}", v.phase, v.error, v.details) } };
    let mut seen = false;
    for _ in 0..150 {
        if app.engine.pair_views().iter().any(|p| p.id == pid && p.phase == Phase::Syncing) { seen = true; break; }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(seen, "upload never showed as syncing: {} \n{}", rig_dump(), rig.dump(&id));
    assert!(rig.view(&id).progress.is_some());
    rig.app.engine.set_paused(&id, true);
    runner.await.unwrap();
    assert!(rig.view(&id).paused);
    assert!(!rig.koofr("Big/big1.bin").exists() || !rig.koofr("Big/big2.bin").exists(), "should have stopped early");

    rig.app.core.update(|s| {
        s.upload_limit_mbps = 0.0;
        s.download_limit_mbps = 0.0;
    });
    rig.app.engine.set_paused(&id, false);
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?} {:?}", rig.view(&id).error, rig.view(&id).details);
    assert_eq!(std::fs::metadata(rig.koofr("Big/big1.bin")).unwrap().len(), 6_000_000);
    assert_eq!(std::fs::metadata(rig.koofr("Big/big2.bin")).unwrap().len(), 6_000_000);
    assert_eq!(names(&rig.pc("Big")), vec!["big1.bin", "big2.bin", "seed.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn repair_rebuilds_the_record_without_deleting() {
    let rig = Rig::new();
    write(&rig.pc("Docs/a.txt"), "a");
    write(&rig.pc("Docs/b.txt"), "b");
    let id = rig.pair("Docs").await;
    std::fs::remove_file(rig.pc("Docs/a.txt")).unwrap();
    rig.app.engine.repair(&id);
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{:?}", rig.view(&id).error);
    assert_eq!(names(&rig.pc("Docs")), vec!["a.txt", "b.txt"], "repair copies back, never deletes");
    assert_eq!(names(&rig.koofr("Docs")), vec!["a.txt", "b.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_folders_are_refused() {
    let rig = Rig::new();
    let _ = rig.pair("Docs").await;
    let e = &rig.app.engine;
    let docs = rig.pc("Docs").to_string_lossy().to_string();
    let inner = rig.pc("Docs/Inner").to_string_lossy().to_string();
    let other = rig.pc("Other").to_string_lossy().to_string();
    assert!(e.add_pair(&docs, "Elsewhere").unwrap_err().contains("overlaps"));
    assert!(e.add_pair(&inner, "Elsewhere").unwrap_err().contains("overlaps"));
    assert!(e.add_pair(&other, "Docs/Sub").unwrap_err().contains("overlaps"));
    assert!(e.add_pair(&other, "docs").unwrap_err().contains("overlaps"));
    assert!(e.add_pair(&other, "").unwrap_err().contains("overlaps"));
    assert!(e.add_pair("relative/path", "X").is_err());
    assert!(e.add_pair("/", "X").is_err());
    let data = rig.app.core.data.join("inside").to_string_lossy().to_string();
    assert!(e.add_pair(&data, "X").unwrap_err().contains("own files"));
    assert!(e.add_pair(&other, "Docs2").is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_folder_leaves_files_on_both_sides() {
    let rig = Rig::new();
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    rig.app.engine.remove_pair(&id);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(rig.app.engine.pair_views().is_empty());
    assert!(rig.pc("Docs/a.txt").exists());
    assert!(rig.koofr("Docs/a.txt").exists());
    assert!(!rig.app.core.pair_dir(&id).exists(), "its sync record should be cleaned up");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_background_loop_syncs_local_changes_and_then_settles() {
    let rig = Rig::new();
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    rig.app.engine.start();
    let app = rig.app.clone();
    let pid = id.clone();
    wait_for("the engine to be idle", 30, move || app.engine.run_count(&pid) >= 1 && !app.engine.busy()).await;

    // A change on this PC is picked up by the watcher, not the timer.
    write(&rig.pc("Docs/typed.txt"), "typed");
    let target = rig.koofr("Docs/typed.txt");
    wait_for("the new file to reach koofr", 30, move || target.exists()).await;

    // A change in koofr arrives when asked, and the download must not set
    // off an endless chain of follow-up syncs.
    write(&rig.koofr("Docs/cloud.txt"), "cloud");
    rig.app.engine.sync_now(Some(&id));
    let target = rig.pc("Docs/cloud.txt");
    wait_for("the cloud file to arrive", 30, move || target.exists()).await;
    // One follow-up check after a download is expected (the download itself
    // looks like a local change). After that it has to go quiet.
    tokio::time::sleep(std::time::Duration::from_secs(24)).await;
    let before = rig.app.engine.run_count(&id);
    tokio::time::sleep(std::time::Duration::from_secs(22)).await;
    assert_eq!(rig.app.engine.run_count(&id), before, "syncing should go quiet when nothing changes");
    assert_eq!(rig.phase(&id), Phase::Ok);
}

#[tokio::test(flavor = "multi_thread")]
async fn paused_folders_are_skipped_until_resumed() {
    let rig = Rig::new();
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    rig.app.engine.set_paused(&id, true);
    write(&rig.pc("Docs/b.txt"), "b");
    rig.app.engine.sync_now(None);
    rig.app.engine.enqueue(Job { pair_id: id.clone(), ..Default::default() });
    rig.app.engine.drain().await;
    assert!(!rig.koofr("Docs/b.txt").exists());
    rig.app.engine.set_paused(&id, false);
    rig.app.engine.drain().await;
    assert!(rig.koofr("Docs/b.txt").exists());
}
