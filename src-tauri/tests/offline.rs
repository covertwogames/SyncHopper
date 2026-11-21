#![cfg(unix)]
//! When Koofr can't be reached, as on a laptop that has just woken up: the
//! first failed try changes nothing on screen, every folder waits together
//! instead of each trying in turn, and everything catches up once Koofr
//! answers.

mod common;
use common::*;
use std::time::Duration;
use synchopper_lib::engine::{Job, Phase};

fn bisync_runs(rig: &Rig) -> usize {
    std::fs::read_to_string(rig.dir.path().join("args.log")).unwrap_or_default().lines().filter(|l| l.starts_with("bisync ")).count()
}

fn notes(rig: &Rig) -> Vec<(String, String)> {
    rig.host.notes.lock().unwrap().clone()
}

/// Three synced folders, all up to date.
async fn three_folders(rig: &Rig) -> Vec<String> {
    let mut ids = Vec::new();
    for name in ["Docs", "Photos", "Music"] {
        write(&rig.pc(&format!("{name}/a.txt")), name);
        let id = rig.pair(name).await;
        assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
        ids.push(id);
    }
    ids
}

/// What the scheduler does when folders are due: queues them, unprompted.
async fn scheduled_syncs(rig: &Rig, ids: &[String]) {
    rig.app.engine.retry_koofr_if_due();
    for id in ids {
        rig.app.engine.enqueue(Job { pair_id: id.clone(), ..Default::default() });
    }
    rig.app.engine.drain().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn waking_up_offline_shows_nothing_and_catches_up() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    let ids = three_folders(&rig).await;
    let notes_before = notes(&rig).len();

    // The laptop wakes up before it's back online, and every folder is due.
    set_flag(&rig, "offline", Some(""));
    let runs = bisync_runs(&rig);
    scheduled_syncs(&rig, &ids).await;
    assert_eq!(bisync_runs(&rig), runs + 1, "only one folder tries");
    for id in &ids {
        let v = rig.view(id);
        assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(id));
        assert!(v.waiting.is_none() && v.error.is_none() && v.retry_at.is_none(), "{}", rig.dump(id));
    }
    assert_eq!(notes(&rig).len(), notes_before, "no notifications: {:?}", notes(&rig));
    assert_eq!(rig.app.status_line(), "SyncHopper: up to date");

    // Twenty seconds later it's online again.
    set_flag(&rig, "offline", None);
    write(&rig.pc("Photos/new.jpg"), "new");
    rig.app.engine.expire_koofr_wait(Duration::ZERO);
    scheduled_syncs(&rig, &ids).await;
    for id in &ids {
        assert_eq!(rig.phase(id), Phase::Ok, "{}", rig.dump(id));
    }
    assert_eq!(read(&rig.koofr("Photos/new.jpg")), "new");
    assert!(notes(&rig)[notes_before..].iter().all(|(t, _)| !t.contains("could not sync") && !t.contains("Koofr")), "{:?}", notes(&rig));
}

