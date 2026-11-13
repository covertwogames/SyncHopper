//! The sync engine: a queue of folder syncs run one at a time through
//! rclone's two-way sync, with a timer, a file watcher, and recovery from
//! the ways a sync can fail.

use crate::config::{same_state, FileState, Pair, Settings, Skipped};
use crate::core::{Core, Notice};
use crate::platform;
use crate::rclone::{friendly_error, LogLine, Rclone, RunResult, Stats};
use crate::util::{clean_remote, file_name, glob_match, go_unquote, new_id, norm_local, now_secs, parse_time_ns, paths_overlap, Cancel};
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
/// How long a failing file is retried quietly before it is shown as
/// needing attention.
pub const FLAG_AFTER: u64 = 3 * 3600;
/// How long a file must keep failing before "Ignore this file" is offered,
/// unless it is a file that can never sync as it is.
pub const IGNORE_AFTER: u64 = 24 * 3600;

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
    /// Koofr is down or out of reach. The folder tries again by itself.
    Waiting,
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
    retry_all: bool,
    waiting: Option<String>,
    retry_at: Option<u64>,
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
    outage_notified: bool,
}

/// A file that is being left out of syncing, as the screens see it.
#[derive(Clone, Serialize)]
pub struct FileView {
    pub path: String,
    pub reason: String,
    pub detail: String,
    pub kind: String,
    /// Shown as needing attention rather than being retried quietly.
    pub flagged: bool,
    /// "Ignore this file" may be offered.
    pub can_ignore: bool,
    pub next_retry: u64,
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
    pub files: Vec<FileView>,
    pub ignored: Vec<String>,
    pub waiting: Option<String>,
    pub retry_at: Option<u64>,
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
    Failed { must_resync: bool, files: Vec<FailedFile>, message: String, details: Vec<String> },
}

/// One file that failed during a sync run.
#[derive(Debug, PartialEq, Clone)]
pub struct FailedFile {
    pub path: String,
    pub msg: String,
    /// rclone's object type, which tells which side the file was read from.
    pub object_type: String,
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

