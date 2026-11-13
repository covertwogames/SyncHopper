//! The sync engine: a queue of folder syncs run one at a time through
//! rclone's two-way sync, with a timer, a file watcher, and recovery from
//! the ways a sync can fail.

use crate::config::{Pair, Settings, Skipped};
use crate::core::{Core, Notice};
use crate::platform;
use crate::rclone::{friendly_error, LogLine, RunResult, Stats};
use crate::util::{clean_remote, file_name, glob_match, new_id, norm_local, now_secs, paths_overlap, Cancel};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// How long a folder must be quiet before local changes are synced.
const QUIET: Duration = Duration::from_secs(4);
/// Shortest pause between two syncs of one folder that were set off by
/// local changes, so a busy folder does not hammer Koofr.
const BREATHER: Duration = Duration::from_secs(15);
/// How often the battery and metered-connection checks run.
const POWER_CHECK: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Queued,
    Checking,
    Syncing,
    Ok,
    Error,
    Approval,
}

/// A sync that stopped because it would delete a large share of a folder.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Approval {
    pub deleted: u64,
    pub total: u64,
    /// Where the files were deleted: "pc" or "koofr".
    pub side: String,
    /// True when that side is completely empty. `deleted` then counts the
    /// files still on the other side.
    pub everything: bool,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct Progress {
    pub percent: f64,
    pub done: u64,
    pub total: u64,
    pub speed: f64,
    pub eta: Option<u64>,
    pub file: String,
}

#[derive(Clone, Debug, Default)]
pub struct Job {
    pub pair_id: String,
    /// Allow a large delete this once.
    pub force: bool,
    /// Rebuild the sync record. Copies both ways and never deletes.
    pub resync: bool,
    /// Started by the user, so automatic pauses do not apply.
    pub manual: bool,
    /// Create the Koofr folder if it is missing.
    pub create_remote: bool,
}

#[derive(Default)]
struct PairRt {
    phase: Phase,
    progress: Option<Progress>,
    error: Option<String>,
    details: Vec<String>,
    approval: Option<Approval>,
    conflicts: Vec<String>,
    last_event: Option<Instant>,
    next_due: Option<Instant>,
    fails: u32,
    fail_counts: HashMap<String, u32>,
    retry_skipped: bool,
    summary: String,
    runs: u64,
    last_finished: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<Job>,
    rt: HashMap<String, PairRt>,
    current: Option<(String, Cancel)>,
    paused_all: bool,
    auto_pause: Option<String>,
    auto_checked: Option<Instant>,
}

#[derive(Serialize)]
pub struct PairView {
    pub id: String,
    pub name: String,
    pub local: String,
    pub remote: String,
    pub paused: bool,
    pub phase: Phase,
    pub progress: Option<Progress>,
    pub last_sync: u64,
    pub error: Option<String>,
    pub details: Vec<String>,
    pub approval: Option<Approval>,
    pub skipped: Vec<Skipped>,
    pub conflicts: Vec<String>,
    pub summary: String,
}

#[derive(Serialize)]
pub struct SyncView {
    pub paused_all: bool,
    pub auto_pause: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Ok,
    Cancelled,
    TooManyDeletes(Approval),
    /// One side has become completely empty: "pc" or "koofr".
    SideEmpty(String),
    LockFound,
    NeedsResync,
    RemoteMissing,
    Failed { must_resync: bool, files: Vec<(String, String)>, message: String, details: Vec<String> },
}

fn parse_deletes(msg: &str) -> Option<Approval> {
    // Example: too many deletes (>50%, 100 of 140) on Path1 "C:\x". Run with --force if desired.
    let inner = msg.split('(').nth(1)?.split(')').next()?;
    let counts = inner.split(',').nth(1)?.trim();
    let mut parts = counts.split(" of ");
    let deleted = parts.next()?.trim().parse().ok()?;
    let total = parts.next()?.trim().parse().ok()?;
    let side = if msg.contains("on Path1") { "pc" } else { "koofr" };
    Some(Approval { deleted, total, side: side.into(), everything: false })
}

fn is_boilerplate(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.starts_with("bisync aborted")
        || l.starts_with("failed to bisync")
        || l.contains("not deleting files as there were io errors")
        || l.contains("not deleting directories as there were io errors")
        || l.contains("config file") && l.contains("not found")
}

/// Works out what happened in a finished two-way sync run.
pub fn classify(res: &RunResult) -> Verdict {
    if res.cancelled {
        return Verdict::Cancelled;
    }
    if let Some(e) = &res.spawn_error {
        return Verdict::Failed { must_resync: false, files: vec![], message: e.clone(), details: vec![e.clone()] };
    }
    if res.exit == 0 {
        return Verdict::Ok;
    }
    let all: Vec<&LogLine> = res.errors.iter().chain(res.notices.iter()).collect();
    let find = |needle: &str| all.iter().find(|l| l.msg.to_lowercase().contains(needle)).copied();

    if let Some(a) = find("too many deletes").and_then(|l| parse_deletes(&l.msg)) {
        return Verdict::TooManyDeletes(a);
    }
    if find("prior lock file found").is_some() {
        return Verdict::LockFound;
    }
    if find("empty current path1 listing").is_some() {
        return Verdict::SideEmpty("pc".into());
    }
    if find("empty current path2 listing").is_some() {
        return Verdict::SideEmpty("koofr".into());
    }

    let mut files: Vec<(String, String)> = Vec::new();
    for l in &res.errors {
        let file_like = l.object_type.ends_with("Object") || l.msg.starts_with("Failed to copy");
        if !l.object.is_empty() && file_like && !files.iter().any(|(o, _)| o == &l.object) {
            files.push((l.object.clone(), l.msg.clone()));
        }
    }
    let must_resync = find("must run --resync to recover").is_some();
    if files.is_empty() {
        if find("filters file has changed").is_some()
            || find("filters file md5 hash not found").is_some()
            || find("cannot find prior path1 or path2 listings").is_some()
            || find("empty prior path").is_some()
        {
            return Verdict::NeedsResync;
        }
        if find("directory not found").is_some() {
            return Verdict::RemoteMissing;
        }
    }
    let details: Vec<String> = {
        let mut d: Vec<String> = Vec::new();
        for l in &res.errors {
            if is_boilerplate(&l.msg) {
                continue;
            }
            let text = if l.object.is_empty() || !l.object_type.ends_with("Object") {
                l.msg.clone()
            } else {
                format!("{}: {}", l.object, l.msg)
            };
            if !d.contains(&text) && d.len() < 20 {
                d.push(text);
            }
        }
        d
    };
    let message = files
        .first()
        .map(|(_, m)| m.clone())
        .or_else(|| details.first().map(|m| m.trim_start_matches("Bisync critical error: ").to_string()))
        .unwrap_or_else(|| format!("The sync engine stopped unexpectedly (code {}).", res.exit));
    Verdict::Failed { must_resync, files, message, details }
}

