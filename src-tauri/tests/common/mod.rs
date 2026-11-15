#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use synchopper_lib::app::App;
use synchopper_lib::core::Host;
use synchopper_lib::engine::{PairView, Phase};

#[derive(Default)]
pub struct TestHost {
    pub notes: Mutex<Vec<(String, String)>>,
    pub shown: Mutex<Vec<Option<String>>>,
}

impl Host for TestHost {
    fn state_changed(&self) {}
    fn notify(&self, title: &str, body: &str) {
        self.notes.lock().unwrap().push((title.to_string(), body.to_string()));
    }
    fn set_autostart(&self, _on: bool) -> Result<(), String> {
        Ok(())
    }
    fn exe_path(&self) -> Option<PathBuf> {
        None
    }
    fn show_window(&self, view: Option<&str>) {
        self.shown.lock().unwrap().push(view.map(|v| v.to_string()));
    }
}

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub app: Arc<App>,
    pub host: Arc<TestHost>,
}

impl Rig {
    /// A fresh app whose "Koofr" is a plain folder, already connected.
    pub fn new() -> Rig {
        Rig::with_engine("rclone")
    }

    pub fn with_engine(exe: &str) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("koofr")).unwrap();
        std::fs::create_dir_all(dir.path().join("pc")).unwrap();
        let host = Arc::new(TestHost::default());
        let app = App::new(
            dir.path().join("data"),
            dir.path().join("koofr").to_string_lossy().to_string(),
            true,
            host.clone(),
        );
        let exe = PathBuf::from(exe);
        app.core.set_engine(|e| {
            e.state = "ready".into();
            e.version = "test".into();
            e.exe = Some(exe);
        });
        app.core.update(|s| {
            s.setup_complete = true;
            s.account_email = "tester@example.com".into();
            s.notify_level = "all".into();
        });
        Rig { dir, app, host }
    }

    pub fn pc(&self, rel: &str) -> PathBuf {
        self.dir.path().join("pc").join(rel)
    }

    pub fn koofr(&self, rel: &str) -> PathBuf {
        self.dir.path().join("koofr").join(rel)
    }

    /// Adds a synced folder and runs its first sync.
    pub async fn pair(&self, name: &str) -> String {
        let local = self.pc(name);
        let pair = self.app.engine.add_pair(&local.to_string_lossy(), name).expect("pair should be accepted");
        self.app.engine.drain().await;
        pair.id
    }

    pub async fn sync(&self, id: &str) {
        self.app.engine.sync_now(Some(id));
        self.app.engine.drain().await;
    }

    pub fn view(&self, id: &str) -> PairView {
        self.app.engine.pair_views().into_iter().find(|p| p.id == id).expect("pair exists")
    }

    /// Everything known about a folder, for failure messages.
    pub fn dump(&self, id: &str) -> String {
        let v = self.view(id);
        let log = std::fs::read_to_string(self.app.core.log_path()).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(25).collect();
        format!(
            "phase={:?} error={:?} waiting={:?} details={:?} approval={:?} files={:?} ignored={:?}\nlog tail:\n{}",
            v.phase, v.error, v.waiting, v.details, v.approval,
            v.files.iter().map(|f| (&f.path, &f.kind, f.flagged, &f.detail)).collect::<Vec<_>>(),
            v.ignored,
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        )
    }

    pub fn phase(&self, id: &str) -> Phase {
        self.view(id).phase
    }
}

pub fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| "<missing>".to_string())
}

pub fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// rclone compares modified times, so edits made back to back need a gap.
pub async fn tick() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