#[tokio::test(flavor = "multi_thread")]
async fn staying_offline_shows_one_wait_for_every_folder() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    let ids = three_folders(&rig).await;
    let notes_before = notes(&rig).len();
    set_flag(&rig, "offline", Some(""));
    scheduled_syncs(&rig, &ids).await;

    // Still offline at the next try: every folder shows the wait at once.
    let runs = bisync_runs(&rig);
    rig.app.engine.expire_koofr_wait(Duration::ZERO);
    scheduled_syncs(&rig, &ids).await;
    assert_eq!(bisync_runs(&rig), runs + 1, "still only one folder tries");
    let first = rig.view(&ids[0]).retry_at;
    for id in &ids {
        let v = rig.view(id);
        assert_eq!(v.phase, Phase::Waiting, "{}", rig.dump(id));
        assert_eq!(v.waiting.as_deref(), Some("Can't connect to Koofr right now."));
        assert_eq!(v.waiting_kind.as_deref(), Some("koofr"));
        assert!(v.error.is_none());
        assert_eq!(v.retry_at, first, "one retry time for all");
    }
    let wait = first.unwrap() - synchopper_lib::util::now_secs();
    assert!((115..=120).contains(&wait), "next try in 2 minutes, not {wait}s");
    assert_eq!(rig.app.status_line(), "SyncHopper: waiting for Koofr");
    assert_eq!(notes(&rig).len(), notes_before);

    // Two more failed tries: one notification, on the third shown wait.
    rig.app.engine.expire_koofr_wait(Duration::ZERO);
    scheduled_syncs(&rig, &ids).await;
    assert_eq!(notes(&rig).len(), notes_before, "not on the second");
    let wait = rig.view(&ids[1]).retry_at.unwrap() - synchopper_lib::util::now_secs();
    assert!((295..=300).contains(&wait), "then 5 minutes, not {wait}s");
    rig.app.engine.expire_koofr_wait(Duration::ZERO);
    scheduled_syncs(&rig, &ids).await;
    assert_eq!(bisync_runs(&rig), runs + 3);
    let new: Vec<(String, String)> = notes(&rig)[notes_before..].to_vec();
    assert_eq!(new, vec![("Can't connect to Koofr right now.".to_string(), "SyncHopper will keep trying and catch up once Koofr is back.".to_string())]);
    let wait = rig.view(&ids[2]).retry_at.unwrap() - synchopper_lib::util::now_secs();
    assert!((595..=600).contains(&wait), "then 10 minutes, not {wait}s");
    rig.app.engine.expire_koofr_wait(Duration::ZERO);
    scheduled_syncs(&rig, &ids).await;
    assert_eq!(notes(&rig).len(), notes_before + 1, "told once");

    // Back online: everything catches up.
    set_flag(&rig, "offline", None);
    rig.app.engine.expire_koofr_wait(Duration::ZERO);
    scheduled_syncs(&rig, &ids).await;
    for id in &ids {
        let v = rig.view(id);
        assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(id));
        assert!(v.waiting.is_none() && v.retry_at.is_none());
    }

    // A new spell starts quietly again.
    set_flag(&rig, "offline", Some(""));
    scheduled_syncs(&rig, &ids).await;
    for id in &ids {
        assert_eq!(rig.phase(id), Phase::Ok, "{}", rig.dump(id));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_now_during_a_wait_tries_right_away_and_ends_it() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    let ids = three_folders(&rig).await;
    set_flag(&rig, "offline", Some(""));
    scheduled_syncs(&rig, &ids).await;

    // "Sync now" goes ahead even though it's not time yet, and says what
    // happened rather than staying quiet. The engine's "critical" line is
    // read as a connection problem, not as something unexpected.
    let runs = bisync_runs(&rig);
    rig.sync(&ids[1]).await;
    assert_eq!(bisync_runs(&rig), runs + 1);
    for id in &ids {
        assert_eq!(rig.view(id).waiting.as_deref(), Some("Can't connect to Koofr right now."), "{}", rig.dump(id));
    }
    assert!(rig.view(&ids[1]).details.iter().any(|d| d.contains("no such host")), "{}", rig.dump(&ids[1]));
    assert!(notes(&rig).iter().all(|(t, _)| !t.contains("could not sync")));

    // Online again: "Try now" on one folder works, and the wait is over for
    // the others too, without waiting for the retry time.
    set_flag(&rig, "offline", None);
    rig.sync(&ids[0]).await;
    assert_eq!(rig.phase(&ids[0]), Phase::Ok, "{}", rig.dump(&ids[0]));
    let runs = bisync_runs(&rig);
    for id in &ids[1..] {
        rig.app.engine.enqueue(Job { pair_id: id.clone(), ..Default::default() });
    }
    rig.app.engine.drain().await;
    assert_eq!(bisync_runs(&rig), runs + 2, "the others weren't held back");
    for id in &ids {
        assert_eq!(rig.phase(id), Phase::Ok, "{}", rig.dump(id));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn work_set_aside_during_a_wait_is_kept() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    let ids = three_folders(&rig).await;
    set_flag(&rig, "offline", Some(""));
    scheduled_syncs(&rig, &ids[..1]).await;
    // A rebuild asked for during the wait is set aside, not lost, even
    // when the folder was already set aside for an ordinary sync.
    let runs = bisync_runs(&rig);
    rig.app.engine.enqueue(Job { pair_id: ids[2].clone(), ..Default::default() });
    rig.app.engine.drain().await;
    rig.app.engine.enqueue(Job { pair_id: ids[2].clone(), resync: true, ..Default::default() });
    rig.app.engine.drain().await;
    assert_eq!(bisync_runs(&rig), runs, "nothing tried before the retry time");
    set_flag(&rig, "offline", None);
    rig.app.engine.expire_koofr_wait(Duration::ZERO);
    scheduled_syncs(&rig, &ids[..1]).await;
    let log = std::fs::read_to_string(rig.dir.path().join("args.log")).unwrap();
    let music = rig.pc("Music").to_string_lossy().to_string();
    let line = log.lines().rev().find(|l| l.starts_with("bisync ") && l.contains(&music)).unwrap();
    assert!(line.contains("--resync"), "{line}");
    assert_eq!(rig.phase(&ids[2]), Phase::Ok, "{}", rig.dump(&ids[2]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_left_over_from_before_sleep_starts_quietly_again() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    let ids = three_folders(&rig).await;
    set_flag(&rig, "offline", Some(""));
    scheduled_syncs(&rig, &ids).await;
    // The laptop went to sleep before the retry and wakes up much later,
    // still offline for a moment.
    rig.app.engine.expire_koofr_wait(Duration::from_secs(700));
    scheduled_syncs(&rig, &ids).await;
    for id in &ids {
        let v = rig.view(id);
        assert_eq!(v.phase, Phase::Ok, "{}", rig.dump(id));
        assert!(v.waiting.is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn koofr_having_problems_shows_straight_away_for_every_folder() {
    let rig = Rig::new();
    if fake_engine(&rig, false).is_none() {
        eprintln!("skipped: needs rclone");
        return;
    }
    let ids = three_folders(&rig).await;
    // Koofr answering with errors isn't a laptop waking up: no quiet try.
    set_flag(&rig, "outage", Some(""));
    let runs = bisync_runs(&rig);
    scheduled_syncs(&rig, &ids).await;
    assert_eq!(bisync_runs(&rig), runs + 1);
    for id in &ids {
        let v = rig.view(id);
        assert_eq!(v.phase, Phase::Waiting, "{}", rig.dump(id));
        assert_eq!(v.waiting.as_deref(), Some("Koofr is having problems right now."));
    }
    set_flag(&rig, "outage", None);
    rig.app.engine.expire_koofr_wait(Duration::ZERO);
    scheduled_syncs(&rig, &ids).await;
    for id in &ids {
        assert_eq!(rig.phase(id), Phase::Ok, "{}", rig.dump(id));
    }
}