    let mut files: Vec<FailedFile> = Vec::new();
    for l in &res.errors {
        let file_like = l.object_type.ends_with("Object") || l.msg.starts_with("Failed to copy");
        if !l.object.is_empty() && file_like && !files.iter().any(|f| f.path == l.object) {
            files.push(FailedFile { path: l.object.clone(), msg: l.msg.clone(), object_type: l.object_type.clone() });
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
        .map(|f| f.msg.clone())
        .or_else(|| details.first().map(|m| m.trim_start_matches("Bisync critical error: ").to_string()))
        .unwrap_or_else(|| format!("The sync engine stopped unexpectedly (code {}).", res.exit));
    Verdict::Failed { must_resync, files, message, details }
}

/// What kind of failure a message describes. Per-file kinds are "server",
/// "network", "busy", "denied", "permanent" and "unknown". "auth", "quota"
/// and "disk" are problems for the whole folder, not for one file.
pub fn issue_kind(msg: &str) -> &'static str {
    let l = msg.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    // An HTTP status code standing on its own, not part of a longer number.
    let code = |c: &str| l.split(|ch: char| !ch.is_ascii_digit()).any(|tok| tok == c);
    if code("401") || has(&["unauthorized", "invalid credentials", "authentication failed", "couldn't login"]) {
        "auth"
    } else if code("507") || has(&["quota", "insufficient storage", "storage is full"]) {
        "quota"
    } else if has(&["not enough space", "no space left", "disk full", "there is not enough space"]) {
        "disk"
    } else if has(&["invalid response status! got 5", "internal server error", "bad gateway", "service unavailable", "gateway timeout"])
        || code("500")
        || code("502")
        || code("503")
        || code("504")
    {
        "server"
    } else if has(&[
        "no such host", "dial tcp", "connection refused", "i/o timeout", "network is unreachable",
        "tls handshake", "connection reset", "unexpected eof", "context deadline exceeded", "broken pipe",
        "took too long", "could not reach koofr",
    ]) {
        "network"
    } else if has(&["being used by another process", "sharing violation", "source file is being updated", "size changed"]) {
        "busy"
    } else if has(&["access is denied", "permission denied"]) {
        "denied"
    } else if has(&["too long", "syntax is incorrect", "invalid argument", "illegal characters", "name is not allowed"]) {
        "permanent"
    } else {
        "unknown"
    }
}

fn outage_kind(kind: &str) -> bool {
    kind == "server" || kind == "network"
}

/// Seconds to wait before trying a left-out file again.
fn retry_delay(kind: &str, tries: u32) -> u64 {
    match kind {
        "permanent" => u64::MAX / 4,
        // Cheap to check: the file is looked at locally before anything is sent.
        "busy" | "denied" => 120,
        _ => match tries {
            0 | 1 => 5 * 60,
            2 => 15 * 60,
            3 => 30 * 60,
            _ => 60 * 60,
        },
    }
}

/// Plain-language reason for a file that could not sync.
pub fn file_reason(kind: &str, object_type: &str, raw: &str) -> String {
    let from_koofr = object_type.to_lowercase().contains("koofr");
    match kind {
        "server" if from_koofr => "Koofr couldn't send this file. This is usually a temporary problem on Koofr's side.".into(),
        "server" => "Koofr had a problem with this file. This is usually a temporary problem on Koofr's side.".into(),
        "network" => "The connection to Koofr dropped while this file was syncing.".into(),
        "busy" => {
            if raw.to_lowercase().contains("being updated") || raw.to_lowercase().contains("size changed") {
                "This file was still being written while it synced.".into()
            } else {
                "This file is open in another program.".into()
            }
        }
        "denied" => "Windows wouldn't let SyncHopper read or write this file.".into(),
        "permanent" => {
            if raw.to_lowercase().contains("too long") {
                "This file's name or path is too long for Windows.".into()
            } else {
                "This file's name can't be used on Windows or in Koofr.".into()
            }
        }
        _ => friendly_error(raw),
    }
}

/// Reads one side's sync record into path and file state.
fn read_listing(work: &Path, suffix: &str) -> HashMap<String, FileState> {
    let mut map = HashMap::new();
    let Ok(entries) = std::fs::read_dir(work) else { return map };
    for e in entries.flatten() {
        if !e.file_name().to_string_lossy().ends_with(suffix) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            // Example: -        5 - - 2026-11-13T15:10:34.650367249+0000 "sub dir/f1.txt"
            let Some(q) = line.find('"') else { continue };
            let quoted = line[q..].trim_end();
            if quoted.len() < 2 || !quoted.ends_with('"') {
                continue;
            }
            let path = go_unquote(&quoted[1..quoted.len() - 1]);
            let fields: Vec<&str> = line[..q].split_whitespace().collect();
            if fields.len() < 3 {
                continue;
            }
            let size = fields[1].parse::<u64>().ok();
            let mtime = fields.last().and_then(|t| parse_time_ns(t));
            if let (Some(size), Some(mtime_ns)) = (size, mtime) {
                map.insert(path, FileState { size, mtime_ns });
            }
        }
    }
    map
}

fn local_state(path: &Path) -> Result<Option<FileState>, String> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {
            let mtime_ns = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            Ok(Some(FileState { size: m.len(), mtime_ns }))
        }
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
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

