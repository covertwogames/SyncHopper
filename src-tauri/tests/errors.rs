#![cfg(unix)]
//! Problems added in 0.3.2: Koofr turning requests away, secure connections
//! that fail, drives that can't be reached, every file looking changed,
//! sync records found out of step, and names Koofr refuses.

mod common;
use common::*;
use synchopper_lib::engine::{Approval, Phase, UNEXPECTED};

fn notes_titled(rig: &Rig, title: &str) -> usize {
    rig.host.notes.lock().unwrap().iter().filter(|(t, _)| t == title).count()
}

fn args_log(rig: &Rig) -> String {
    std::fs::read_to_string(rig.dir.path().join("args.log")).unwrap_or_default()
}

fn log(rig: &Rig) -> String {
    std::fs::read_to_string(rig.app.core.log_path()).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn koofr_turning_requests_away_waits_and_points_to_the_setting() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));

    set_flag(&rig, "listing_error", Some("error reading destination root directory: Invalid response status! Got 429, expected [200 206]; headers: map[Content-Length:[17]], content: Too Many Requests"));
    write(&rig.pc("Docs/b.txt"), "b");
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Waiting, "{}", rig.dump(&id));
    assert_eq!(v.waiting.as_deref(), Some("Koofr says SyncHopper is sending too many requests."));
    assert_eq!(v.waiting_kind.as_deref(), Some("throttled"));
    assert!(v.files.is_empty() && v.error.is_none());
    assert_eq!(rig.app.status_line(), "SyncHopper: waiting for Koofr");

    // One notification once it has gone on for a while, pointing to the setting.
    let title = "Koofr says SyncHopper is sending too many requests";
    assert_eq!(notes_titled(&rig, title), 0);
    rig.sync(&id).await;
    rig.sync(&id).await;
    rig.sync(&id).await;
    assert_eq!(notes_titled(&rig, title), 1);
    let body = rig.host.notes.lock().unwrap().iter().find(|(t, _)| t == title).unwrap().1.clone();
    assert!(body.contains("turn on Limit requests to Koofr in Advanced settings"), "{body}");

    // With the limit already on, it just says Koofr is busy.
    rig.app.core.update(|s| s.limit_requests = true);
    rig.sync(&id).await;
    assert_eq!(rig.view(&id).waiting.as_deref(), Some("Koofr is busy right now."));
    assert_eq!(notes_titled(&rig, "Koofr is busy right now."), 0, "already told about this wait");

    // Koofr lets requests through again. The limit reaches the engine.
    set_flag(&rig, "listing_error", None);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/b.txt")), "b");
    let last_bisync = args_log(&rig).lines().rev().find(|l| l.starts_with("bisync ")).unwrap().to_string();
    assert!(last_bisync.contains("--tpslimit 40"), "{last_bisync}");

    // A new spell of it is told about again.
    set_flag(&rig, "listing_error", Some("Invalid response status! Got 429, expected [200 206]"));
    for _ in 0..3 {
        rig.sync(&id).await;
    }
    assert_eq!(notes_titled(&rig, "Koofr is busy right now."), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_limit_is_only_used_when_turned_on() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    let before = args_log(&rig);
    assert!(before.lines().any(|l| l.starts_with("mkdir ")), "{before}");
    assert!(before.lines().any(|l| l.starts_with("bisync ")), "{before}");
    assert!(!before.contains("--tpslimit"), "{before}");

    rig.app.core.update(|s| s.limit_requests = true);
    write(&rig.pc("Docs/b.txt"), "b");
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    let after: Vec<String> = args_log(&rig).lines().skip(before.lines().count()).map(|l| l.to_string()).collect();
    assert!(!after.is_empty());
    for line in &after {
        assert!(line.ends_with("--tpslimit 40"), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secure_connection_problem_waits_with_the_likely_cause() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;

    set_flag(&rig, "listing_error", Some("Get https://app.koofr.net/api/v2/mounts: tls: failed to verify certificate: x509: certificate has expired or is not yet valid: current time 2019-01-01T00:00:00Z is before 2026-01-01T00:00:00Z"));
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Waiting, "{}", rig.dump(&id));
    assert_eq!(v.waiting_kind.as_deref(), Some("cert"));
    assert_eq!(v.waiting.as_deref(), Some("SyncHopper couldn't connect securely to Koofr. Check that this PC's date and time are correct."));
    rig.sync(&id).await;
    rig.sync(&id).await;
    assert_eq!(notes_titled(&rig, "SyncHopper can't connect securely to Koofr"), 1);

    set_flag(&rig, "listing_error", Some("Get https://app.koofr.net/api/v2/mounts: tls: failed to verify certificate: x509: certificate signed by unknown authority"));
    rig.sync(&id).await;
    let w = rig.view(&id).waiting.unwrap();
    assert!(w.contains("public Wi-Fi sign-in page, antivirus that scans secure connections, or a work network"), "{w}");
    assert_eq!(notes_titled(&rig, "SyncHopper can't connect securely to Koofr"), 1);

    set_flag(&rig, "listing_error", None);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_drive_that_cannot_be_reached_makes_the_folder_wait() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;

    set_flag(&rig, "listing_error", Some("error reading source root directory: open E:\\\\Docs: The device is not ready."));
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Waiting, "{}", rig.dump(&id));
    assert_eq!(v.waiting_kind.as_deref(), Some("drive"));
    assert_eq!(v.waiting.as_deref(), Some("Windows can't reach the drive this folder is on. It may be unplugged."));
    assert_eq!(rig.app.status_line(), "SyncHopper: waiting for a drive");
    rig.sync(&id).await;
    rig.sync(&id).await;
    rig.sync(&id).await;
    assert_eq!(notes_titled(&rig, "Docs can't sync right now"), 1);
    set_flag(&rig, "listing_error", None);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));

    // The same from one file: nothing is left out, the folder just waits.
    set_flag(&rig, "fail.list", Some("b.txt|*local.Object|failed to open source object: open E:\\\\Docs\\\\b.txt: The media is write protected.\n"));
    write(&rig.pc("Docs/b.txt"), "b");
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Waiting, "{}", rig.dump(&id));
    assert_eq!(v.waiting.as_deref(), Some("The drive this folder is on is write-protected, so SyncHopper can't save files there."));
    assert!(v.files.is_empty(), "{}", rig.dump(&id));
    set_flag(&rig, "fail.list", None);
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/b.txt")), "b");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_few_changed_files_sync_and_many_wait_for_an_ok() {
    let rig = Rig::new();
    // A folder where a person edited every file: synced without asking.
    for i in 0..3 {
        write(&rig.pc(&format!("Docs/f{i}.txt")), "old");
    }
    let id = rig.pair("Docs").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    tick().await;
    for i in 0..3 {
        write(&rig.pc(&format!("Docs/f{i}.txt")), "new, longer");
    }
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert!(rig.view(&id).approval.is_none());
    for i in 0..3 {
        assert_eq!(read(&rig.koofr(&format!("Docs/f{i}.txt"))), "new, longer");
    }
    assert!(log(&rig).contains("Every file looked changed (3 files). Syncing them anyway."));

    // Every one of many files changed: the user is asked first.
    for i in 0..25 {
        write(&rig.pc(&format!("Big/f{i:02}.txt")), "old");
    }
    let big = rig.pair("Big").await;
    assert_eq!(rig.phase(&big), Phase::Ok, "{}", rig.dump(&big));
    tick().await;
    for i in 0..25 {
        write(&rig.pc(&format!("Big/f{i:02}.txt")), "new, longer");
    }
    rig.sync(&big).await;
    let v = rig.view(&big);
    assert_eq!(v.approval, Some(Approval { deleted: 25, total: 25, side: "pc".into(), everything: false, changed: true }), "{}", rig.dump(&big));
    assert_eq!(read(&rig.koofr("Big/f00.txt")), "old");
    assert_eq!(notes_titled(&rig, "Big needs your OK"), 1);
    // The large-delete OK doesn't count for this.
    rig.app.engine.approve_deletes(&big);
    rig.app.engine.drain().await;
    assert!(rig.view(&big).approval.is_some());
    // Another sync before the user answers doesn't ask again.
    rig.sync(&big).await;
    assert_eq!(notes_titled(&rig, "Big needs your OK"), 1);

    rig.app.engine.approve_changed(&big);
    rig.app.engine.drain().await;
    let v = rig.view(&big);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&big));
    assert!(v.approval.is_none());
    for i in 0..25 {
        assert_eq!(read(&rig.koofr(&format!("Big/f{i:02}.txt"))), "new, longer");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_safety_checks_are_off_for_one_run_only() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    for i in 0..3 {
        write(&rig.pc(&format!("Docs/f{i}.txt")), "old");
    }
    let id = rig.pair("Docs").await;
    tick().await;
    for i in 0..3 {
        write(&rig.pc(&format!("Docs/f{i}.txt")), "new, longer");
    }
    // One file fails in the run that syncs the rest, so another run follows.
    set_flag(&rig, "fail.list", Some("f1.txt|*local.Object|failed to open source object: Invalid response status! Got 500, expected [200 206]\n"));
    let before = args_log(&rig).lines().count();
    rig.sync(&id).await;
    let runs: Vec<String> = args_log(&rig).lines().skip(before).filter(|l| l.starts_with("bisync ")).map(|l| l.to_string()).collect();
    assert_eq!(runs.len(), 3, "{runs:?}");
    assert!(!runs[0].contains("--force"), "{}", runs[0]);
    assert!(runs[1].contains("--force"), "{}", runs[1]);
    assert!(!runs[2].contains("--force"), "{}", runs[2]);
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/f0.txt")), "new, longer");
    assert_eq!(rig.view(&id).files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["f1.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ok_for_changed_files_still_checks_for_a_large_delete() {
    let rig = Rig::new();
    for i in 0..25 {
        write(&rig.pc(&format!("Big/f{i:02}.txt")), "old");
    }
    let id = rig.pair("Big").await;
    tick().await;
    for i in 0..25 {
        write(&rig.pc(&format!("Big/f{i:02}.txt")), "new, longer");
    }
    rig.sync(&id).await;
    assert!(rig.view(&id).approval.as_ref().is_some_and(|a| a.changed), "{}", rig.dump(&id));
    // Before answering, most of the files are deleted.
    for i in 0..20 {
        std::fs::remove_file(rig.pc(&format!("Big/f{i:02}.txt"))).unwrap();
    }
    rig.app.engine.approve_changed(&id);
    rig.app.engine.drain().await;
    let v = rig.view(&id);
    assert_eq!(v.approval, Some(Approval { deleted: 20, total: 25, side: "pc".into(), everything: false, changed: false }), "{}", rig.dump(&id));
    assert_eq!(names(&rig.koofr("Big")).len(), 25, "nothing deleted in Koofr without asking");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_record_out_of_step_is_rebuilt_quietly_but_not_over_and_over() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    let oos = "path1 and path2 are out of sync, run --resync to recover";

    set_flag(&rig, "listing_error_once", Some(oos));
    write(&rig.pc("Docs/b.txt"), "b");
    let lines_before = args_log(&rig).lines().count();
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/b.txt")), "b");
    let new: Vec<String> = args_log(&rig).lines().skip(lines_before).map(|l| l.to_string()).collect();
    assert_eq!(new.iter().filter(|l| l.starts_with("bisync ")).count(), 2, "{new:?}");
    assert!(new.last().unwrap().contains("--resync"), "{new:?}");
    assert!(rig.host.notes.lock().unwrap().iter().all(|(t, _)| !t.contains("could not sync")));

    // Straight away again, before an ordinary sync has worked: it stops.
    set_flag(&rig, "listing_error_once", Some(oos));
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Error, "{}", rig.dump(&id));
    assert_eq!(v.error.as_deref(), Some(UNEXPECTED));
    assert!(v.details.iter().any(|d| d.contains("out of sync")), "{:?}", v.details);

    // An ordinary sync works, so a later one is rebuilt quietly again.
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    set_flag(&rig, "listing_error_once", Some(oos));
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
}

#[tokio::test(flavor = "multi_thread")]
async fn names_koofr_refuses_are_left_out_until_renamed() {
    let rig = Rig::new();
    write(&rig.pc("Docs/a.txt"), "a");
    write(&rig.pc("Docs/beach \u{1F3D6}.jpg"), "sand");
    write(&rig.pc("Docs/Party \u{1F389}/x.jpg"), "cake");
    std::fs::create_dir_all(rig.pc("Docs/empty \u{1F4E6}")).unwrap();
    let id = rig.pair("Docs").await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&id));
    let mut left: Vec<(String, String, bool, bool)> = v.files.iter().map(|f| (f.path.clone(), f.kind.clone(), f.flagged, f.auto_retry)).collect();
    left.sort();
    assert_eq!(
        left,
        vec![
            ("Party \u{1F389}/".to_string(), "koofr_name".to_string(), true, false),
            ("beach \u{1F3D6}.jpg".to_string(), "koofr_name".to_string(), true, false),
            ("empty \u{1F4E6}/".to_string(), "koofr_name".to_string(), true, false),
        ]
    );
    let beach = v.files.iter().find(|f| f.path.starts_with("beach")).unwrap();
    assert_eq!(beach.reason, "Koofr doesn't allow emoji and some other symbols in file names. Rename this file to sync it.");
    assert_eq!(v.files.iter().find(|f| f.path.starts_with("Party")).unwrap().reason, "Koofr doesn't allow emoji and some other symbols in folder names. Rename this folder to sync it.");
    // None of them reached "Koofr"; everything else did.
    assert_eq!(names(&rig.koofr("Docs")), vec!["a.txt"]);
    assert_eq!(notes_titled(&rig, "Docs: 3 files need attention"), 1);

    // Trying again doesn't change anything, or announce them again.
    rig.app.engine.retry_skipped(&id);
    rig.app.engine.drain().await;
    assert_eq!(rig.view(&id).files.len(), 3, "{}", rig.dump(&id));
    assert_eq!(names(&rig.koofr("Docs")), vec!["a.txt"]);
    assert_eq!(rig.host.notes.lock().unwrap().iter().filter(|(t, _)| t.contains("need")).count(), 1);

    // Renamed: they drop off the list and sync.
    std::fs::rename(rig.pc("Docs/beach \u{1F3D6}.jpg"), rig.pc("Docs/beach.jpg")).unwrap();
    std::fs::rename(rig.pc("Docs/Party \u{1F389}"), rig.pc("Docs/Party")).unwrap();
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(v.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["empty \u{1F4E6}/"]);
    assert_eq!(read(&rig.koofr("Docs/beach.jpg")), "sand");
    assert_eq!(read(&rig.koofr("Docs/Party/x.jpg")), "cake");

    // Ignored: it stays ignored.
    rig.app.engine.ignore_file(&id, "empty \u{1F4E6}/").unwrap();
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert!(v.files.is_empty(), "{}", rig.dump(&id));
    assert_eq!(v.ignored, vec!["empty \u{1F4E6}/".to_string()]);
    assert_eq!(names(&rig.koofr("Docs")), vec!["Party", "a.txt", "beach.jpg"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_name_koofr_refuses_is_explained_up_front() {
    let rig = Rig::new();
    std::fs::create_dir_all(rig.pc("Fun \u{1F389}")).unwrap();
    let err = rig.app.engine.add_pair(&rig.pc("Fun \u{1F389}").to_string_lossy(), "Fun \u{1F389}").unwrap_err();
    assert!(err.starts_with("Koofr doesn't allow emoji"), "{err}");
    // Its own Koofr name is fine.
    rig.app.engine.add_pair(&rig.pc("Fun \u{1F389}").to_string_lossy(), "Fun").unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn file_problems_get_their_own_reasons_and_retry_times() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    for name in ["ok.txt", "virus.txt", "refused.txt", "odd.txt", "locked.pst"] {
        write(&rig.pc(&format!("Docs/{name}")), name);
    }
    let list = [
        "virus.txt|*local.Object|failed to open source object: open /x/virus.txt: Operation did not complete successfully because the file contains a virus or potentially unwanted software.",
        "refused.txt|*local.Object|Invalid response status! Got 400, expected 200; headers: map[Content-Length:[0]], content: ",
        "odd.txt|*local.Object|something nobody has seen before",
        "locked.pst|*local.Object|failed to open source object: open /x/locked.pst: The process cannot access the file because another process has locked a portion of the file.",
    ];
    set_flag(&rig, "fail.list", Some(&format!("{}\n", list.join("\n"))));
    let id = rig.pair("Docs").await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/ok.txt")), "ok.txt");
    let now = synchopper_lib::util::now_secs();
    let f = |path: &str| v.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} missing: {}", rig.dump(&id))).clone();

    let virus = f("virus.txt");
    assert_eq!((virus.kind.as_str(), virus.flagged, virus.auto_retry, virus.can_ignore), ("virus", true, false, true));
    assert!(virus.reason.contains("Check Protection history in Windows Security"), "{}", virus.reason);

    let refused = f("refused.txt");
    assert_eq!((refused.kind.as_str(), refused.flagged, refused.auto_retry), ("rejected", false, true));
    assert!(refused.reason.starts_with("Koofr refused this file."), "{}", refused.reason);
    assert!(refused.next_retry >= now + 24 * 3600 - 60, "tried again in a day");

    let odd = f("odd.txt");
    assert_eq!(odd.kind, "unknown");
    assert_eq!(odd.reason, "Something unexpected stopped this file from syncing.");
    assert!(odd.detail.contains("something nobody has seen before"), "the engine's words stay under Technical details");

    let locked = f("locked.pst");
    assert_eq!(locked.kind, "locked");
    assert_eq!(locked.reason, "This file is open in another program.");
    assert!(locked.next_retry <= now + 300 + 5 && locked.next_retry >= now + 300 - 60);

    assert_eq!(notes_titled(&rig, "Docs: 1 file needs attention"), 1, "only the virus needs the user right away");

    // Their turn comes: the virus isn't tried again by itself, the rest are,
    // and the locked file waits longer each time.
    make_retries_due(&rig, &id);
    rig.sync(&id).await;
    let p = rig.app.core.pair(&id).unwrap();
    let tries = |path: &str| p.skipped.iter().find(|k| k.path == path).map(|k| (k.kind.clone(), k.tries, k.next_retry)).unwrap();
    assert_eq!(tries("virus.txt").1, 1, "{}", rig.dump(&id));
    let (kind, n, next) = tries("locked.pst");
    assert_eq!((kind.as_str(), n), ("locked", 2));
    assert!(next >= now + 900 - 60, "15 minutes the second time");
    assert_eq!(tries("refused.txt").1, 2);

    // Once Windows Security lets it go, Try again brings it back.
    set_flag(&rig, "fail.list", None);
    rig.app.engine.retry_skipped(&id);
    rig.app.engine.drain().await;
    let v = rig.view(&id);
    assert!(v.files.is_empty(), "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/virus.txt")), "virus.txt");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_in_an_error_is_not_taken_for_the_error() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    write(&rig.pc("Docs/a.txt"), "a");
    let id = rig.pair("Docs").await;
    // Open in another program. Its name has a number and a word that used
    // to be read as a wrong password and a full Koofr.
    write(&rig.pc("Docs/Sales quota 401.xlsx"), "numbers");
    write(&rig.pc("Docs/b.txt"), "b");
    set_flag(&rig, "fail.list", Some("Sales quota 401.xlsx|*local.Object|failed to open source object: open /x/Docs/Sales quota 401.xlsx: The process cannot access the file because it is being used by another process.\n"));
    rig.sync(&id).await;
    let v = rig.view(&id);
    assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(&id));
    assert!(v.error.is_none());
    assert_eq!(v.files.len(), 1, "{}", rig.dump(&id));
    assert_eq!((v.files[0].kind.as_str(), v.files[0].reason.as_str()), ("busy", "This file is open in another program."));
    assert_eq!(read(&rig.koofr("Docs/b.txt")), "b");
    assert!(rig.host.notes.lock().unwrap().iter().all(|(_, b)| !b.contains("app password") && !b.contains("storage is full")));
}
