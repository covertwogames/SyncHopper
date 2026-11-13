#![cfg(unix)]
//! How the app behaves when Koofr has problems: an outage blames no file, a
//! few files Koofr can't send are retried quietly, files that can never sync
//! can be ignored, and ignored files can be brought back.

mod common;
use common::*;
use synchopper_lib::engine::{Phase, FLAG_AFTER};
use synchopper_lib::util::now_secs;

#[tokio::test(flavor = "multi_thread")]
async fn a_koofr_outage_blames_no_files_and_recovers_by_itself() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));

    set_flag(&rig, "outage", Some(""));
    write(&rig.pc("Docs/b.txt"), "b");
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Waiting, "{}", rig.dump(&id));
    assert_eq!(v.waiting.as_deref(), Some("Koofr is having problems right now."));
    assert!(v.files.is_empty(), "no file is to blame for an outage");
    assert!(v.retry_at.unwrap() > now_secs());
    assert!(v.error.is_none());
    // One notification for the whole outage, and only once it has lasted a while.
    assert!(rig.host.notes.lock().unwrap().iter().all(|(t, _)| !t.contains("Koofr is having problems")));
    rig.sync(&id).await;
    rig.sync(&id).await;
    rig.sync(&id).await;
    let outage_notes = rig.host.notes.lock().unwrap().iter().filter(|(t, _)| t.contains("Koofr is having problems")).count();
    assert_eq!(outage_notes, 1);
    assert!(!rig.app.core.pair(&id).unwrap().needs_resync, "an outage must not force a rebuild");

    // Koofr comes back.
    set_flag(&rig, "outage", None);
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&id));
    assert!(v.waiting.is_none() && v.retry_at.is_none());
    assert_eq!(read(&rig.koofr("Docs/b.txt")), "b");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_few_files_koofr_cannot_send_are_retried_quietly() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;

    write(&rig.koofr("Docs/cloud.txt"), "from koofr");
    write(&rig.koofr("Docs/fine.txt"), "fine");
    write(&rig.pc("Docs/local.txt"), "from pc");
    set_flag(&rig, "fail.list", Some("cloud.txt|*koofr.Object|failed to open source object: Invalid response status! Got 500, expected [200 206]\n"));
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "the rest of the folder finishes: {}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/local.txt")), "from pc");
    assert_eq!(read(&rig.pc("Docs/fine.txt")), "fine");
    assert!(!rig.pc("Docs/cloud.txt").exists());
    assert_eq!(v.files.len(), 1);
    assert_eq!(v.files[0].kind, "server");
    assert!(!v.files[0].flagged, "retried quietly at first");
    assert!(v.files[0].reason.starts_with("Koofr couldn't send this file"), "{}", v.files[0].reason);
    assert!(v.files[0].next_retry >= now_secs() + 4 * 60, "next try in a few minutes, not every sync");

    // A normal sync before its turn does not retry it, and nothing is rebuilt.
    rig.sync(&id).await;
    assert_eq!(rig.view(&id).files.len(), 1);

    // Its turn comes and Koofr still can't send it: it stays left out with
    // its history, and the wait before the next try grows.
    let before = rig.app.core.pair(&id).unwrap().skipped[0].clone();
    make_retries_due(&rig, &id);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    let after = rig.app.core.pair(&id).unwrap().skipped[0].clone();
    assert_eq!(after.first_failed, before.first_failed);
    assert_eq!(after.tries, before.tries + 1);
    assert!(after.next_retry >= now_secs() + 14 * 60, "second wait is about 15 minutes");

    // After hours of failing it is shown as needing attention, once.
    rig.app.core.update_pair(&id, |p| p.skipped[0].first_failed = now_secs() - FLAG_AFTER - 10);
    rig.sync(&id).await;
    assert!(rig.view(&id).files[0].flagged);
    assert!(!rig.view(&id).files[0].can_ignore, "ignoring is only offered after a day");
    rig.sync(&id).await;
    let notes = rig.host.notes.lock().unwrap().iter().filter(|(t, _)| t.contains("needs attention")).count();
    assert_eq!(notes, 1, "{:?}", rig.host.notes.lock().unwrap());

    // Koofr can send it again: the next due retry brings it down.
    set_flag(&rig, "fail.list", None);
    make_retries_due(&rig, &id);
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&id));
    assert!(v.files.is_empty(), "{}", rig.dump(&id));
    assert_eq!(read(&rig.pc("Docs/cloud.txt")), "from koofr");
    assert_eq!(names(&rig.pc("Docs")), vec!["a.txt", "cloud.txt", "fine.txt", "local.txt"]);
    let log = std::fs::read_to_string(rig.app.core.log_path()).unwrap();
    assert_eq!(log.matches("rebuilding the sync record").count(), 1, "no rebuilds after the first sync");
}