/// How a per-file failure is treated.
#[derive(PartialEq, Debug)]
enum FileIssue {
    /// The file is busy on this PC right now. Checked again every sync.
    Busy,
    /// The file can never sync as it is. Left out until the user asks.
    Stuck,
    /// Could be anything. Left out only if it keeps failing.
    Unknown,
}

fn file_issue(msg: &str) -> FileIssue {
    let l = msg.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    if has(&[
        "being used by another process", "sharing violation", "failed to open source object",
        "access is denied", "permission denied", "source file is being updated", "size changed",
    ]) {
        FileIssue::Busy
    } else if has(&["too long", "syntax is incorrect", "cannot find the path specified", "invalid argument"]) {
        FileIssue::Stuck
    } else {
        FileIssue::Unknown
    }
}

fn escape_glob(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    for c in path.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']' | '{' | '}') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The rclone filter file for a folder: the ignore list plus any files that
/// are being left out because they could not sync.
pub fn filters_text(s: &Settings, pair: &Pair) -> String {
    let mut out = String::from("# Written by SyncHopper. Change the ignore list in Advanced settings.\n");
    for p in &s.ignore_patterns {
        out.push_str(&format!("- {p}\n"));
    }
    for k in &pair.skipped {
        out.push_str(&format!("- /{}\n", escape_glob(&k.path)));
    }
    out
}

fn listing_ready(work: &Path) -> bool {
    // rclone keeps one record per side. A missing or empty record means the
    // next run has to rebuild it.
    let mut found = [false, false];
    let Ok(entries) = std::fs::read_dir(work) else { return false };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let idx = if name.ends_with(".path1.lst") {
            0
        } else if name.ends_with(".path2.lst") {
            1
        } else {
            continue;
        };
        let has_rows = std::fs::read_to_string(e.path())
            .map(|t| t.lines().any(|l| !l.trim().is_empty() && !l.starts_with('#')))
            .unwrap_or(false);
        found[idx] = has_rows;
    }
    found[0] && found[1]
}

/// After a failed run rclone sets the last good sync record aside (as
/// ".lst-err") and refuses to run again without a full rebuild. Putting the
/// record back lets the next run carry on from the last good state, the same
/// way rclone itself recovers from an interrupted run. Returns true when
/// both sides' records are in place afterwards.
fn restore_records(work: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(work) else { return false };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(base) = name.strip_suffix(".lst-err") {
            let target = work.join(format!("{base}.lst"));
            if !target.exists() {
                let _ = std::fs::rename(e.path(), target);
            }
        }
    }
    let has = |suffix: &str| {
        std::fs::read_dir(work)
            .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().ends_with(suffix)))
            .unwrap_or(false)
    };
    has(".path1.lst") && has(".path2.lst")
}

