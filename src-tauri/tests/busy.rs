#![cfg(unix)]
//! A file that cannot be read must not hold up the rest of its folder, and
//! must sync by itself once it can be read again.
//!
//! The test needs to make a file unreadable to the sync engine, which root
//! cannot do to itself, so the engine is run as the "nobody" user through a
//! small wrapper script. It is skipped when that is not possible.

mod common;
use common::*;
use std::os::unix::fs::PermissionsExt;
use synchopper_lib::engine::Phase;

fn real_rclone() -> Option<String> {
    let out = std::process::Command::new("sh").args(["-c", "command -v rclone && command -v setpriv && id -u"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut lines = text.lines();
    let rclone = lines.next()?.to_string();
    lines.next()?;
    (lines.next()? == "0").then_some(rclone)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_file_is_left_out_then_synced_once_readable() {
    let Some(rclone) = real_rclone() else {
        eprintln!("skipped: needs root, setpriv and rclone");
        return;
    };
    let rig = Rig::new();
    let root = rig.dir.path().to_string_lossy().to_string();
    let wrapper = rig.dir.path().join("rclone-as-nobody.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nchmod -R a+rwX '{root}' 2>/dev/null\nif [ -f '{root}/locked.list' ]; then while read -r f; do chmod 000 \"$f\"; done < '{root}/locked.list'; fi\nexec setpriv --reuid=65534 --regid=65534 --clear-groups '{rclone}' \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    rig.app.core.set_engine(|e| e.exe = Some(wrapper.clone()));

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
    std::fs::write(rig.dir.path().join("locked.list"), format!("{}\n", locked.display())).unwrap();

    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/c.txt")), "c", "the rest of the folder must not be held up");
    assert!(!rig.koofr("Docs/mail.pst").exists());
    assert!(!rig.koofr("Docs/b.txt").exists(), "the waiting delete must still be passed on: {}", rig.dump(&id));
    assert!(!rig.pc("Docs/b.txt").exists(), "a deleted file must not come back: {}", rig.dump(&id));
    let skipped = rig.app.core.pair(&id).unwrap().skipped;
    assert_eq!(skipped.len(), 1, "{}", rig.dump(&id));
    assert_eq!(skipped[0].path, "mail.pst");
    assert!(skipped[0].reason.contains("would not let the app read"), "{}", skipped[0].reason);

    // With the trouble file set aside, ordinary syncing works, deletes included.
    write(&rig.pc("Docs/d.txt"), "d");
    std::fs::remove_file(rig.koofr("Docs/a.txt")).unwrap();
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/d.txt")), "d");
    assert!(!rig.pc("Docs/a.txt").exists(), "a delete in koofr should still reach the pc");
    assert_eq!(read(&locked), "locked content", "the trouble file itself is never touched");

    // The file becomes readable. The next sync brings it across.
    std::fs::remove_file(rig.dir.path().join("locked.list")).unwrap();
    rig.app.engine.retry_skipped(&id);
    rig.app.engine.drain().await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert!(rig.app.core.pair(&id).unwrap().skipped.is_empty(), "{}", rig.dump(&id));
    assert_eq!(read(&rig.koofr("Docs/mail.pst")), "locked content");
}

/// A stand-in engine that fails one sync the way rclone does when a transfer
/// dies (it sets the sync record aside and asks for a rebuild), then behaves
/// normally. The app must carry on from the last good record instead of
/// rebuilding, so a delete that was waiting is passed on, not undone.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_transfer_is_retried_without_undoing_deletes() {
    let Some(rclone) = real_rclone() else {
        eprintln!("skipped: needs root, setpriv and rclone");
        return;
    };
    let rig = Rig::new();
    let root = rig.dir.path().to_string_lossy().to_string();
    let wrapper = rig.dir.path().join("rclone-fail-once.sh");
    std::fs::write(
        &wrapper,
        format!(
            r#"#!/bin/sh
if [ "$1" = "bisync" ] && [ -f '{root}/fail-once' ]; then
  rm -f '{root}/fail-once'
  prev=""
  for a in "$@"; do
    if [ "$prev" = "--workdir" ]; then work="$a"; fi
    prev="$a"
  done
  for f in "$work"/*.path1.lst "$work"/*.path2.lst; do mv "$f" "$f-err"; done
  echo '{{"level":"error","msg":"Failed to copy: unexpected EOF","object":"big.bin","objectType":"*local.Object"}}' >&2
  echo '{{"level":"error","msg":"Bisync critical error: unexpected EOF"}}' >&2
  echo '{{"level":"error","msg":"Bisync aborted. Must run --resync to recover."}}' >&2
  exit 7
fi
exec '{rclone}' "$@"
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    rig.app.core.set_engine(|e| e.exe = Some(wrapper.clone()));

    for n in 1..=5 {
        write(&rig.pc(&format!("Docs/f{n}.txt")), "x");
    }
    let id = rig.pair("Docs").await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));

    std::fs::remove_file(rig.pc("Docs/f1.txt")).unwrap();
    write(&rig.pc("Docs/new.txt"), "new");
    std::fs::write(rig.dir.path().join("fail-once"), "").unwrap();
    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Error, "{}", rig.dump(&id));
    assert!(!rig.app.core.pair(&id).unwrap().needs_resync, "the record was put back, so no rebuild is needed");
    assert!(rig.app.core.pair(&id).unwrap().skipped.is_empty(), "one unexplained failure does not set a file aside");
    assert!(rig.koofr("Docs/f1.txt").exists(), "nothing happened yet");

    rig.sync(&id).await;
    assert_eq!(rig.phase(&id), Phase::Ok, "{}", rig.dump(&id));
    assert!(!rig.koofr("Docs/f1.txt").exists(), "the waiting delete was passed on");
    assert!(!rig.pc("Docs/f1.txt").exists(), "and the deleted file did not come back");
    assert_eq!(read(&rig.koofr("Docs/new.txt")), "new");
    let log = std::fs::read_to_string(rig.app.core.log_path()).unwrap();
    let rebuilds = log.matches("rebuilding the sync record").count();
    assert_eq!(rebuilds, 1, "only the very first sync builds the record");
}