/// The rclone filter file for a folder: the ignore list, then the files
/// that are being left out (because they could not sync, or because the user
/// chose to ignore them).
pub fn filters_text(s: &Settings, pair: &Pair) -> String {
    let mut out = String::from("# Written by SyncHopper. Change the ignore list in Advanced settings.\n");
    for p in &s.ignore_patterns {
        out.push_str(&format!("- {p}\n"));
    }
    let mut paths: Vec<&str> = pair.skipped.iter().chain(pair.ignored.iter()).map(|k| k.path.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        out.push_str(&format!("- /{}\n", escape_glob(path)));
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

/// How a left-out file is shown, worked out from how long it has failed.
pub fn file_view(k: &Skipped) -> FileView {
    let now = now_secs();
    let age = now.saturating_sub(k.first_failed);
    let permanent = k.kind == "permanent";
    FileView {
        path: k.path.clone(),
        reason: k.reason.clone(),
        detail: k.detail.clone(),
        kind: k.kind.clone(),
        flagged: permanent || age >= FLAG_AFTER,
        can_ignore: permanent || age >= IGNORE_AFTER,
        next_retry: k.next_retry,
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
                    files: p.skipped.iter().map(file_view).collect(),
                    ignored: p.ignored.iter().map(|k| k.path.clone()).collect(),
                    waiting: rt.and_then(|r| r.waiting.clone()),
                    retry_at: rt.and_then(|r| r.retry_at),
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
        let flagged = self.core.settings().pairs.iter().any(|p| p.skipped.iter().any(|k| file_view(k).flagged));
        let inner = self.inner.lock().unwrap();
        flagged || inner.rt.values().any(|r| matches!(r.phase, Phase::Error | Phase::Approval))
    }

    /// True while any folder is waiting for Koofr to come back.
    pub fn waiting_for_koofr(&self) -> bool {
        self.inner.lock().unwrap().rt.values().any(|r| r.phase == Phase::Waiting)
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
            rt.retry_all = true;
        });
        self.enqueue(Job { pair_id: id.to_string(), resync: true, manual: true, create_remote: true, ..Default::default() });
    }

    /// Tries every left-out file again now, including ones that would not
    /// otherwise be retried.
    pub fn retry_skipped(&self, id: &str) {
        self.with_rt(id, |rt| rt.retry_all = true);
        self.enqueue(Job { pair_id: id.to_string(), manual: true, ..Default::default() });
    }

    /// Stops syncing one left-out file until the user says otherwise.
    pub fn ignore_file(&self, id: &str, path: &str) -> Result<(), String> {
        let mut found = false;
        self.core.update_pair(id, |p| {
            if let Some(i) = p.skipped.iter().position(|k| k.path == path) {
                let k = p.skipped.remove(i);
                if !p.ignored.iter().any(|x| x.path == k.path) {
                    p.ignored.push(k);
                }
                found = true;
            }
        });
        if found {
            self.core.log(&format!("Ignoring {path} at the user's request"));
            Ok(())
        } else {
            Err("That file is no longer in the list.".into())
        }
    }

    /// Starts syncing an ignored file again. It is handled like a left-out
    /// file that is due for a retry, so changes made meanwhile are carried
    /// across correctly.
    pub fn unignore_file(&self, id: &str, path: &str) -> Result<(), String> {
        let mut found = false;
        self.core.update_pair(id, |p| {
            if let Some(i) = p.ignored.iter().position(|k| k.path == path) {
                let mut k = p.ignored.remove(i);
                k.next_retry = 0;
                k.tries = 0;
                k.first_failed = now_secs();
                k.notified = false;
                if k.kind == "permanent" {
                    k.kind = "unknown".into();
                }
                if !p.skipped.iter().any(|x| x.path == k.path) {
                    p.skipped.push(k);
                }
                found = true;
            }
        });
        if !found {
            return Err("That file is no longer ignored.".into());
        }
        self.core.log(&format!("No longer ignoring {path}"));
        self.enqueue(Job { pair_id: id.to_string(), manual: true, ..Default::default() });
        Ok(())
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
                Phase::Waiting => match rt.fails {
                    0 | 1 => 120,
                    2 => 300,
                    3 => 600,
                    _ => 900,
                }
                .min(interval_min as u64 * 60),
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
            rt.retry_at = (rt.phase == Phase::Waiting).then(|| now_secs() + wait.max(5));
            if rt.phase != Phase::Waiting {
                rt.waiting = None;
            }
        }
        self.core.host.state_changed();
    }

    /// Koofr is down or out of reach. Nothing is blamed on any file; the
    /// folder simply tries again later.
    fn wait_outage(&self, pair: &Pair, interval_min: u32, kind: &str, raw: String, details: Vec<String>) {
        let msg = if kind == "network" {
            "Can't connect to Koofr right now."
        } else {
            "Koofr is having problems right now."
        };
        self.core.log(&format!("[{}] Koofr unavailable: {}", pair.local, raw));
        let mut fails = 0;
        self.finish(&pair.id, interval_min, |rt| {
            rt.phase = Phase::Waiting;
            rt.fails += 1;
            fails = rt.fails;
            rt.waiting = Some(msg.to_string());
            rt.error = None;
            rt.details = details;
        });
        let first_notice = {
            let mut inner = self.inner.lock().unwrap();
            let due = fails >= 3 && !inner.outage_notified;
            if due {
                inner.outage_notified = true;
            }
            due
        };
        if first_notice {
            self.core.notify(Notice::Problem, msg, "SyncHopper will keep trying and catch up once Koofr is back.");
        }
    }

    fn fail(&self, pair: &Pair, interval_min: u32, message: String, details: Vec<String>) {
        let friendly = friendly_error(&message);
        self.core.log(&format!("[{}] FAILED: {}", pair.local, message));
        let mut first = false;
        self.finish(&pair.id, interval_min, |rt| {
            rt.phase = Phase::Error;
            rt.fails += 1;
            first = rt.fails == 1;
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

    /// Leaves the files that failed out of syncing, so the rest of the folder
    /// can finish, and remembers how both sides last agreed on each file.
    /// Returns true when the list grew.
    fn update_skips(&self, pair: &Pair, files: &[FailedFile], work: &Path, rebuild: bool, recent: &[Skipped]) -> bool {
        let now = now_secs();
        let base_pc = read_listing(work, ".path1.lst");
        let base_koofr = read_listing(work, ".path2.lst");
        let mut add: Vec<Skipped> = Vec::new();
        for f in files {
            let path = f.path.replace('\\', "/");
            if pair.skipped.iter().chain(pair.ignored.iter()).any(|k| k.path == path) || add.iter().any(|k| k.path == path) {
                continue;
            }
            let kind = issue_kind(&f.msg);
            // A file that was just put back for a retry and failed again keeps
            // its history, so long-running trouble is still noticed.
            if let Some(prev) = recent.iter().find(|k| k.path == path) {
                let tries = prev.tries.saturating_add(1);
                add.push(Skipped {
                    reason: file_reason(kind, &f.object_type, &f.msg),
                    detail: f.msg.clone(),
                    kind: kind.to_string(),
                    next_retry: now.saturating_add(retry_delay(kind, tries)),
                    tries,
                    ..prev.clone()
                });
                continue;
            }
            add.push(Skipped {
                reason: file_reason(kind, &f.object_type, &f.msg),
                detail: f.msg.clone(),
                kind: kind.to_string(),
                first_failed: now,
                next_retry: now.saturating_add(retry_delay(kind, 1)),
                tries: 1,
                base_pc: base_pc.get(&path).copied(),
                base_koofr: base_koofr.get(&path).copied(),
                notified: false,
                path,
            });
        }
        if add.is_empty() {
            return false;
        }
        for k in &add {
            self.core.log(&format!("[{}] Leaving out {} for now ({}): {}", pair.local, k.path, k.kind, k.detail));
        }
        self.core.update_pair(&pair.id, |p| {
            p.skipped.extend(add);
            if rebuild {
                p.needs_resync = true;
            }
        });
        true
    }

    /// The full Koofr path of a file inside a synced folder. Unlike
    /// `Core::remote`, the file name is kept exactly as it is.
    fn remote_file(&self, pair: &Pair, rel: &str) -> String {
        let base = self.core.remote(&pair.remote);
        if base.ends_with(':') || base.ends_with('/') {
            format!("{base}{rel}")
        } else {
            format!("{base}/{rel}")
        }
    }

    fn local_file(pair: &Pair, rel: &str) -> PathBuf {
        Path::new(&pair.local).join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
    }

    fn engine_flags(s: &Settings) -> Vec<String> {
        let mut a: Vec<String> = vec!["--retries".into(), "2".into(), "--use-json-log".into(), "--log-level".into(), "INFO".into(), "--color".into(), "NEVER".into()];
        if !s.use_http2 {
            a.push("--disable-http2".into());
        }
        if let Some(limit) = s.bwlimit() {
            a.push("--bwlimit".into());
            a.push(limit);
        }
        a
    }

    async fn remote_state(&self, rc: &Rclone, s: &Settings, pair: &Pair, rel: &str) -> Result<Option<FileState>, String> {
        let target = self.remote_file(pair, rel);
        let mut args: Vec<&str> = vec!["lsjson", "--stat", "--no-mimetype", &target];
        if !s.use_http2 {
            args.push("--disable-http2");
        }
        match rc.capture(&args, Duration::from_secs(120)).await {
            Ok(out) => {
                let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
                if v.get("IsDir").and_then(|d| d.as_bool()).unwrap_or(false) {
                    return Ok(None);
                }
                let size = v.get("Size").and_then(|x| x.as_u64()).unwrap_or(0);
                let mtime_ns = v.get("ModTime").and_then(|x| x.as_str()).and_then(parse_time_ns).unwrap_or(0);
                Ok(Some(FileState { size, mtime_ns }))
            }
            Err(e) if e.to_lowercase().contains("not found") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn run_quiet(rc: &Rclone, args: Vec<String>, cancel: &Cancel) -> Result<(), String> {
        let res = rc.run_logged(&args, cancel, |_| {}).await;
        if res.cancelled {
            return Err("cancelled".into());
        }
        if res.exit == 0 {
            return Ok(());
        }
        Err(res
            .spawn_error
            .clone()
            .or_else(|| res.errors.iter().rev().find(|l| !l.msg.starts_with("Attempt ")).map(|l| l.msg.clone()))
            .or_else(|| res.errors.last().map(|l| l.msg.clone()))
            .unwrap_or_else(|| format!("The sync engine stopped (code {}).", res.exit)))
    }

    /// Finishes the job for one left-out file, the way a normal sync would
    /// have: whichever side changed since both last agreed wins, and a delete
    /// on one side is passed on if the other side was left alone. Files that
    /// were never synced, or that changed on both sides, are left for the
    /// next sync to handle with the usual rules.
    async fn resolve(&self, rc: &Rclone, s: &Settings, pair: &Pair, k: &Skipped, cancel: &Cancel) -> Result<(), String> {
        if k.base_pc.is_none() && k.base_koofr.is_none() {
            return Ok(());
        }
        let local = Self::local_file(pair, &k.path);
        let pc = local_state(&local)?;
        let koofr = self.remote_state(rc, s, pair, &k.path).await?;
        let pc_changed = !same_state(&pc, &k.base_pc);
        let koofr_changed = !same_state(&koofr, &k.base_koofr);
        let flags = Self::engine_flags(s);
        let remote = self.remote_file(pair, &k.path);
        let local_text = local.to_string_lossy().to_string();
        match (pc_changed, koofr_changed) {
            (true, false) if pc.is_some() => {
                self.core.log(&format!("[{}] Retry: sending {} to Koofr", pair.local, k.path));
                let mut args = vec!["copyto".to_string(), local_text, remote];
                args.extend(flags);
                Self::run_quiet(rc, args, cancel).await
            }
            (true, false) => {
                if koofr.is_some() {
                    self.core.log(&format!("[{}] Retry: {} was deleted on this PC, deleting it in Koofr", pair.local, k.path));
                    let mut args = vec!["deletefile".to_string(), remote];
                    args.extend(flags);
                    Self::run_quiet(rc, args, cancel).await?;
                }
                Ok(())
            }
            (false, true) if koofr.is_some() => {
                self.core.log(&format!("[{}] Retry: getting {} from Koofr", pair.local, k.path));
                let mut args = vec!["copyto".to_string(), remote, local_text];
                args.extend(flags);
                Self::run_quiet(rc, args, cancel).await
            }
            (false, true) => {
                if pc.is_some() {
                    self.core.log(&format!("[{}] Retry: {} was deleted in Koofr, deleting it on this PC", pair.local, k.path));
                    std::fs::remove_file(&local).map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Tries the left-out files whose turn has come. Returns whether any were
    /// put back into syncing, and the kind of problem if Koofr turned out to
    /// be unavailable.
    async fn retry_due(&self, rc: &Rclone, s: &Settings, pair: &Pair, cancel: &Cancel) -> (Vec<Skipped>, Option<(String, String)>) {
        if pair.skipped.is_empty() {
            self.with_rt(&pair.id, |rt| rt.retry_all = false);
            return (Vec::new(), None);
        }
        let all = self.with_rt(&pair.id, |rt| std::mem::take(&mut rt.retry_all));
        let now = now_secs();
        let mut cleared: Vec<String> = Vec::new();
        let mut updated: Vec<Skipped> = Vec::new();
        let mut outage: Option<(String, String)> = None;
        for k in &pair.skipped {
            if outage.is_some() || cancel.is_cancelled() {
                break;
            }
            if !all && (k.kind == "permanent" || now < k.next_retry) {
                continue;
            }
            // A file that is still locked on this PC can't sync yet, so there
            // is no point sending anything.
            let local = Self::local_file(pair, &k.path);
            let still_locked = local.is_file() && std::fs::File::open(&local).is_err();
            let result = if still_locked {
                Err("The file is still open in another program (being used by another process).".to_string())
            } else {
                self.resolve(rc, s, pair, k, cancel).await
            };
            match result {
                Ok(()) => cleared.push(k.path.clone()),
                Err(e) if e == "cancelled" => break,
                Err(e) => {
                    let mut kind = issue_kind(&e);
                    if outage_kind(kind) && !still_locked {
                        outage = Some((kind.to_string(), e.clone()));
                    }
                    if matches!(kind, "auth" | "quota" | "disk") {
                        kind = "unknown";
                    }
                    let mut next = k.clone();
                    next.tries = next.tries.saturating_add(1);
                    next.next_retry = now.saturating_add(retry_delay(kind, next.tries));
                    if !still_locked {
                        next.kind = kind.to_string();
                        next.reason = file_reason(kind, "", &e);
                        next.detail = e;
                    }
                    updated.push(next);
                }
            }
        }
        if cleared.is_empty() && updated.is_empty() {
            return (Vec::new(), outage);
        }
        for path in &cleared {
            self.core.log(&format!("[{}] {} can sync again", pair.local, path));
        }
        let put_back: Vec<Skipped> = pair.skipped.iter().filter(|k| cleared.contains(&k.path)).cloned().collect();
        self.core.update_pair(&pair.id, |p| {
            p.skipped.retain(|k| !cleared.contains(&k.path));
            for u in updated {
                if let Some(k) = p.skipped.iter_mut().find(|k| k.path == u.path) {
                    *k = u;
                }
            }
        });
        (put_back, outage)
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

        // Files whose retry time has come are dealt with first, one by one,
        // so the folder sync itself never has to start over for them.
        let (put_back, outage) = self.retry_due(&rc, &s, &first, &cancel).await;
        if let Some((kind, raw)) = outage {
            return self.wait_outage(&first, interval, &kind, raw.clone(), vec![raw]);
        }
        if cancel.is_cancelled() {
            return self.finish(&first.id, interval, |rt| rt.phase = Phase::Idle);
        }

        let mut want_resync = job.resync;
        let force = job.force;
        // Set when the list of left-out files changed in a way rclone can
        // take without a full rebuild of the sync record.
        let mut soft_filters = !put_back.is_empty() && listing_ready(&work);
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
                    let kind = issue_kind(&e);
                    if outage_kind(kind) {
                        return self.wait_outage(&pair, interval, kind, e.clone(), vec![e]);
                    }
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
                    });
                    self.inner.lock().unwrap().outage_notified = false;
                    self.core.log(&format!("[{}] Sync finished. {}", pair.local, summary));
                    let name = file_name(&pair.local);
                    self.notify_flagged(&pair.id, &name);
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
                    // Problems with the account, Koofr's storage or this PC's
                    // disk are not about any one file.
                    let folder_problem = std::iter::once(message.as_str())
                        .chain(files.iter().map(|f| f.msg.as_str()))
                        .find(|m| matches!(issue_kind(m), "auth" | "quota" | "disk"))
                        .map(|m| m.to_string());
                    if let Some(m) = folder_problem {
                        return self.fail(&pair, interval, m, details);
                    }
                    // Koofr itself is down: nothing could be read or sent, so
                    // no file is to blame.
                    let transferred = res.stats.as_ref().map(|st| st.transfers).unwrap_or(0);
                    // If only the same few files keep failing for about an
                    // hour, Koofr itself is clearly up, so treat them as
                    // problem files instead of waiting forever.
                    let long_wait = self.with_rt(&pair.id, |rt| rt.waiting.is_some() && rt.fails >= 6);
                    let outage = if files.is_empty() {
                        Some(issue_kind(&message)).filter(|k| outage_kind(k))
                    } else if !long_wait && files.len() >= 3 && transferred == 0 && files.iter().all(|f| outage_kind(issue_kind(&f.msg))) {
                        Some(issue_kind(&files[0].msg))
                    } else {
                        None
                    };
                    if let Some(kind) = outage {
                        return self.wait_outage(&pair, interval, kind, message, details);
                    }
                    let soft_ok = !resync && (restored || (!must_resync && listing_ready(&work)));
                    if self.update_skips(&pair, &files, &work, !soft_ok, &put_back) && pass < 4 && !cancel.is_cancelled() {
                        if soft_ok {
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

    /// Shows one notification when files in a folder start needing
    /// attention, instead of one per file or per sync.
    fn notify_flagged(&self, id: &str, name: &str) {
        let Some(pair) = self.core.pair(id) else { return };
        let fresh: Vec<String> = pair
            .skipped
            .iter()
            .filter(|k| !k.notified && file_view(k).flagged)
            .map(|k| k.path.clone())
            .collect();
        if fresh.is_empty() {
            return;
        }
        self.core.update_pair(id, |p| {
            for k in p.skipped.iter_mut() {
                if fresh.contains(&k.path) {
                    k.notified = true;
                }
            }
        });
        let title = if fresh.len() == 1 {
            format!("{name}: 1 file needs attention")
        } else {
            format!("{name}: {} files need attention", fresh.len())
        };
        self.core.notify(Notice::Problem, &title, "Open SyncHopper to see which ones and why.");
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
                assert_eq!(files[0].path, "bad.txt");
                assert_eq!(files[0].object_type, "*local.Object");
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
        assert_eq!(issue_kind("open x: The process cannot access the file because it is being used by another process."), "busy");
        assert_eq!(issue_kind("Failed to copy: failed to open source object: Invalid response status! Got 500, expected [200 206]; headers: map[]"), "server");
        assert_eq!(issue_kind("Get \"https:/api/v2/mounts\": dial tcp: lookup app.koofr.net: no such host"), "network");
        assert_eq!(issue_kind("The filename or extension is too long."), "permanent");
        assert_eq!(issue_kind("failed to open source object: open /a/b: permission denied"), "denied");
        assert_eq!(issue_kind("unexpected status code 401"), "auth");
        assert_eq!(issue_kind("insufficient storage (507)"), "quota");
        assert_eq!(issue_kind("Failed to copy IMG_15001.jpg: odd"), "unknown");
        assert_eq!(retry_delay("server", 1), 300);
        assert_eq!(retry_delay("server", 9), 3600);
        assert!(retry_delay("permanent", 1) > 10 * 365 * 86_400);
        assert!(file_reason("server", "*koofr.Object", "x").starts_with("Koofr couldn't send"));
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
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join("a..b.path1.lst"),
            "# bisync listing v1 from x\n-        5 - - 2026-11-13T15:10:34.650367249+0000 \"sub dir/we\\\"ird.txt\"\n",
        )
        .unwrap();
        std::fs::write(work.join("a..b.path2.lst"), "# bisync listing v1 from x\n-        7 - - 2026-11-13T15:10:35.000000000+0000 \"sub dir/we\\\"ird.txt\"\n").unwrap();
        let core = Core::new(dir.path().join("data"), "koofr:".into(), true, Arc::new(NoHost));
        let engine = Engine::new(core.clone());
        let pair = Pair { id: "p1".into(), local: dir.path().to_string_lossy().to_string(), remote: "Docs".into(), ..Default::default() };
        core.update(|s| s.pairs.push(pair.clone()));
        let get = || core.pair("p1").unwrap();

        let failed = vec![
            FailedFile { path: "sub dir/we\"ird.txt".into(), msg: "Failed to copy: failed to open source object: Invalid response status! Got 500".into(), object_type: "*koofr.Object".into() },
            FailedFile { path: "new.txt".into(), msg: "open x: being used by another process".into(), object_type: "*local.Object".into() },
        ];
        assert!(engine.update_skips(&get(), &failed, &work, false, &[]));
        let p = get();
        assert!(!p.needs_resync, "a soft update must not force a rebuild");
        let a = p.skipped.iter().find(|k| k.path == "sub dir/we\"ird.txt").unwrap();
        assert_eq!(a.kind, "server");
        assert_eq!(a.base_pc, Some(FileState { size: 5, mtime_ns: 1_794_582_634_650_367_249 }));
        assert_eq!(a.base_koofr.map(|b| b.size), Some(7));
        assert!(a.reason.starts_with("Koofr couldn't send"));
        let b = p.skipped.iter().find(|k| k.path == "new.txt").unwrap();
        assert_eq!(b.kind, "busy");
        assert!(b.base_pc.is_none() && b.base_koofr.is_none(), "never synced, so no shared state");
        // The same failure again does not add a second entry.
        assert!(!engine.update_skips(&get(), &failed, &work, true, &[]));

        // Freshly failed files are retried quietly; old or hopeless ones are flagged.
        let v = file_view(a);
        assert!(!v.flagged && !v.can_ignore);
        let mut old = a.clone();
        old.first_failed = now_secs() - FLAG_AFTER - 1;
        assert!(file_view(&old).flagged && !file_view(&old).can_ignore);
        old.first_failed = now_secs() - IGNORE_AFTER - 1;
        assert!(file_view(&old).can_ignore);
        let mut hopeless = b.clone();
        hopeless.kind = "permanent".into();
        assert!(file_view(&hopeless).flagged && file_view(&hopeless).can_ignore);

        // Ignoring moves the entry without changing the filter file; un-ignoring
        // brings it back as due for a retry.
        let before = filters_text(&core.settings(), &get());
        engine.ignore_file("p1", "new.txt").unwrap();
        assert_eq!(get().ignored.len(), 1);
        assert_eq!(filters_text(&core.settings(), &get()), before);
        assert!(engine.ignore_file("p1", "nope.txt").is_err());
    }

    #[test]
    fn listings_are_read_correctly() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("x..y.path1.lst"),
            "# bisync listing v1 from 2026-11-13T15:10:38.619499381+0000\n-        5 - - 2026-11-13T15:10:34.650367249+0000 \"f1.txt\"\n-        2 - - 2026-11-13T15:10:34.650367249+0000 \"sub dir/we\\\"ird [x].txt\"\n",
        )
        .unwrap();
        let m = read_listing(dir.path(), ".path1.lst");
        assert_eq!(m.len(), 2);
        assert_eq!(m["f1.txt"].size, 5);
        assert!(m.contains_key("sub dir/we\"ird [x].txt"));
        assert!(read_listing(dir.path(), ".path2.lst").is_empty());
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
