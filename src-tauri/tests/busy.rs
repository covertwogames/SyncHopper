#![cfg(unix)]
//! Files that can't be read must not hold up the rest of their folder, must
//! sync by themselves once they can be read again, and must finish the way a
//! normal sync would have: no stray conflict copies, no deleted files coming
//! back.
//!
//! Making a file unreadable to the sync engine needs the engine to run as an
//! unprivileged user, which needs root. These tests are skipped otherwise.

mod common;
use common::*;
use synchopper_lib::engine::Phase;

fn lock(rig: &Rig, paths: &[&std::path::Path]) {
    let text: String = paths.iter().map(|p| format!("{}\n", p.display())).collect();
    set_flag(rig, "locked.list", Some(&text));
}

fn unlock(rig: &Rig) {
    set_flag(rig, "locked.list", None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_file_is_left_out_then_synced_once_readable() {
    let rig = Rig::new();
    if fake_engine(&rig, true).is_none() {
        eprintln!("skipped: needs root, setpriv and rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    write(&rig.pc("Docs/b.txt"), "b");
    let id = rig.pair("Docs").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));

    // One file the engine cannot open, next to an ordinary new file and a
    // delete that is waiting to be passed on.
    let locked = rig.pc("Docs/mail.pst");
    write(&locked, "locked content");
    write(&rig.pc("Docs/c.txt"), "c");
    std::fs::remove_file(rig.pc("Docs/b.txt")).unwrap();
    lock(&rig, &[&locked]);

    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/c.txt")), "c", "the rest of the folder must not be held up");
    assert!(!rig.koofr("Docs/mail.pst").exists());
    assert!(!rig.koofr("Docs/b.txt").exists(), "the waiting delete must still be passed on: {}", rig.dump(&id));
    assert!(!rig.pc("Docs/b.txt").exists(), "a deleted file must not come back: {}", rig.dump(&id));
    let files = rig.view(&id).files;
    assert_eq!(files.len(), 1, "{}", rig.dump(&id));
    assert_eq!(files[0].path, "mail.pst");
    assert_eq!(files[0].kind, "denied");
    assert!(!files[0].flagged, "a fresh failure is retried quietly, not flagged");
    assert!(files[0].reason.contains("wouldn't let SyncHopper read"), "{}", files[0].reason);

    // With the trouble file set aside, ordinary syncing works, deletes included,
    // and the folder is not rebuilt each time.
    write(&rig.pc("Docs/d.txt"), "d");
    std::fs::remove_file(rig.koofr("Docs/a.txt")).unwrap();
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/d.txt")), "d");
    assert!(!rig.pc("Docs/a.txt").exists(), "a delete in koofr should still reach the pc");
    assert_eq!(read(&locked), "locked content", "the trouble file itself is never touched");

    // The file becomes readable. "Try them again" brings it across.
    unlock(&rig);
    rig.app.engine.retry_skipped(&id);
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert!(rig.view(&id).files.is_empty(), "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/mail.pst")), "locked content");
    let log = std::fs::read_to_string(rig.app.core.log_path()).unwrap();
    assert_eq!(log.matches("rebuilding the sync record").count(), 1, "only the very first sync builds the record");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_changed_while_locked_syncs_without_a_conflict_copy() {
    let rig = Rig::new();
    if fake_engine(&rig, true).is_none() {
        eprintln!("skipped: needs root, setpriv and rclone");
        return;
    }
    write(&rig.pc("Docs/report.txt"), "v1");
    write(&rig.pc("Docs/other.txt"), "o");
    let id = rig.pair("Docs").await;
    tick().await;

    // Edited while another program holds it open.
    write(&rig.pc("Docs/report.txt"), "v2, edited while open");
    lock(&rig, &[&rig.pc("Docs/report.txt")]);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/report.txt")), "v1");
    assert_eq!(rig.view(&id).files.len(), 1, "{}", rig.dump(&id));

    // Closed again. Its turn for a retry comes up on an ordinary sync.
    unlock(&rig);
    make_retries_due(&rig, &id);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert!(rig.view(&id).files.is_empty(), "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/report.txt")), "v2, edited while open");
    assert_eq!(names(&rig.pc("Docs")), vec!["other.txt", "report.txt"], "no conflict copy on the pc");
    assert_eq!(names(&rig.koofr("Docs")), vec!["other.txt", "report.txt"], "no conflict copy in koofr");
    assert!(rig.view(&id).conflicts.is_empty());

    // And it keeps syncing normally afterwards.
    tick().await;
    write(&rig.koofr("Docs/report.txt"), "v3 from koofr");
    rig.sync(&id).await;
    assert_eq!(read(&rig.pc("Docs/report.txt")), "v3 from koofr");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_deleted_while_left_out_is_not_brought_back() {
    let rig = Rig::new();
    if fake_engine(&rig, true).is_none() {
        eprintln!("skipped: needs root, setpriv and rclone");
        return;
    }
    write(&rig.pc("Docs/draft.txt"), "v1");
    write(&rig.pc("Docs/keep.txt"), "k");
    let id = rig.pair("Docs").await;
    tick().await;
    write(&rig.pc("Docs/draft.txt"), "v2");
    lock(&rig, &[&rig.pc("Docs/draft.txt")]);
    rig.sync(&id).await;
    assert_eq!(rig.view(&id).files.len(), 1, "{}", rig.dump(&id));

    // The user throws the draft away before it ever syncs.
    unlock(&rig);
    std::fs::remove_file(rig.pc("Docs/draft.txt")).unwrap();
    make_retries_due(&rig, &id);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert!(rig.view(&id).files.is_empty(), "{}", rig.dump(&id));
    assert!(!rig.pc("Docs/draft.txt").exists(), "a deleted file must not come back from koofr");
    assert!(!rig.koofr("Docs/draft.txt").exists(), "the delete is passed on to koofr");
    assert_eq!(read(&rig.koofr("Docs/keep.txt")), "k");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_still_locked_file_is_not_retried_and_stays_quiet() {
    let rig = Rig::new();
    if fake_engine(&rig, true).is_none() {
        eprintln!("skipped: needs root, setpriv and rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    let locked = rig.pc("Docs/mail.pst");
    write(&locked, "x");
    lock(&rig, &[&locked]);
    rig.sync(&id).await;
    assert_eq!(rig.view(&id).files.len(), 1);
    let first_failed = rig.app.core.pair(&id).unwrap().skipped[0].first_failed;
    make_retries_due(&rig, &id);
    rig.sync(&id).await;
    make_retries_due(&rig, &id);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    let files = rig.view(&id).files;
    assert_eq!(files.len(), 1);
    assert!(!files[0].flagged);
    assert!(rig.host.notes.lock().unwrap().iter().all(|(t, _)| !t.contains("need")));
    // Failing again keeps the file's history, so trouble that lasts is noticed.
    let k = rig.app.core.pair(&id).unwrap().skipped[0].clone();
    assert_eq!(k.first_failed, first_failed);
    assert!(k.tries >= 3, "tries {}", k.tries);
}