/// Removes set-aside records so an old one can never be put back later.
fn purge_set_aside(work: &Path) {
    if let Ok(entries) = std::fs::read_dir(work) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(".lst-err") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Writes the filter file. With `keep_record` the checksum rclone keeps next
/// to it is refreshed too, so rclone accepts the new list without a rebuild.
/// That is only used when files are newly left out, which is safe: a left
/// out file simply drops out of the comparison on both sides.
fn write_filters(path: &Path, text: &str, keep_record: bool) -> std::io::Result<()> {
    std::fs::write(path, text)?;
    let sum_path = PathBuf::from(format!("{}.md5", path.to_string_lossy()));
    if keep_record {
        use md5::{Digest, Md5};
        let sum: String = Md5::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        std::fs::write(sum_path, sum)?;
    }
    Ok(())
}

fn clear_locks(work: &Path) {
    if let Ok(entries) = std::fs::read_dir(work) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().ends_with(".lck") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

pub struct Engine {
    core: Arc<Core>,
    inner: Mutex<Inner>,
    wake: Notify,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

impl Engine {
    pub fn new(core: Arc<Core>) -> Arc<Engine> {
        Arc::new(Engine { core, inner: Mutex::new(Inner::default()), wake: Notify::new(), watcher: Mutex::new(None) })
    }

    /// Starts the background work. Call from inside the async runtime.
    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        self.core.spawn(async move { me.run_loop().await });
        let me = self.clone();
        self.core.spawn(async move { me.schedule_loop().await });
        self.rewatch();
    }

    // ----- views -------------------------------------------------------

    pub fn pair_views(&self) -> Vec<PairView> {
        let s = self.core.settings();
        let inner = self.inner.lock().unwrap();
        s.pairs
            .iter()
            .map(|p| {
                let rt = inner.rt.get(&p.id);
                PairView {
                    id: p.id.clone(),
                    name: if p.remote.is_empty() { "Whole account".into() } else { file_name(&p.remote) },
                    local: p.local.clone(),
                    remote: p.remote.clone(),
                    paused: p.paused,
                    phase: rt.map(|r| r.phase).unwrap_or_default(),
                    progress: rt.and_then(|r| r.progress.clone()),
                    last_sync: p.last_sync,
                    error: rt.and_then(|r| r.error.clone()),
                    details: rt.map(|r| r.details.clone()).unwrap_or_default(),
                    approval: rt.and_then(|r| r.approval.clone()),
                    skipped: p.skipped.clone(),
                    conflicts: rt.map(|r| r.conflicts.clone()).unwrap_or_default(),
                    summary: rt.map(|r| r.summary.clone()).unwrap_or_default(),
                }
            })
            .collect()
    }

    pub fn sync_view(&self) -> SyncView {
        let inner = self.inner.lock().unwrap();
        SyncView { paused_all: inner.paused_all, auto_pause: inner.auto_pause.clone() }
    }

    /// True while any folder is syncing or waiting to sync.
    pub fn busy(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.current.is_some() || !inner.queue.is_empty()
    }

    pub fn has_problems(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.rt.values().any(|r| matches!(r.phase, Phase::Error | Phase::Approval))
    }

    /// How many sync runs a folder has had since the app started.
    pub fn run_count(&self, id: &str) -> u64 {
        self.inner.lock().unwrap().rt.get(id).map(|r| r.runs).unwrap_or(0)
    }

    fn with_rt<R>(&self, id: &str, f: impl FnOnce(&mut PairRt) -> R) -> R {
        let mut inner = self.inner.lock().unwrap();
        f(inner.rt.entry(id.to_string()).or_default())
    }

    // ----- commands ----------------------------------------------------

    pub fn enqueue(&self, job: Job) {
        {
            let mut inner = self.inner.lock().unwrap();
            let running = inner.current.as_ref().map(|(id, _)| id == &job.pair_id).unwrap_or(false);
            if running {
                // Let the current run finish, then look again.
                inner.rt.entry(job.pair_id.clone()).or_default().last_event =
                    Some(Instant::now().checked_sub(QUIET).unwrap_or_else(Instant::now));
                return;
            }
            if let Some(q) = inner.queue.iter_mut().find(|q| q.pair_id == job.pair_id) {
                q.force |= job.force;
                q.resync |= job.resync;
                q.manual |= job.manual;
                q.create_remote |= job.create_remote;
            } else {
                let rt = inner.rt.entry(job.pair_id.clone()).or_default();
                if rt.phase != Phase::Approval {
                    rt.phase = Phase::Queued;
                }
                if job.manual {
                    inner.queue.push_front(job);
                } else {
                    inner.queue.push_back(job);
                }
            }
        }
        self.wake.notify_one();
        self.core.host.state_changed();
    }

    pub fn sync_now(&self, id: Option<&str>) {
        let s = self.core.settings();
        for p in s.pairs.iter().filter(|p| !p.paused && id.map(|i| i == p.id).unwrap_or(true)) {
            self.enqueue(Job { pair_id: p.id.clone(), manual: true, ..Default::default() });
        }
    }

    fn cancel_pair(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.queue.retain(|j| j.pair_id != id);
        if let Some((cur, cancel)) = &inner.current {
            if cur == id {
                cancel.cancel();
            }
        }
        if let Some(rt) = inner.rt.get_mut(id) {
            if rt.phase == Phase::Queued {
                rt.phase = Phase::Idle;
            }
        }
    }

    pub fn set_paused(&self, id: &str, paused: bool) {
        self.core.update_pair(id, |p| p.paused = paused);
        if paused {
            self.cancel_pair(id);
        } else {
            self.enqueue(Job { pair_id: id.to_string(), manual: true, ..Default::default() });
        }
        self.core.host.state_changed();
    }

    pub fn set_paused_all(&self, paused: bool) {
        let ids: Vec<String> = self.core.settings().pairs.iter().map(|p| p.id.clone()).collect();
        self.inner.lock().unwrap().paused_all = paused;
        if paused {
            for id in ids {
                self.cancel_pair(&id);
            }
        }
        self.core.host.state_changed();
    }

    /// Stops everything, for example before the account is disconnected.
    pub fn stop_all(&self) {
        let ids: Vec<String> = self.core.settings().pairs.iter().map(|p| p.id.clone()).collect();
        for id in ids {
            self.cancel_pair(&id);
        }
    }

    pub async fn wait_idle(&self) {
        for _ in 0..100 {
            if self.inner.lock().unwrap().current.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Lets a large delete go through. An emptied folder is never passed on
    /// this way: the app does not empty a whole folder by itself.
    pub fn approve_deletes(&self, id: &str) {
        let allowed = self.with_rt(id, |rt| match &rt.approval {
            Some(a) if !a.everything => {
                rt.approval = None;
                rt.phase = Phase::Idle;
                true
            }
            _ => false,
        });
        if allowed {
            self.enqueue(Job { pair_id: id.to_string(), force: true, manual: true, ..Default::default() });
        }
    }

    /// Instead of passing a large delete on, copy the files back.
    pub fn restore_deleted(&self, id: &str) {
        self.with_rt(id, |rt| {
            rt.approval = None;
            rt.phase = Phase::Idle;
        });
        self.enqueue(Job { pair_id: id.to_string(), resync: true, manual: true, create_remote: true, ..Default::default() });
    }

    /// Throws away a folder's sync record and rebuilds it. Never deletes files.
    pub fn repair(&self, id: &str) {
        self.cancel_pair(id);
        let work = self.core.pair_dir(id).join("work");
        let _ = std::fs::remove_dir_all(&work);
        self.core.update_pair(id, |p| p.needs_resync = true);
        self.with_rt(id, |rt| {
            rt.approval = None;
            rt.error = None;
            rt.details.clear();
            rt.fails = 0;
            rt.retry_skipped = true;
        });
        self.enqueue(Job { pair_id: id.to_string(), resync: true, manual: true, create_remote: true, ..Default::default() });
    }

    pub fn retry_skipped(&self, id: &str) {
        self.with_rt(id, |rt| rt.retry_skipped = true);
        self.enqueue(Job { pair_id: id.to_string(), manual: true, ..Default::default() });
    }

    pub fn dismiss_conflicts(&self, id: &str) {
        self.with_rt(id, |rt| rt.conflicts.clear());
        self.core.host.state_changed();
    }

    /// Checks a new folder pairing and saves it.
    pub fn add_pair(self: &Arc<Self>, local: &str, remote: &str) -> Result<Pair, String> {
        let s = self.core.settings();
        let local = local.trim().to_string();
        let remote = clean_remote(remote);
        let lp = Path::new(&local);
        if local.is_empty() || !lp.is_absolute() {
            return Err("Pick a folder on this PC.".into());
        }
        if lp.parent().is_none() {
            return Err("Pick a folder, not a whole drive.".into());
        }
        let ln = norm_local(&local);
        let sep = std::path::MAIN_SEPARATOR;
        if paths_overlap(&ln, &norm_local(&self.core.data.to_string_lossy()), sep) {
            return Err("That folder holds SyncHopper's own files. Pick a different one.".into());
        }
        if s.feature_drive && cfg!(windows) && ln.starts_with(&format!("{}:", s.drive_letter.to_lowercase())) {
            return Err("That folder is on the Koofr drive, which is already Koofr. Pick a folder on this PC.".into());
        }
        for p in &s.pairs {
            if paths_overlap(&ln, &norm_local(&p.local), sep) {
                return Err(format!("That PC folder overlaps one that is already synced: {}", p.local));
            }
            if paths_overlap(&remote.to_lowercase(), &p.remote.to_lowercase(), '/') {
                let shown = if p.remote.is_empty() { "the whole account" } else { p.remote.as_str() };
                return Err(format!("That Koofr folder overlaps one that is already synced: {shown}"));
            }
        }
        std::fs::create_dir_all(lp).map_err(|e| format!("Could not create {local}: {e}"))?;
        let pair = Pair { id: new_id("p"), local, remote, needs_resync: true, ..Default::default() };
        let saved = pair.clone();
        self.core.update(|s| s.pairs.push(saved));
        self.rewatch();
        self.enqueue(Job { pair_id: pair.id.clone(), resync: true, create_remote: true, ..Default::default() });
        Ok(pair)
    }

    /// Stops syncing a folder. Files stay where they are on both sides.
    pub fn remove_pair(self: &Arc<Self>, id: &str) {
        self.cancel_pair(id);
        self.core.update(|s| s.pairs.retain(|p| p.id != id));
        self.inner.lock().unwrap().rt.remove(id);
        self.rewatch();
        let me = self.clone();
        let id = id.to_string();
        self.core.spawn(async move {
            me.wait_idle().await;
            let _ = std::fs::remove_dir_all(me.core.pair_dir(&id));
        });
    }

    /// Call when the ignore list changed.
    pub fn filters_changed(self: &Arc<Self>) {
        self.core.update(|s| {
            for p in s.pairs.iter_mut() {
                p.needs_resync = true;
            }
        });
        self.rewatch();
        self.sync_now(None);
    }

    // ----- watching for local changes ----------------------------------

    /// (Re)starts watching every synced folder for local changes.
    pub fn rewatch(self: &Arc<Self>) {
        let s = self.core.settings();
        let roots: Vec<(String, PathBuf)> = s.pairs.iter().map(|p| (p.id.clone(), PathBuf::from(&p.local))).collect();
        let ignores = s.ignore_patterns.clone();
        let me = Arc::downgrade(self);
        let watch_roots = roots.clone();
        let handler = move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            let Some(me) = me.upgrade() else { return };
            for path in &ev.paths {
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                if name.ends_with(".partial") || ignores.iter().any(|g| glob_match(g, &name)) {
                    continue;
                }
                if let Some((id, _)) = watch_roots.iter().find(|(_, root)| path.starts_with(root)) {
                    me.with_rt(id, |rt| rt.last_event = Some(Instant::now()));
                }
            }
        };
        let mut slot = self.watcher.lock().unwrap();
        *slot = None;
        if !s.feature_sync || !s.setup_complete {
            return;
        }
        match notify::recommended_watcher(handler) {
            Ok(mut w) => {
                for (_, root) in &roots {
                    if let Err(e) = w.watch(root, RecursiveMode::Recursive) {
                        self.core.log(&format!("Could not watch {}: {e}", root.display()));
                    }
                }
                *slot = Some(w);
            }
            Err(e) => self.core.log(&format!("File watching is not available: {e}")),
        }
    }

    // ----- scheduling --------------------------------------------------

    async fn refresh_auto_pause(&self, s: &Settings) {
        let due = {
            let inner = self.inner.lock().unwrap();
            inner.auto_checked.map(|t| t.elapsed() > POWER_CHECK).unwrap_or(true)
        };
        if !due {
            return;
        }
        if !s.pause_on_battery && !s.pause_on_metered {
            let mut inner = self.inner.lock().unwrap();
            inner.auto_checked = Some(Instant::now());
            if inner.auto_pause.take().is_some() {
                drop(inner);
                self.core.host.state_changed();
            }
            return;
        }
        let mut reason = None;
        if s.pause_on_battery && platform::on_battery().await == Some(true) {
            reason = Some("battery".to_string());
        } else if s.pause_on_metered && platform::metered().await == Some(true) {
            reason = Some("metered".to_string());
        }
        let changed = {
            let mut inner = self.inner.lock().unwrap();
            inner.auto_checked = Some(Instant::now());
            let changed = inner.auto_pause != reason;
            inner.auto_pause = reason;
            changed
        };
        if changed {
            self.core.host.state_changed();
        }
    }

    async fn schedule_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let s = self.core.settings();
            if !s.setup_complete || !s.feature_sync || self.core.engine_info().exe.is_none() {
                continue;
            }
            self.refresh_auto_pause(&s).await;
            let mut due: Vec<String> = Vec::new();
            {
                let mut inner = self.inner.lock().unwrap();
                if inner.paused_all || inner.auto_pause.is_some() {
                    continue;
                }
                let running = inner.current.as_ref().map(|(id, _)| id.clone());
                for p in s.pairs.iter().filter(|p| !p.paused) {
                    if running.as_deref() == Some(p.id.as_str()) || inner.queue.iter().any(|j| j.pair_id == p.id) {
                        continue;
                    }
                    let rt = inner.rt.entry(p.id.clone()).or_default();
                    let quiet = rt.last_event.map(|t| t.elapsed() >= QUIET).unwrap_or(true);
                    let rested = rt.last_finished.map(|t| t.elapsed() >= BREATHER).unwrap_or(true);
                    let changed = rt.last_event.is_some() && quiet && rested;
                    let timer = rt.next_due.map(|t| Instant::now() >= t).unwrap_or(true);
                    if changed || (timer && quiet) {
                        due.push(p.id.clone());
                    }
                }
            }
            for id in due {
                self.enqueue(Job { pair_id: id, ..Default::default() });
            }
        }
    }

    /// Runs everything that is waiting, then returns.
    pub async fn drain(self: &Arc<Self>) {
        loop {
            let job = self.inner.lock().unwrap().queue.pop_front();
            match job {
                Some(job) => self.run_pair(job).await,
                None => break,
            }
        }
    }

    async fn run_loop(self: Arc<Self>) {
        loop {
            self.drain().await;
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    // ----- running one sync --------------------------------------------

    fn bisync_args(&self, pair: &Pair, s: &Settings, work: &Path, filters: &Path, resync: bool, force: bool) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "bisync".into(),
            pair.local.clone(),
            self.core.remote(&pair.remote),
            "--workdir".into(),
            work.to_string_lossy().to_string(),
            "--filters-file".into(),
            filters.to_string_lossy().to_string(),
            "--resilient".into(),
            "--recover".into(),
            "--max-lock".into(),
            "2m".into(),
            "--conflict-resolve".into(),
            "newer".into(),
            "--conflict-loser".into(),
            if s.conflict_policy == "newest" { "delete".into() } else { "num".into() },
            "--suffix-keep-extension".into(),
            "--max-delete".into(),
            s.max_delete_percent.to_string(),
            "--create-empty-src-dirs".into(),
            "--transfers".into(),
            s.transfers.to_string(),
            "--retries".into(),
            "3".into(),
            "--retries-sleep".into(),
            "5s".into(),
            "--use-json-log".into(),
            "--log-level".into(),
            "INFO".into(),
            "--stats".into(),
            "1s".into(),
            "--stats-log-level".into(),
            "NOTICE".into(),
            "--color".into(),
            "NEVER".into(),
        ];
        if !s.use_http2 {
            a.push("--disable-http2".into());
        }
        if let Some(limit) = s.bwlimit() {
            a.push("--bwlimit".into());
            a.push(limit);
        }
        if resync {
            a.push("--resync".into());
            a.push("--resync-mode".into());
            a.push("newer".into());
        }
        if force {
            a.push("--force".into());
        }
        a
    }

    fn finish(&self, id: &str, interval_min: u32, f: impl FnOnce(&mut PairRt)) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.current = None;
            let rt = inner.rt.entry(id.to_string()).or_default();
            rt.progress = None;
            rt.runs += 1;
            rt.last_finished = Some(Instant::now());
            f(rt);
            let wait = match rt.phase {
                Phase::Error => match rt.fails {
                    0 | 1 => 60,
                    2 => 120,
                    3 => 300,
                    _ => interval_min as u64 * 60,
                }
                .min(interval_min as u64 * 60),
                _ => interval_min as u64 * 60,
            };
            rt.next_due = Some(Instant::now() + Duration::from_secs(wait.max(5)));
        }
        self.core.host.state_changed();
    }

    fn fail(&self, pair: &Pair, interval_min: u32, message: String, details: Vec<String>) {
        let friendly = friendly_error(&message);
        self.core.log(&format!("[{}] FAILED: {}", pair.local, message));
        let mut first = false;
        self.finish(&pair.id, interval_min, |rt| {
            rt.phase = Phase::Error;
            rt.fails += 1;
            // A network hiccup (for example right after signing in to
            // Windows) only earns a notification if it lasts.
            first = if friendly.starts_with("Could not reach Koofr") { rt.fails == 3 } else { rt.fails == 1 };
            rt.error = Some(friendly.clone());
            rt.details = details;
        });
        if first {
            let name = file_name(&pair.local);
            self.core.notify(Notice::Problem, &format!("{name} could not sync"), &friendly);
        }
    }

    /// Parks a folder until the user decides what to do about a large delete.
    fn ask_approval(&self, pair: &Pair, interval_min: u32, a: Approval) {
        let name = file_name(&pair.local);
        let body = if a.everything {
            let place = if a.side == "pc" { "on this PC" } else { "in Koofr" };
            format!("Everything in this folder is gone {place}. SyncHopper is waiting for you to decide what to do.")
        } else {
            format!("{} of {} files were deleted. SyncHopper is waiting for your OK before passing that on.", a.deleted, a.total)
        };
        let mut fresh = false;
        self.finish(&pair.id, interval_min, |rt| {
            fresh = rt.approval.is_none();
            rt.phase = Phase::Approval;
            rt.approval = Some(a);
            rt.error = None;
            rt.details.clear();
        });
        if fresh {
            self.core.notify(Notice::Problem, &format!("{name} needs your OK"), &body);
        }
    }

    /// Adds files that failed to the folder's skip list when the rules say
    /// so. Returns true when the list grew.
    fn update_skips(&self, pair: &Pair, files: &[(String, String)], rebuild: bool) -> bool {
        let mut add: Vec<Skipped> = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            let rt = inner.rt.entry(pair.id.clone()).or_default();
            for (object, msg) in files {
                let path = object.replace('\\', "/");
                if pair.skipped.iter().any(|k| k.path == path) {
                    continue;
                }
                let count = rt.fail_counts.entry(path.clone()).or_insert(0);
                *count += 1;
                let issue = file_issue(msg);
                let skip = match issue {
                    FileIssue::Busy | FileIssue::Stuck => true,
                    FileIssue::Unknown => *count >= 3,
                };
                if skip {
                    add.push(Skipped {
                        path,
                        reason: friendly_error(msg),
                        sticky: issue == FileIssue::Stuck || *count >= 3,
                        tries: *count,
                    });
                }
            }
        }
        if add.is_empty() {
            return false;
        }
        for k in &add {
            self.core.log(&format!("[{}] Leaving out {} for now: {}", pair.local, k.path, k.reason));
        }
        self.core.update_pair(&pair.id, |p| {
            p.skipped.extend(add);
            if rebuild {
                p.needs_resync = true;
            }
        });
        true
    }

    /// Puts skipped files back into syncing when they look ready. Returns
    /// how many were put back.
    fn unskip_ready(&self, pair: &Pair) -> usize {
        if pair.skipped.is_empty() {
            self.with_rt(&pair.id, |rt| rt.retry_skipped = false);
            return 0;
        }
        let all = self.with_rt(&pair.id, |rt| std::mem::take(&mut rt.retry_skipped));
        let keep: Vec<Skipped> = pair
            .skipped
            .iter()
            .filter(|k| {
                if all {
                    return false;
                }
                if k.sticky {
                    return true;
                }
                let p = Path::new(&pair.local).join(k.path.replace('/', std::path::MAIN_SEPARATOR_STR));
                // Still there and still unreadable: keep leaving it out.
                p.exists() && std::fs::File::open(&p).is_err()
            })
            .cloned()
            .collect();
        let removed = pair.skipped.len() - keep.len();
        if removed > 0 {
            if all {
                self.with_rt(&pair.id, |rt| rt.fail_counts.clear());
            }
            self.core.update_pair(&pair.id, |p| {
                p.skipped = keep;
                p.needs_resync = true;
            });
        }
        removed
    }

    /// Runs one folder sync from start to finish, including the automatic
    /// second and third tries that some situations call for.
    pub async fn run_pair(self: &Arc<Self>, job: Job) {
        let s = self.core.settings();
        let Some(first) = self.core.pair(&job.pair_id) else { return };
        if first.paused && !job.manual {
            self.with_rt(&first.id, |rt| rt.phase = Phase::Idle);
            return;
        }
        let cancel = Cancel::default();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.current = Some((first.id.clone(), cancel.clone()));
            let rt = inner.rt.entry(first.id.clone()).or_default();
            if rt.phase != Phase::Approval {
                rt.phase = Phase::Checking;
            }
            rt.progress = None;
            rt.last_event = None;
        }
        self.core.host.state_changed();
        let interval = s.check_interval_min;

        let rc = match self.core.rclone() {
            Ok(rc) => rc,
            Err(e) => return self.fail(&first, interval, e, vec![]),
        };
        if !Path::new(&first.local).is_dir() {
            let msg = format!("This folder is missing on this PC: {}. Nothing was changed in Koofr.", first.local);
            return self.fail(&first, interval, msg, vec![]);
        }
        let pdir = self.core.pair_dir(&first.id);
        let work = pdir.join("work");
        let filters = pdir.join("filters.txt");
        if let Err(e) = std::fs::create_dir_all(&work) {
            return self.fail(&first, interval, format!("Could not create the sync record folder: {e}"), vec![]);
        }

        let mut want_resync = job.resync;
        let mut force = job.force;
        let mut unskip_done = false;
        // Set when files were just left out and the last good record was put
        // back, so the next pass can be a normal sync instead of a rebuild.
        let mut soft_filters = false;
        let mut conflicts: Vec<String> = Vec::new();
        let mut moved: u64 = 0;

        for pass in 0..5 {
            let Some(pair) = self.core.pair(&job.pair_id) else {
                self.inner.lock().unwrap().current = None;
                return;
            };
            let text = filters_text(&s, &pair);
            let filters_new = std::fs::read_to_string(&filters).map(|old| old != text).unwrap_or(true);
            let soft = std::mem::take(&mut soft_filters) && !want_resync && !pair.needs_resync;
            if filters_new {
                if let Err(e) = write_filters(&filters, &text, soft) {
                    return self.fail(&pair, interval, format!("Could not write the ignore list: {e}"), vec![]);
                }
                if !soft && !pair.needs_resync {
                    self.core.update_pair(&pair.id, |p| p.needs_resync = true);
                }
            }
            let resync = want_resync || pair.needs_resync || (filters_new && !soft) || !listing_ready(&work);
            clear_locks(&work);
            if resync {
                purge_set_aside(&work);
            }

            if resync && (pair.last_sync == 0 || job.create_remote) {
                if let Err(e) = rc.mkdir(&self.core.remote(&pair.remote)).await {
                    return self.fail(&pair, interval, e.clone(), vec![e]);
                }
            }
            if cancel.is_cancelled() {
                return self.finish(&pair.id, interval, |rt| rt.phase = Phase::Idle);
            }

            let args = self.bisync_args(&pair, &s, &work, &filters, resync, force);
            self.core.log(&format!(
                "[{}] Sync starting{}{}",
                pair.local,
                if resync { " (rebuilding the sync record)" } else { "" },
                if force { " (large delete allowed)" } else { "" }
            ));
            let me = self.clone();
            let id = pair.id.clone();
            let label = pair.local.clone();
            let mut found: Vec<String> = Vec::new();
            let res: RunResult = rc
                .run_logged(&args, &cancel, |line| {
                    if let Some(st) = &line.stats {
                        me.on_stats(&id, st);
                        return;
                    }
                    // rclone names a winner only for files that really differ
                    // on the two sides, so this marks a true conflict.
                    if line.msg.starts_with("The winner is") && !line.object.is_empty() && !found.contains(&line.object) {
                        found.push(line.object.clone());
                    }
                    if !line.msg.starts_with("Bisyncing with Comparison Settings") {
                        let object = if line.object.is_empty() { String::new() } else { format!("{}: ", line.object) };
                        me.core.log(&format!("[{}] {} {}{}", label, line.level, object, line.msg));
                    }
                })
                .await;
            if let Some(st) = &res.stats {
                moved += st.transfers + st.deletes;
            }
            conflicts.extend(found);

            match classify(&res) {
                Verdict::Ok => {
                    self.core.update_pair(&pair.id, |p| {
                        p.needs_resync = false;
                        p.last_sync = now_secs();
                    });
                    want_resync = false;
                    force = false;
                    if !unskip_done {
                        unskip_done = true;
                        let fresh = self.core.pair(&pair.id).unwrap_or(pair.clone());
                        if self.unskip_ready(&fresh) > 0 {
                            continue;
                        }
                    }
                    let left_out = self.core.pair(&pair.id).map(|p| p.skipped.len()).unwrap_or(0);
                    let summary = match moved {
                        0 => String::new(),
                        1 => "1 change synced".to_string(),
                        n => format!("{n} changes synced"),
                    };
                    let conflict_count = conflicts.len();
                    self.finish(&pair.id, interval, |rt| {
                        rt.phase = Phase::Ok;
                        rt.fails = 0;
                        rt.error = None;
                        rt.details.clear();
                        rt.approval = None;
                        rt.summary = summary.clone();
                        for c in conflicts.drain(..) {
                            if !rt.conflicts.contains(&c) && rt.conflicts.len() < 100 {
                                rt.conflicts.push(c);
                            }
                        }
                        if left_out == 0 {
                            rt.fail_counts.clear();
                        }
                    });
                    self.core.log(&format!("[{}] Sync finished. {}", pair.local, summary));
                    let name = file_name(&pair.local);
                    if conflict_count > 0 && s.conflict_policy != "newest" {
                        self.core.notify(
                            Notice::Problem,
                            &format!("{name}: files changed in both places"),
                            "The newest version kept its name. The other was saved next to it as a conflict copy.",
                        );
                    } else if moved > 0 {
                        self.core.notify(Notice::Info, &format!("{name} is up to date"), &summary);
                    }
                    return;
                }
                Verdict::Cancelled => {
                    self.core.log(&format!("[{}] Sync stopped.", pair.local));
                    return self.finish(&pair.id, interval, |rt| rt.phase = Phase::Idle);
                }
                Verdict::LockFound if pass < 4 => continue,
                Verdict::NeedsResync if !resync && pass < 4 => {
                    self.core.update_pair(&pair.id, |p| p.needs_resync = true);
                    want_resync = true;
                    continue;
                }
                Verdict::TooManyDeletes(_) if soft && pass < 4 => {
                    // Files that were just left out look like deletions to
                    // rclone's safety check. Rebuild the record instead of
                    // bothering the user with a count that is not real.
                    self.core.update_pair(&pair.id, |p| p.needs_resync = true);
                    want_resync = true;
                    continue;
                }
                Verdict::TooManyDeletes(a) => {
                    self.core.log(&format!("[{}] Stopped before deleting {} of {} files.", pair.local, a.deleted, a.total));
                    return self.ask_approval(&pair, interval, a);
                }
                Verdict::SideEmpty(side) => {
                    // rclone refuses to sync against a side that has become
                    // completely empty. If the other side is empty too there
                    // is nothing to lose, so the record is simply rebuilt.
                    let other = if side == "pc" { self.core.remote(&pair.remote) } else { pair.local.clone() };
                    let filters_text = filters.to_string_lossy().to_string();
                    let counted = rc
                        .capture(&["size", &other, "--filter-from", &filters_text, "--json"], Duration::from_secs(900))
                        .await
                        .ok()
                        .and_then(|out| serde_json::from_str::<serde_json::Value>(&out).ok())
                        .and_then(|v| v.get("count").and_then(|c| c.as_u64()));
                    match counted {
                        Some(0) if pass < 4 => {
                            self.core.update_pair(&pair.id, |p| p.needs_resync = true);
                            want_resync = true;
                            continue;
                        }
                        Some(n) => {
                            self.core.log(&format!("[{}] One side is empty ({side}). {n} files are still on the other side.", pair.local));
                            let a = Approval { deleted: n, total: n, side, everything: true };
                            return self.ask_approval(&pair, interval, a);
                        }
                        None => {
                            let msg = "Could not check the folder. It will be tried again automatically.".to_string();
                            return self.fail(&pair, interval, msg, vec![]);
                        }
                    }
                }
                Verdict::RemoteMissing => {
                    let shown = if pair.remote.is_empty() { "your Koofr account".to_string() } else { format!("\"{}\"", pair.remote) };
                    let msg = format!("The Koofr folder {shown} was not found. It may have been renamed or deleted in Koofr. Nothing was changed on this PC.");
                    return self.fail(&pair, interval, msg, vec![]);
                }
                Verdict::Failed { must_resync, files, message, details } => {
                    // rclone locks a folder after most failures and wants a
                    // full rebuild. After an ordinary sync the last good
                    // record can be put back instead, so nothing that was
                    // waiting (deletes included) is lost or undone.
                    let restored = must_resync && !resync && restore_records(&work);
                    if must_resync && !restored {
                        self.core.update_pair(&pair.id, |p| p.needs_resync = true);
                    }
                    if self.update_skips(&pair, &files, !restored) && pass < 4 && !cancel.is_cancelled() {
                        if restored {
                            soft_filters = true;
                        } else {
                            want_resync = true;
                        }
                        continue;
                    }
                    return self.fail(&pair, interval, message, details);
                }
                other => {
                    let msg = format!("The sync could not get going ({other:?}).");
                    return self.fail(&pair, interval, msg, vec![]);
                }
            }
        }
        if let Some(pair) = self.core.pair(&job.pair_id) {
            self.fail(&pair, interval, "The sync kept having to start over. It will be tried again.".into(), vec![]);
        }
    }

    fn on_stats(&self, id: &str, st: &Stats) {
        {
            let mut inner = self.inner.lock().unwrap();
            let rt = inner.rt.entry(id.to_string()).or_default();
            if st.total_transfers > 0 || st.bytes > 0 {
                rt.phase = Phase::Syncing;
                rt.progress = Some(Progress {
                    percent: if st.total_bytes > 0 { (st.bytes as f64 / st.total_bytes as f64 * 100.0).min(100.0) } else { 0.0 },
                    done: st.transfers,
                    total: st.total_transfers,
                    speed: st.speed,
                    eta: st.eta,
                    file: st.current.first().cloned().unwrap_or_default(),
                });
            }
        }
        self.core.host.state_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(msg: &str, object: &str, ty: &str) -> LogLine {
        LogLine { level: "error".into(), msg: msg.into(), object: object.into(), object_type: ty.into(), stats: None }
    }

    #[test]
    fn deletes_message() {
        let a = parse_deletes("too many deletes (>50%, 100 of 140) on Path1 \"/x/local/\". Run with --force if desired.").unwrap();
        assert_eq!(a, Approval { deleted: 100, total: 140, side: "pc".into(), everything: false });
        let b = parse_deletes("too many deletes (>10%, 3 of 9) on Path2 \"koofr:Docs\". Run with --force if desired.").unwrap();
        assert_eq!(b.side, "koofr");
    }

    #[test]
    fn verdicts() {
        let mut r = RunResult { exit: 0, ..Default::default() };
        assert_eq!(classify(&r), Verdict::Ok);

        r = RunResult { exit: 1, ..Default::default() };
        r.errors.push(err("too many deletes (>50%, 2 of 3) on Path1 \"/a\". Run with --force if desired.", "", ""));
        assert!(matches!(classify(&r), Verdict::TooManyDeletes(_)));

        r = RunResult { exit: 7, ..Default::default() };
        r.errors.push(err("Bisync critical error: filters file has changed (must run --resync): /f", "", ""));
        r.errors.push(err("Bisync aborted. Error is retryable without --resync due to --resilient mode.", "", ""));
        assert_eq!(classify(&r), Verdict::NeedsResync);

        r = RunResult { exit: 7, ..Default::default() };
        r.errors.push(err("Failed to copy: failed to open source object: open /a/bad.txt: permission denied", "bad.txt", "*local.Object"));
        r.errors.push(err("not deleting files as there were IO errors", "Local file system at /b", "*local.Fs"));
        r.errors.push(err("Bisync critical error: failed to open source object", "", ""));
        r.errors.push(err("Bisync aborted. Must run --resync to recover.", "", ""));
        match classify(&r) {
            Verdict::Failed { must_resync, files, .. } => {
                assert!(must_resync);
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].0, "bad.txt");
            }
            v => panic!("unexpected {v:?}"),
        }

        r = RunResult { exit: 7, ..Default::default() };
        r.errors.push(err("error during march: directory not found", "", ""));
        assert_eq!(classify(&r), Verdict::RemoteMissing);

        r = RunResult { exit: 7, ..Default::default() };
        r.errors.push(err("Empty current Path2 listing. Cannot sync to an empty directory: /w/x.path2.lst-new", "", ""));
        r.errors.push(err("Bisync critical error: empty current Path2 listing: /w/x.path2.lst-new", "", ""));
        assert_eq!(classify(&r), Verdict::SideEmpty("koofr".into()));

        r = RunResult { exit: 1, ..Default::default() };
        r.notices.push(LogLine { level: "notice".into(), msg: "Failed to bisync: prior lock file found: /x.lck".into(), ..Default::default() });
        assert_eq!(classify(&r), Verdict::LockFound);
    }

    #[test]
    fn issues() {
        assert_eq!(file_issue("open x: The process cannot access the file because it is being used by another process."), FileIssue::Busy);
        assert_eq!(file_issue("The filename or extension is too long."), FileIssue::Stuck);
        assert_eq!(file_issue("unexpected EOF"), FileIssue::Unknown);
    }

    struct NoHost;
    impl crate::core::Host for NoHost {
        fn state_changed(&self) {}
        fn notify(&self, _: &str, _: &str) {}
        fn set_autostart(&self, _: bool) -> Result<(), String> {
            Ok(())
        }
        fn exe_path(&self) -> Option<PathBuf> {
            None
        }
        fn show_window(&self, _: Option<&str>) {}
    }

    #[test]
    fn skip_list_rules() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("pc");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(local.join("readable.txt"), "x").unwrap();
        let core = Core::new(dir.path().join("data"), "koofr:".into(), true, Arc::new(NoHost));
        let engine = Engine::new(core.clone());
        let pair = Pair { id: "p1".into(), local: local.to_string_lossy().to_string(), remote: "Docs".into(), ..Default::default() };
        core.update(|s| s.pairs.push(pair.clone()));
        let get = || core.pair("p1").unwrap();

        // A busy file is left out at once and is not sticky.
        let busy = vec![("readable.txt".to_string(), "open x: The process cannot access the file because it is being used by another process.".to_string())];
        assert!(engine.update_skips(&get(), &busy, true));
        assert_eq!(get().skipped.len(), 1);
        assert!(!get().skipped[0].sticky);
        assert!(get().needs_resync);
        // Seeing the same failure again does not add it twice.
        assert!(!engine.update_skips(&get(), &busy, true));

        // An unknown failure is only left out the third time, and then stays out.
        let odd = vec![("sub/odd.bin".to_string(), "Failed to copy: unexpected EOF".to_string())];
        assert!(!engine.update_skips(&get(), &odd, true));
        assert!(!engine.update_skips(&get(), &odd, true));
        assert!(engine.update_skips(&get(), &odd, true));
        let odd_entry = get().skipped.iter().find(|k| k.path == "sub/odd.bin").cloned().unwrap();
        assert!(odd_entry.sticky);

        // A name Windows cannot store is left out at once and stays out.
        let long = vec![("deep/name.txt".to_string(), "The filename or extension is too long.".to_string())];
        assert!(engine.update_skips(&get(), &long, false));
        assert!(get().skipped.iter().find(|k| k.path == "deep/name.txt").unwrap().sticky);

        // The busy file can be opened now, so it goes back in. Sticky ones stay.
        assert_eq!(engine.unskip_ready(&get()), 1);
        let left: Vec<String> = get().skipped.iter().map(|k| k.path.clone()).collect();
        assert_eq!(left, vec!["sub/odd.bin".to_string(), "deep/name.txt".to_string()]);

        // "Try again" puts everything back in.
        engine.with_rt("p1", |rt| rt.retry_skipped = true);
        assert_eq!(engine.unskip_ready(&get()), 2);
        assert!(get().skipped.is_empty());
    }

    #[test]
    fn records_are_put_back_only_when_complete() {
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path();
        std::fs::write(w.join("a..b.path1.lst-err"), "# x\n- 1 - - t \"f\"\n").unwrap();
        assert!(!restore_records(w), "one side alone is not enough");
        std::fs::write(w.join("a..b.path2.lst-err"), "# x\n- 1 - - t \"f\"\n").unwrap();
        assert!(restore_records(w));
        assert!(w.join("a..b.path1.lst").exists() && w.join("a..b.path2.lst").exists());
        assert!(!w.join("a..b.path2.lst-err").exists());
        assert!(listing_ready(w));
        std::fs::write(w.join("a..b.path1.lst-err"), "stale").unwrap();
        purge_set_aside(w);
        assert!(!w.join("a..b.path1.lst-err").exists());
    }

    #[test]
    fn filter_checksum_is_plain_md5() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("filters.txt");
        write_filters(&f, "hello\n", true).unwrap();
        // The well-known md5 of "hello\n", as md5sum prints it.
        assert_eq!(std::fs::read_to_string(dir.path().join("filters.txt.md5")).unwrap(), "b1946ac92492d2347c6235b4d2611184");
        // Without keep_record the checksum is left for rclone to write.
        let g = dir.path().join("other.txt");
        write_filters(&g, "hello\n", false).unwrap();
        assert!(!dir.path().join("other.txt.md5").exists());
    }

    #[test]
    fn filter_file() {
        let s = Settings::default();
        let mut p = Pair::default();
        p.skipped.push(Skipped { path: "sub dir/we[ird] name.txt".into(), ..Default::default() });
        let t = filters_text(&s, &p);
        assert!(t.contains("- ~$*\n"));
        assert!(t.contains("- /sub dir/we\\[ird\\] name.txt\n"));
    }
}