pub async fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// A stand-in for the sync engine program that can pretend Koofr is down, or
/// that particular files fail, and can run the real engine as an unprivileged
/// user so files can be made unreadable to it. Controlled by files in the
/// rig's folder:
/// - `outage`: every two-way sync fails as if Koofr returned errors.
/// - `fail.list`: lines of `path|object type|message`. A two-way sync that
///   would touch one of these paths (because it is not left out yet) syncs
///   everything else, then reports these files as failed.
/// - `locked.list`: absolute paths made unreadable to the engine.
/// - `listing_error`: every two-way sync fails with this message (written
///   JSON-escaped), as when a folder listing fails.
/// - `listing_error_once`: the same, for the next two-way sync only.
///
/// Every command line it is given is added to `args.log`.
///
/// Returns None when the test machine can't run it.
#[cfg(unix)]
pub fn fake_engine(rig: &Rig, as_nobody: bool) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let out = std::process::Command::new("sh").args(["-c", "command -v rclone; command -v setpriv; id -u"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let lines: Vec<&str> = text.lines().collect();
    let rclone = lines.first()?.to_string();
    if as_nobody && (lines.len() < 3 || lines[2] != "0") {
        return None;
    }
    let root = rig.dir.path().to_string_lossy().to_string();
    let run = if as_nobody {
        format!("setpriv --reuid=65534 --regid=65534 --clear-groups '{rclone}'")
    } else {
        format!("'{rclone}'")
    };
    let script = format!(
        r#"#!/bin/sh
R='{root}'
printf '%s\n' "$*" >> "$R/args.log"
chmod -R a+rwX "$R" 2>/dev/null
if [ -f "$R/locked.list" ]; then while read -r f; do chmod 000 "$f"; done < "$R/locked.list"; fi
if [ "$1" != "bisync" ]; then exec {run} "$@"; fi
work=""; filters=""; prev=""
for a in "$@"; do
  [ "$prev" = "--workdir" ] && work="$a"
  [ "$prev" = "--filters-file" ] && filters="$a"
  prev="$a"
done
if [ -f "$R/outage" ]; then
  echo '{{"level":"error","msg":"error reading destination root directory: Invalid response status! Got 500, expected [200 206]"}}' >&2
  echo '{{"level":"error","msg":"Bisync critical error: Invalid response status! Got 500, expected [200 206]"}}' >&2
  echo '{{"level":"error","msg":"Bisync aborted. Error is retryable without --resync due to --resilient mode."}}' >&2
  exit 7
fi
if [ -f "$R/listing_error" ]; then
  m=$(cat "$R/listing_error")
  echo '{{"level":"error","msg":"'"$m"'"}}' >&2
  echo '{{"level":"error","msg":"Bisync critical error: '"$m"'"}}' >&2
  echo '{{"level":"error","msg":"Bisync aborted. Error is retryable without --resync due to --resilient mode."}}' >&2
  exit 7
fi
if [ -f "$R/listing_error_once" ]; then
  m=$(cat "$R/listing_error_once")
  rm -f "$R/listing_error_once"
  echo '{{"level":"error","msg":"'"$m"'"}}' >&2
  echo '{{"level":"error","msg":"Bisync critical error: '"$m"'"}}' >&2
  echo '{{"level":"error","msg":"Bisync aborted. Must run --resync to recover."}}' >&2
  exit 7
fi
if [ ! -f "$R/fail.list" ]; then exec {run} "$@"; fi
active=""
while IFS='|' read -r p t m; do
  [ -z "$p" ] && continue
  if [ -n "$filters" ] && grep -qxF -- "- /$p" "$filters"; then continue; fi
  active="$active$p|$t|$m
"
done < "$R/fail.list"
if [ -z "$active" ]; then exec {run} "$@"; fi
set -- "$@"
excl=""
printf '%s' "$active" | while IFS='|' read -r p t m; do [ -n "$p" ] && printf -- '- /%s
' "$p"; done > "$R/extra-filter.txt"
cat "$filters" >> "$R/extra-filter.txt"
{run} "$@" --filter-from "$R/extra-filter.txt"
code=$?
if [ $code -ne 0 ]; then exit $code; fi
for f in "$work"/*.path1.lst "$work"/*.path2.lst; do [ -f "$f" ] && mv "$f" "$f-err"; done
printf '%s' "$active" | while IFS='|' read -r p t m; do
  [ -n "$p" ] && echo '{{"level":"error","msg":"Failed to copy: '"$m"'","object":"'"$p"'","objectType":"'"$t"'"}}' >&2
done
echo '{{"level":"error","msg":"Bisync aborted. Must run --resync to recover."}}' >&2
exit 7
"#
    );
    let path = rig.dir.path().join("fake-rclone.sh");
    std::fs::write(&path, script).ok()?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
    let exe = path.clone();
    rig.app.core.set_engine(|e| e.exe = Some(exe));
    Some(path)
}

pub fn set_flag(rig: &Rig, name: &str, content: Option<&str>) {
    let p = rig.dir.path().join(name);
    match content {
        Some(c) => std::fs::write(p, c).unwrap(),
        None => {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Makes every left-out file due for a retry, as if time had passed.
pub fn make_retries_due(rig: &Rig, id: &str) {
    rig.app.core.update_pair(id, |p| {
        for k in p.skipped.iter_mut() {
            k.next_retry = 0;
        }
    });
}