#[tokio::test(flavor = "multi_thread")]
async fn when_nothing_at_all_comes_through_it_counts_as_an_outage() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    for n in 1..=3 {
        write(&rig.koofr(&format!("Docs/c{n}.txt")), "c");
    }
    let msg = "failed to open source object: Invalid response status! Got 500, expected [200 206]";
    set_flag(&rig, "fail.list", Some(&format!("c1.txt|*koofr.Object|{msg}\nc2.txt|*koofr.Object|{msg}\nc3.txt|*koofr.Object|{msg}\n")));
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Waiting, "{}", rig.dump(&id));
    assert!(v.files.is_empty(), "{}", rig.dump(&id));
    assert_eq!(rig.app.status_line(), "SyncHopper: waiting for Koofr");

    // If only those same files keep failing for about an hour, Koofr is
    // clearly up, so they become problem files and the folder carries on.
    for _ in 0..6 {
        rig.sync(&id).await;
    }
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(v.files.len(), 3);

    set_flag(&rig, "fail.list", None);
    make_retries_due(&rig, &id);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert!(rig.view(&id).files.is_empty(), "{}", rig.dump(&id));
    assert_eq!(names(&rig.pc("Docs")), vec!["a.txt", "c1.txt", "c2.txt", "c3.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_can_never_sync_can_be_ignored_and_brought_back() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    write(&rig.pc("Docs/long.txt"), "long name");
    set_flag(&rig, "fail.list", Some("long.txt|*local.Object|The filename or extension is too long.\n"));
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(v.files.len(), 1);
    assert!(v.files[0].flagged && v.files[0].can_ignore, "a file that can never sync is shown at once");
    assert!(v.files[0].reason.contains("too long"));

    // It is never retried by itself.
    make_retries_due(&rig, &id);
    rig.app.core.update_pair(&id, |p| p.skipped[0].next_retry = 0);
    rig.sync(&id).await;
    assert_eq!(rig.view(&id).files.len(), 1);

    rig.app.engine.ignore_file(&id, "long.txt").unwrap();
    let v = rig.view(&id);
    assert!(v.files.is_empty());
    assert_eq!(v.ignored, vec!["long.txt".to_string()]);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok);
    assert!(!rig.koofr("Docs/long.txt").exists(), "ignored files are not synced");
    let pre = std::fs::read_to_string(rig.app.core.log_path()).unwrap().matches("rebuilding the sync record").count();

    // Brought back after the name was fixed on Koofr's side, say.
    set_flag(&rig, "fail.list", None);
    rig.app.engine.unignore_file(&id, "long.txt").unwrap();
    rig.app.engine.drain().await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&id));
    assert!(v.ignored.is_empty() && v.files.is_empty(), "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/long.txt")), "long name");
    let post = std::fs::read_to_string(rig.app.core.log_path()).unwrap().matches("rebuilding the sync record").count();
    assert_eq!(pre, post, "bringing a file back does not rebuild the folder");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_transfer_is_retried_without_undoing_deletes() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    for n in 1..=5 {
        write(&rig.pc(&format!("Docs/f{n}.txt")), "x");
    }
    let id = rig.pair("Docs").await;

    std::fs::remove_file(rig.pc("Docs/f1.txt")).unwrap();
    write(&rig.pc("Docs/new.txt"), "new");
    write(&rig.pc("Docs/big.bin"), "big");
    set_flag(&rig, "fail.list", Some("big.bin|*local.Object|unexpected EOF\n"));
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert!(!rig.koofr("Docs/f1.txt").exists(), "the waiting delete was passed on");
    assert!(!rig.pc("Docs/f1.txt").exists(), "and the deleted file did not come back");
    assert_eq!(read(&rig.koofr("Docs/new.txt")), "new");
    assert!(!rig.koofr("Docs/big.bin").exists());
    assert_eq!(rig.view(&id).files.len(), 1);

    set_flag(&rig, "fail.list", None);
    make_retries_due(&rig, &id);
    rig.sync(&id).await;
    assert_eq!(read(&rig.koofr("Docs/big.bin")), "big");
    assert!(rig.view(&id).files.is_empty());
    let log = std::fs::read_to_string(rig.app.core.log_path()).unwrap();
    assert_eq!(log.matches("rebuilding the sync record").count(), 1);
}
