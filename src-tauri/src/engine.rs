//! The sync engine: a queue of folder syncs run one at a time through
//! rclone's two-way sync, with a timer, a file watcher, and recovery from
//! the ways a sync can fail.

use crate::config::{same_state, FileState, Pair, Settings, Skipped};
use crate::core::{Core, Notice};
use crate::platform;
use crate::rclone::{
    error_words, friendly_error, friendly_known, is_blocked_message, is_network_error, reports_status, LogLine, Rclone, RunResult, Stats,
};
use crate::util::{
    clean_remote, file_name, glob_match, go_unquote, koofr_refuses_chars, new_id, norm_local, now_secs, parse_time_ns, paths_overlap,
    system_folder_reason, Cancel,
};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// How long a folder must be quiet before local changes are synced.
const QUIET: Duration = Duration::from_secs(4);
/// Shortest pause between two syncs of one folder that were set off by
/// local changes, so a busy folder does not hammer Koofr.
const BREATHER: Duration = Duration::from_secs(15);
/// How long a synced PC folder may be missing before it counts as a problem.
/// A folder that was just renamed disappears for a moment before SyncHopper
/// catches up with the new name.
pub const MISSING_GRACE: Duration = Duration::from_secs(90);
/// How often the battery and metered-connection checks run.
const POWER_CHECK: Duration = Duration::from_secs(120);
/// After the first failed connection to Koofr in a while, the next try comes
/// this soon, and nothing changes on screen in between. A laptop that has
/// just woken up is usually back online within it.
pub const QUIET_RETRY: Duration = Duration::from_secs(20);
/// A wait for Koofr whose retry time passed this long ago without a retry
/// (the PC was asleep, say) is over: the next failure starts afresh.
const KOOFR_WAIT_STALE: Duration = Duration::from_secs(600);
/// The headline for a folder error SyncHopper does not recognise.
pub const UNEXPECTED: &str = "Something unexpected stopped this folder from syncing.";
/// The reason shown for a file whose error SyncHopper does not recognise.
/// The engine's own words are kept under the technical details.
pub const FILE_UNEXPECTED: &str = "Something unexpected stopped this file from syncing.";
/// rclone stops a sync when every file looks changed (meant for clock
/// changes). Up to this many changed files are synced anyway without asking,
/// since a person could easily have edited them all.
pub const ALL_CHANGED_AUTO: u64 = 20;
/// The longest path Koofr accepts, in characters.
pub const KOOFR_MAX_PATH: usize = 1023;
/// At most this many names Koofr refuses are listed per folder. Any beyond
/// it are still caught when they fail.
const KOOFR_NAME_SCAN_MAX: usize = 1000;
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

/// A sync that stopped to wait for the user: because it would delete a
/// large share of a folder, or because every file looked changed.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Approval {
    /// Files deleted, or with `changed`, files changed.
    pub deleted: u64,
    pub total: u64,
    /// Where it happened: "pc", "koofr", or "both" (only with `changed`).
    pub side: String,
    /// True when that side is completely empty. `deleted` then counts the
    /// files still on the other side.
    pub everything: bool,
    /// Every file looked changed since the last sync, which rclone stops for
    /// (meant for clock changes). `deleted` then counts the changed files,
    /// or is 0 when rclone did not say.
    pub changed: bool,
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
    /// The user approved syncing a folder where every file looked changed.
    /// The run still starts normally, so rclone's large-delete check runs
    /// first.
    pub allow_all_changed: bool,
    /// Rebuild the sync record. Copies both ways and never deletes.
    pub resync: bool,
    /// Started by the user, so automatic pauses do not apply.
    pub manual: bool,
    /// Create the Koofr folder if it is missing.
    pub create_remote: bool,
    /// When it was queued. Set by `enqueue`.
    pub queued_at: Option<Instant>,
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
    /// What the folder is waiting for: "koofr", "throttled", "cert" or
    /// "drive".
    waiting_kind: Option<String>,
    retry_at: Option<u64>,
    summary: String,
    runs: u64,
    last_finished: Option<Instant>,
    missing_since: Option<Instant>,
    /// The sync record was rebuilt because rclone found the two sides out
    /// of step, and no ordinary sync has finished since. If it happens again
    /// before one does, the folder stops instead of rebuilding over and over
    /// (a rebuild can bring back files that were deleted).
    rebuilt_out_of_sync: bool,
    /// What the folder showed before it was queued, to go back to if the
    /// job is set aside while waiting for Koofr.
    queued_from: Phase,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<Job>,
    rt: HashMap<String, PairRt>,
    current: Option<(String, Cancel)>,
    paused_all: bool,
    auto_pause: Option<String>,
    auto_checked: Option<Instant>,
    /// Waits already notified about ("koofr", "throttled", "cert", or
    /// "drive:<folder id>"), so each is told once until things work again.
    notified: HashSet<String>,
    /// Koofr can't be reached (or is turning requests away). One folder
    /// finding that out is enough: the others wait with it instead of each
    /// trying in turn.
    koofr_wait: Option<KoofrWait>,
    /// Jobs set aside while waiting for Koofr, queued again once it's time
    /// to try again or Koofr has answered.
    held: Vec<Job>,
}

/// A wait for Koofr shared by every folder.
struct KoofrWait {
    /// Only a fresh "Sync now" or "Try now" runs before this.
    until: Instant,
    /// When the latest try failed.
    since: Instant,
    /// Unix time of `until`, for the screens.
    retry_at: u64,
    /// Failed tries in a row, the quiet first one included.
    tries: u32,
    /// How many of them were shown as "Waiting for Koofr".
    shown: u32,
    /// While shown: the folder text, the kind of wait and the details.
    view: Option<(String, String, Vec<String>)>,
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
    /// SyncHopper tries it again by itself. False for files that can never
    /// sync as they are, until the user does something.
    pub auto_retry: bool,
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
    pub waiting_kind: Option<String>,
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
    /// rclone stopped because every file looked changed on a side: "pc",
    /// "koofr" or "both".
    AllChanged(String),
    /// rclone's check after a sync found the two sides' records out of step.
    OutOfSync { message: String, details: Vec<String> },
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
    Some(Approval { deleted, total, side: side.into(), everything: false, changed: false })
}

fn is_boilerplate(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.starts_with("bisync aborted")
        || l.starts_with("failed to bisync")
        || l.contains("not deleting files as there were io errors")
        || l.contains("not deleting directories as there were io errors")
        || l.contains("config file") && l.contains("not found")
}

/// Reads rclone's change summary for one side, for example
/// "Path1:    4 changes:    1 new,    3 modified,    0 deleted", as the side
/// ("pc" or "koofr") and the number of files modified.
pub fn parse_change_summary(msg: &str) -> Option<(&'static str, u64)> {
    let side = if msg.starts_with("Path1:") {
        "pc"
    } else if msg.starts_with("Path2:") {
        "koofr"
    } else {
        return None;
    };
    if !msg.contains(" change") {
        return None;
    }
    let part = msg.split(',').find(|p| p.trim().ends_with("modified"))?;
    let n = part.split_whitespace().rev().nth(1)?.parse().ok()?;
    Some((side, n))
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
    // "Safety abort: all files were changed on Path1 ...". Checked after the
    // large-delete check, the same order rclone checks them in.
    let all_changed: Vec<String> = all
        .iter()
        .map(|l| l.msg.to_lowercase())
        .filter(|m| m.contains("all files were changed"))
        .collect();
    if !all_changed.is_empty() {
        let pc = all_changed.iter().any(|m| m.contains("on path1"));
        let koofr = all_changed.iter().any(|m| m.contains("on path2"));
        let side = match (pc, koofr) {
            (true, true) => "both",
            (false, true) => "koofr",
            _ => "pc",
        };
        return Verdict::AllChanged(side.into());
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
    if files.is_empty() {
        if let Some(l) = find("path1 and path2 are out of sync") {
            // From rclone's check after a sync: "path1 and path2 are out of
            // sync, run --resync to recover". The record can't be trusted,
            // so it is rebuilt rather than put back.
            let message = l.msg.trim_start_matches("Bisync critical error: ").to_string();
            return Verdict::OutOfSync { message, details };
        }
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
    let message = files
        .first()
        .map(|f| f.msg.clone())
        .or_else(|| details.first().map(|m| m.trim_start_matches("Bisync critical error: ").to_string()))
        .unwrap_or_else(|| format!("The sync engine stopped unexpectedly (code {}).", res.exit));
    Verdict::Failed { must_resync, files, message, details }
}

/// What kind of failure a message describes.
///
/// Per-file kinds: "server", "network", "throttled" (Koofr says too many
/// requests), "cert", "busy", "locked" (part of the file locked by another
/// program), "virus", "onedrive" (online-only and OneDrive not running),
/// "cantaccess", "unreadable", "denied", "permanent", "rejected" (Koofr
/// refused it) and "unknown". The engine adds "koofr_name" for names Koofr
/// refuses, since that depends on the path.
///
/// "auth", "quota" and "disk" are problems for the whole folder, "drive" is
/// the drive the folder is on being out of reach, and "blocked" is the
/// system refusing to run the engine.
pub fn issue_kind(msg: &str) -> &'static str {
    // Paths are left out, so a name in the message can't decide what the
    // problem is.
    let l = error_words(msg);
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    let code = |c: &str| reports_status(&l, c);
    if is_blocked_message(msg) {
        // The system won't run the engine. Not about any file or folder.
        "blocked"
    } else if code("429") || has(&["too many requests"]) {
        "throttled"
    } else if code("401") || has(&["unauthorized", "invalid credentials", "authentication failed", "couldn't login"]) {
        "auth"
    } else if code("507") || has(&["quota", "insufficient storage", "storage is full"]) {
        "quota"
    } else if has(&["not enough space", "no space left", "disk full", "there is not enough space"]) {
        "disk"
    } else if has(&[
        "x509:",
        "failed to verify certificate",
        "certificate signed by unknown authority",
        "certificate has expired or is not yet valid",
    ]) {
        "cert"
    } else if has(&["invalid response status! got 5", "internal server error", "bad gateway", "service unavailable", "gateway timeout"])
        || code("500")
        || code("502")
        || code("503")
        || code("504")
    {
        "server"
    } else if is_network_error(msg) {
        "network"
    } else if has(&["being used by another process", "sharing violation", "source file is being updated", "size changed"]) {
        "busy"
    } else if has(&["locked a portion of the file", "user-mapped section open"]) {
        "locked"
    } else if has(&["contains a virus or potentially unwanted software"]) {
        "virus"
    } else if has(&["cloud file provider is not running"]) {
        "onedrive"
    } else if has(&["the file cannot be accessed by the system"]) {
        "cantaccess"
    } else if has(&["media is write protected", "device is not ready", "device is not connected"]) {
        "drive"
    } else if has(&["cyclic redundancy check", "i/o device error", "corrupted and unreadable"]) {
        "unreadable"
    } else if has(&["access is denied", "permission denied"]) {
        "denied"
    } else if has(&["too long", "syntax is incorrect", "invalid argument", "illegal characters", "name is not allowed"]) {
        "permanent"
    } else if code("400") {
        "rejected"
    } else {
        "unknown"
    }
}

/// Kinds where nothing is blamed on any file: Koofr, or the connection to
/// it, is the problem, and the folder waits and tries again by itself.
fn outage_kind(kind: &str) -> bool {
    matches!(kind, "server" | "network" | "throttled" | "cert")
}

/// Kinds that make a folder wait instead of failing.
fn waits(kind: &str) -> bool {
    outage_kind(kind) || kind == "drive"
}

/// Files of these kinds can never sync as they are, so they are not tried
/// again until the user does something (renames the file, presses Try
/// again, and so on).
pub fn never_retried(kind: &str) -> bool {
    matches!(kind, "permanent" | "koofr_name" | "virus")
}

/// Seconds to wait before trying a left-out file again.
fn retry_delay(kind: &str, tries: u32) -> u64 {
    if never_retried(kind) {
        return u64::MAX / 4;
    }
    match kind {
        // Cheap to check: the file is looked at locally before anything is sent.
        "busy" | "denied" => 120,
        // Koofr refused it. Most likely it never will accept it, but a daily
        // look costs nothing in case it was a one-off.
        "rejected" => 24 * 3600,
        "unreadable" => 3600,
        // "locked" goes here too: a file with part of it locked still opens,
        // so the quick local check can't see it and every try is a real one.
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
    let l = error_words(raw);
    match kind {
        "server" if from_koofr => "Koofr couldn't send this file. This is usually a temporary problem on Koofr's side.".into(),
        "server" => "Koofr had a problem with this file. This is usually a temporary problem on Koofr's side.".into(),
        "network" => "The connection to Koofr dropped while this file was syncing.".into(),
        "throttled" => "Koofr said SyncHopper was sending too many requests. If this keeps happening, turn on Limit requests to Koofr in Advanced settings.".into(),
        "busy" => {
            if l.contains("being updated") || l.contains("size changed") {
                "This file was still being written while it synced.".into()
            } else {
                "This file is open in another program.".into()
            }
        }
        "locked" => "This file is open in another program.".into(),
        "virus" => "Windows Security blocked this file because it thinks it contains a virus or unwanted software. Check Protection history in Windows Security.".into(),
        "onedrive" => "This file is online-only in OneDrive, and OneDrive isn't running to download it. Start OneDrive, or set the folder to Always keep on this device.".into(),
        "cantaccess" => {
            // Here the path is the clue: the file is in the OneDrive folder.
            if raw.to_lowercase().contains("onedrive") {
                "Windows couldn't open this file. If it's online-only in OneDrive, start OneDrive, or set the folder to Always keep on this device.".into()
            } else {
                "Windows couldn't open this file.".into()
            }
        }
        "unreadable" => "Windows couldn't read this file. The drive may be failing.".into(),
        "drive" => drive_problem_text(raw).into(),
        // Only reached for a file tried on its own; whole-folder waits use
        // `cert_hint` with their own headline.
        "cert" => format!("SyncHopper couldn't connect securely to Koofr. {}", cert_hint(raw)),
        "denied" => "Windows wouldn't let SyncHopper read or write this file.".into(),
        "permanent" => {
            if l.contains("too long") {
                "This file's name or path is too long for Windows.".into()
            } else {
                "This file's name can't be used on Windows or in Koofr.".into()
            }
        }
        "rejected" => "Koofr refused this file. This usually means its name can't be used there, for example when a folder with the same name already exists in Koofr.".into(),
        _ => friendly_known(raw).unwrap_or_else(|| FILE_UNEXPECTED.to_string()),
    }
}

/// What a folder shows while the drive it's on can't be used.
pub fn drive_problem_text(raw: &str) -> &'static str {
    if error_words(raw).contains("write protected") {
        "The drive this folder is on is write-protected, so SyncHopper can't save files there."
    } else {
        "Windows can't reach the drive this folder is on. It may be unplugged."
    }
}

/// What to check when SyncHopper can't connect securely to Koofr. A
/// certificate that looks expired or not yet valid usually means the PC's
/// clock is wrong; one signed by someone unknown means something on the way
/// is intercepting the connection.
pub fn cert_hint(raw: &str) -> &'static str {
    if error_words(raw).contains("expired or is not yet valid") {
        "Check that this PC's date and time are correct."
    } else {
        "This is usually a public Wi-Fi sign-in page, antivirus that scans secure connections, or a work network."
    }
}

/// The root of the drive or network share a Windows folder is on, such as
/// `E:\` or `\\server\share\`. None for other paths.
pub fn drive_root(local: &str) -> Option<String> {
    let b = local.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        return Some(format!("{}:\\", &local[..1]));
    }
    let rest = local.strip_prefix("\\\\").or_else(|| local.strip_prefix("//"))?;
    // Long-path forms like \\?\C:\ are not shares.
    if rest.starts_with('?') || rest.starts_with('.') {
        return None;
    }
    let mut parts = rest.split(['\\', '/']).filter(|p| !p.is_empty());
    let server = parts.next()?;
    let share = parts.next()?;
    Some(format!("\\\\{server}\\{share}\\"))
}

/// Why Koofr would refuse a path inside a synced folder, if it would:
/// characters Koofr doesn't allow in a name, or a path longer than Koofr
/// accepts. `rel` uses forward slashes; a trailing slash marks a folder.
pub fn koofr_name_problem(remote: &str, rel: &str) -> Option<String> {
    let is_dir = rel.ends_with('/');
    let trimmed = rel.trim_end_matches('/');
    let (parent, name) = match trimmed.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", trimmed),
    };
    if koofr_refuses_chars(name) {
        let what = if is_dir { "folder" } else { "file" };
        return Some(format!("Koofr doesn't allow emoji and some other symbols in {what} names. Rename this {what} to sync it."));
    }
    if koofr_refuses_chars(parent) {
        return Some("Koofr doesn't allow emoji and some other symbols in folder names. Rename the folder it's in to sync it.".into());
    }
    let remote = remote.trim_matches('/');
    let full = if remote.is_empty() { format!("/{trimmed}") } else { format!("/{remote}/{trimmed}") };
    if full.chars().count() > KOOFR_MAX_PATH {
        return Some(format!(
            "This {}'s path is longer than Koofr allows ({KOOFR_MAX_PATH} characters). Shorten the folder or file names to sync it.",
            if is_dir { "folder" } else { "file" }
        ));
    }
    None
}

/// Finds the names in a synced folder that Koofr would refuse, so they can
/// be left out before syncing instead of being sent to Koofr at all. A
/// folder with such a name is listed once, without looking inside it.
/// Returns paths with forward slashes, folders ending in a slash, and the
/// reason for each. Names matching the ignore list are skipped, as they
/// are never synced anyway.
pub fn find_koofr_name_problems(local_root: &Path, remote: &str, ignore: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(local_root.to_path_buf(), String::new())];
    while let Some((dir, rel_dir)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            if out.len() >= KOOFR_NAME_SCAN_MAX {
                return out;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if ignore.iter().any(|g| glob_match(g, &name)) {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            // Links are left to rclone, which doesn't follow them by default.
            if ft.is_symlink() {
                continue;
            }
            let rel = if rel_dir.is_empty() { name.clone() } else { format!("{rel_dir}/{name}") };
            let rel_shown = if ft.is_dir() { format!("{rel}/") } else { rel.clone() };
            if let Some(reason) = koofr_name_problem(remote, &rel_shown) {
                out.push((rel_shown, reason));
            } else if ft.is_dir() {
                stack.push((e.path(), rel));
            }
        }
    }
    out
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
/// chose to ignore them). A left-out folder, stored with a trailing slash,
/// leaves out the folder and everything in it.
pub fn filters_text(s: &Settings, pair: &Pair) -> String {
    let mut out = String::from("# Written by SyncHopper. Change the ignore list in Advanced settings.\n");
    for p in &s.ignore_patterns {
        out.push_str(&format!("- {p}\n"));
    }
    let mut paths: Vec<&str> = pair.skipped.iter().chain(pair.ignored.iter()).map(|k| k.path.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        match path.strip_suffix('/') {
            Some(dir) => out.push_str(&format!("- /{}/**\n", escape_glob(dir))),
            None => out.push_str(&format!("- /{}\n", escape_glob(path))),
        }
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
    let stuck = never_retried(&k.kind);
    FileView {
        path: k.path.clone(),
        reason: k.reason.clone(),
        detail: k.detail.clone(),
        kind: k.kind.clone(),
        flagged: stuck || age >= FLAG_AFTER,
        can_ignore: stuck || age >= IGNORE_AFTER,
        next_retry: k.next_retry,
        auto_retry: !stuck,
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
        self.fill_local_ids();
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
                    waiting_kind: rt.and_then(|r| r.waiting_kind.clone()),
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
        self.inner.lock().unwrap().rt.values().any(|r| r.phase == Phase::Waiting && r.waiting_kind.as_deref() != Some("drive"))
    }

    /// True while any folder is waiting for the drive it is on.
    pub fn waiting_for_drive(&self) -> bool {
        self.inner.lock().unwrap().rt.values().any(|r| r.phase == Phase::Waiting && r.waiting_kind.as_deref() == Some("drive"))
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
                q.allow_all_changed |= job.allow_all_changed;
                q.resync |= job.resync;
                q.manual |= job.manual;
                q.create_remote |= job.create_remote;
                if job.manual {
                    q.queued_at = Some(Instant::now());
                }
            } else {
                let rt = inner.rt.entry(job.pair_id.clone()).or_default();
                // A folder waiting for Koofr keeps saying so until it really
                // tries again, rather than flicking to "Waiting to sync".
                let koofr_wait = rt.phase == Phase::Waiting && rt.waiting_kind.as_deref() != Some("drive");
                if rt.phase != Phase::Approval && !koofr_wait {
                    if rt.phase != Phase::Queued {
                        rt.queued_from = rt.phase;
                    }
                    rt.phase = Phase::Queued;
                }
                let mut job = job;
                job.queued_at = Some(Instant::now());
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
            Some(a) if !a.everything && !a.changed => {
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

    /// Syncs a folder where every file looked changed, after the user said
    /// so. The run starts normally, and is only repeated with rclone's
    /// safety checks off if it stops on the same thing again, so the
    /// large-delete check still runs first.
    pub fn approve_changed(&self, id: &str) {
        let allowed = self.with_rt(id, |rt| match &rt.approval {
            Some(a) if a.changed => {
                rt.approval = None;
                rt.phase = Phase::Idle;
                true
            }
            _ => false,
        });
        if allowed {
            self.enqueue(Job { pair_id: id.to_string(), allow_all_changed: true, manual: true, ..Default::default() });
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
                if never_retried(&k.kind) {
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
        if let Some(why) = system_folder_reason(&local, cfg!(windows), &|name| std::env::var(name).ok()) {
            return Err(why);
        }
        let ln = norm_local(&local);
        let sep = std::path::MAIN_SEPARATOR;
        if paths_overlap(&ln, &norm_local(&self.core.data.to_string_lossy()), sep) {
            return Err("That folder holds SyncHopper's own files. Pick a different one.".into());
        }
        if s.feature_drive && cfg!(windows) && ln.starts_with(&format!("{}:", s.drive_letter.to_lowercase())) {
            return Err("That folder is on the Koofr drive, which is already Koofr. Pick a folder on this PC.".into());
        }
        if koofr_refuses_chars(&remote) {
            return Err("Koofr doesn't allow emoji and some other symbols in folder names. Rename the folder, or pick a different Koofr folder name.".into());
        }
        if remote.chars().count() + 1 > KOOFR_MAX_PATH {
            return Err(format!("That Koofr folder's path is longer than Koofr allows ({KOOFR_MAX_PATH} characters)."));
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
        let local_id = platform::folder_id(lp).unwrap_or_default();
        let pair = Pair { id: new_id("p"), local, remote, needs_resync: true, local_id, ..Default::default() };
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

    /// For tests: acts as if the folder went missing longer ago than the
    /// grace period.
    #[doc(hidden)]
    pub fn expire_missing_grace(&self, id: &str) {
        self.with_rt(id, |rt| {
            rt.missing_since = Instant::now().checked_sub(MISSING_GRACE + Duration::from_secs(1));
        });
    }

    /// Notes the identity of every synced PC folder that doesn't have one
    /// yet, for example folders added before SyncHopper kept track of it.
    pub fn fill_local_ids(&self) {
        let missing: Vec<(String, String)> = self
            .core
            .settings()
            .pairs
            .iter()
            .filter(|p| p.local_id.is_empty())
            .filter_map(|p| platform::folder_id(Path::new(&p.local)).map(|id| (p.id.clone(), id)))
            .collect();
        if missing.is_empty() {
            return;
        }
        self.core.update(|s| {
            for p in s.pairs.iter_mut() {
                if let Some((_, id)) = missing.iter().find(|(pid, _)| pid == &p.id) {
                    p.local_id = id.clone();
                }
            }
        });
    }

    /// Points a synced folder at its new PC location after it was renamed,
    /// and renames the Koofr folder too when `new_remote` is given. If Koofr
    /// can't be renamed, the folder keeps syncing under its old Koofr name
    /// and the error is returned so it can be shown.
    pub async fn rename_pair(self: &Arc<Self>, id: &str, new_local: &str, new_remote: Option<&str>) -> Result<(), String> {
        let Some(pair) = self.core.pair(id) else { return Err("That folder is no longer synced.".into()) };
        let was_paused = pair.paused;
        // Hold the folder still while it is being moved around.
        self.core.update_pair(id, |p| p.paused = true);
        self.cancel_pair(id);
        for _ in 0..300 {
            let busy = self.inner.lock().unwrap().current.as_ref().is_some_and(|(cur, _)| cur == id);
            if !busy {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let mut result = Ok(());
        let mut remote = pair.remote.clone();
        if let Some(target) = new_remote.map(clean_remote).filter(|t| !t.is_empty() && *t != pair.remote) {
            match self.core.rclone() {
                Ok(rc) => {
                    let from = self.core.remote(&pair.remote);
                    let to = self.core.remote(&target);
                    match rc.capture(&["moveto", &from, &to], Duration::from_secs(600)).await.map(|_| ()) {
                        Ok(()) => {
                            self.core.log(&format!("Renamed {from} to {to} in Koofr to match the folder on this PC"));
                            remote = target;
                        }
                        Err(e) => {
                            self.core.log(&format!("Could not rename {from} to {to} in Koofr: {e}"));
                            result = Err(e);
                        }
                    }
                }
                Err(e) => result = Err(e),
            }
        }
        let new_local = new_local.to_string();
        let local_id = platform::folder_id(Path::new(&new_local)).unwrap_or(pair.local_id.clone());
        self.core.update_pair(id, |p| {
            p.local = new_local.clone();
            p.remote = remote.clone();
            p.local_id = local_id.clone();
            p.paused = was_paused;
            p.needs_resync = true;
        });
        self.with_rt(id, |rt| {
            rt.missing_since = None;
            rt.error = None;
            rt.details.clear();
            if rt.phase == Phase::Error {
                rt.phase = Phase::Idle;
                rt.fails = 0;
            }
        });
        self.rewatch();
        if !was_paused {
            self.enqueue(Job { pair_id: id.to_string(), manual: true, ..Default::default() });
        }
        self.core.host.state_changed();
        result
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
            self.retry_koofr_if_due();
            let mut due: Vec<String> = Vec::new();
            {
                let mut inner = self.inner.lock().unwrap();
                if inner.paused_all || inner.auto_pause.is_some() {
                    continue;
                }
                // Waiting for Koofr: nothing starts by itself until it's time
                // to try again.
                if inner.koofr_wait.as_ref().is_some_and(|w| Instant::now() < w.until) {
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
            let job = {
                let mut inner = self.inner.lock().unwrap();
                let mut next = None;
                while let Some(job) = inner.queue.pop_front() {
                    // While waiting for Koofr, only a "Sync now" or "Try now"
                    // pressed since the last failed try goes ahead.
                    let waiting = inner.koofr_wait.as_ref().filter(|w| Instant::now() < w.until).map(|w| w.since);
                    match waiting {
                        Some(since) if !(job.manual && job.queued_at.is_some_and(|t| t > since)) => Self::hold_job(&mut inner, job),
                        _ => {
                            next = Some(job);
                            break;
                        }
                    }
                }
                next
            };
            match job {
                Some(job) => self.run_pair(job).await,
                None => break,
            }
        }
        self.core.host.state_changed();
    }

    /// Sets a job aside until it's time to try Koofr again. The folder shows
    /// the shared wait, if there is one on screen.
    fn hold_job(inner: &mut Inner, job: Job) {
        let shown = inner.koofr_wait.as_ref().and_then(|w| w.view.clone().map(|v| (v, w.retry_at, w.until)));
        let rt = inner.rt.entry(job.pair_id.clone()).or_default();
        match shown {
            Some(((text, kind, details), retry_at, until)) if rt.phase != Phase::Approval => {
                rt.phase = Phase::Waiting;
                rt.waiting = Some(text);
                rt.waiting_kind = Some(kind);
                rt.details = details;
                rt.error = None;
                rt.retry_at = Some(retry_at);
                rt.next_due = Some(until);
            }
            _ => {
                if rt.phase == Phase::Queued {
                    rt.phase = rt.queued_from;
                }
            }
        }
        if let Some(h) = inner.held.iter_mut().find(|h| h.pair_id == job.pair_id) {
            h.force |= job.force;
            h.allow_all_changed |= job.allow_all_changed;
            h.resync |= job.resync;
            h.manual |= job.manual;
            h.create_remote |= job.create_remote;
        } else {
            inner.held.push(job);
        }
    }

    /// Once it's time to try Koofr again, puts the jobs that were set aside
    /// back in the queue. The first one to run finds out whether Koofr is
    /// back; if it isn't, the rest are set aside again without trying.
    pub fn retry_koofr_if_due(&self) {
        let jobs = {
            let mut inner = self.inner.lock().unwrap();
            let due = inner.koofr_wait.as_ref().is_some_and(|w| Instant::now() >= w.until);
            if !due {
                return;
            }
            std::mem::take(&mut inner.held)
        };
        for job in jobs {
            self.enqueue(job);
        }
    }

    /// Koofr answered, so the shared wait is over: whatever was set aside
    /// runs now, and folders still showing the wait sync straight away.
    fn koofr_answered(&self) {
        let jobs = {
            let mut inner = self.inner.lock().unwrap();
            if inner.koofr_wait.take().is_none() && inner.held.is_empty() {
                return;
            }
            for rt in inner.rt.values_mut() {
                if rt.phase == Phase::Waiting && rt.waiting_kind.as_deref() != Some("drive") {
                    rt.next_due = None;
                }
            }
            std::mem::take(&mut inner.held)
        };
        for job in jobs {
            self.enqueue(job);
        }
    }

    /// For tests: acts as if the time to try Koofr again came `ago`.
    #[doc(hidden)]
    pub fn expire_koofr_wait(&self, ago: Duration) {
        let mut inner = self.inner.lock().unwrap();
        let past = Instant::now().checked_sub(ago + Duration::from_millis(10)).unwrap_or_else(Instant::now);
        if let Some(w) = inner.koofr_wait.as_mut() {
            w.until = past;
        }
        for rt in inner.rt.values_mut() {
            if rt.next_due.is_some_and(|t| t > past) {
                rt.next_due = Some(past);
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
                rt.waiting_kind = None;
            }
        }
        self.core.host.state_changed();
    }

    /// The folder can't sync for now for a reason that is not about its
    /// files: Koofr is down or out of reach ("server", "network"), Koofr
    /// says it is getting too many requests ("throttled"), a secure
    /// connection to Koofr can't be made ("cert"), or the drive the folder
    /// is on can't be used ("drive"). Nothing is blamed on any file; the
    /// folder tries again by itself, and one notification is shown if it
    /// goes on for a while.
    ///
    /// A wait for Koofr is shared by every folder (see `wait_for_koofr`).
    /// `manual` says the user started this sync.
    fn wait_outage(&self, pair: &Pair, interval_min: u32, kind: &str, raw: String, details: Vec<String>, manual: bool) {
        if kind == "blocked" || is_blocked_message(&raw) {
            return self.engine_blocked(pair, interval_min, &raw);
        }
        let name = file_name(&pair.local);
        let keep_trying = "SyncHopper will keep trying and catch up once Koofr is back.";
        // What the folder shows, which wait it is, and the notification.
        let (waiting_kind, shown, key, title, body): (&str, String, String, String, String) = match kind {
            "network" => {
                let m = "Can't connect to Koofr right now.";
                ("koofr", m.into(), "koofr".into(), m.into(), keep_trying.into())
            }
            "throttled" if self.core.settings().limit_requests => {
                let m = "Koofr is busy right now.";
                ("throttled", m.into(), "throttled".into(), m.into(), keep_trying.into())
            }
            "throttled" => (
                "throttled",
                "Koofr says SyncHopper is sending too many requests.".into(),
                "throttled".into(),
                "Koofr says SyncHopper is sending too many requests".into(),
                "SyncHopper will keep trying. If this keeps happening, turn on Limit requests to Koofr in Advanced settings.".into(),
            ),
            "cert" => (
                "cert",
                format!("SyncHopper couldn't connect securely to Koofr. {}", cert_hint(&raw)),
                "cert".into(),
                "SyncHopper can't connect securely to Koofr".into(),
                format!("{} SyncHopper will keep trying.", cert_hint(&raw)),
            ),
            "drive" => (
                "drive",
                drive_problem_text(&raw).into(),
                format!("drive:{}", pair.id),
                format!("{name} can't sync right now"),
                format!("{} SyncHopper will keep trying.", drive_problem_text(&raw)),
            ),
            _ => {
                let m = "Koofr is having problems right now.";
                ("koofr", m.into(), "koofr".into(), m.into(), keep_trying.into())
            }
        };
        if waiting_kind != "drive" {
            return self.wait_for_koofr(pair, interval_min, kind, &raw, (waiting_kind, shown, key, title, body), details, manual);
        }
        self.core.log(&format!("[{}] Waiting for the drive: {}", pair.local, raw));
        let mut fails = 0;
        self.finish(&pair.id, interval_min, |rt| {
            rt.phase = Phase::Waiting;
            rt.fails += 1;
            fails = rt.fails;
            rt.waiting = Some(shown.clone());
            rt.waiting_kind = Some(waiting_kind.to_string());
            rt.error = None;
            rt.details = details;
        });
        // Brief blips are not worth a notification: only the third wait in
        // a row tells the user, and only once until things work again.
        let first_notice = fails >= 3 && self.inner.lock().unwrap().notified.insert(key);
        if first_notice {
            self.core.notify(Notice::Problem, &title, &body);
        }
    }

    /// Koofr can't be reached, or is turning requests away. Every folder
    /// waits together: the others are set aside instead of each trying in
    /// turn. The first failed connection in a while, unless the user started
    /// the sync, changes nothing on screen and is tried again shortly, since
    /// a PC that has just woken up is usually back online within seconds.
    /// After that every folder shows the wait at once, it's tried again
    /// after 2, 5, 10 and then every 15 minutes, and the third shown wait
    /// brings one notification.
    #[allow(clippy::too_many_arguments)]
    fn wait_for_koofr(
        &self,
        pair: &Pair,
        interval_min: u32,
        kind: &str,
        raw: &str,
        texts: (&str, String, String, String, String),
        details: Vec<String>,
        manual: bool,
    ) {
        let (waiting_kind, shown_text, key, title, body) = texts;
        let now = Instant::now();
        let (quiet, shown, until, retry_at) = {
            let mut inner = self.inner.lock().unwrap();
            // A retry time that passed long ago without a retry (the PC was
            // asleep) ends the old wait: this is a fresh start.
            if inner.koofr_wait.as_ref().is_some_and(|w| now > w.until + KOOFR_WAIT_STALE) {
                inner.koofr_wait = None;
            }
            let w = inner.koofr_wait.get_or_insert(KoofrWait { until: now, since: now, retry_at: 0, tries: 0, shown: 0, view: None });
            w.tries += 1;
            let quiet = kind == "network" && w.tries == 1 && !manual;
            let secs = if quiet {
                QUIET_RETRY.as_secs()
            } else {
                w.shown += 1;
                match w.shown {
                    1 => 120,
                    2 => 300,
                    3 => 600,
                    _ => 900,
                }
            };
            let secs = secs.min(interval_min as u64 * 60).max(5);
            w.since = now;
            w.until = now + Duration::from_secs(secs);
            w.retry_at = now_secs() + secs;
            w.view = (!quiet).then(|| (shown_text.clone(), waiting_kind.to_string(), details.clone()));
            (quiet, w.shown, w.until, w.retry_at)
        };
        if quiet {
            self.core.log(&format!("[{}] Couldn't reach Koofr. Trying again shortly: {}", pair.local, raw));
        } else {
            self.core.log(&format!("[{}] Koofr unavailable: {}", pair.local, raw));
        }
        let synced_before = pair.last_sync > 0;
        self.finish(&pair.id, interval_min, |rt| {
            if quiet {
                // Nothing changes on screen: the folder goes back to how it
                // looked before this try.
                if rt.phase != Phase::Approval {
                    rt.phase = if rt.waiting.is_some() && rt.waiting_kind.as_deref() != Some("drive") {
                        Phase::Waiting
                    } else if rt.error.is_some() {
                        Phase::Error
                    } else if synced_before {
                        Phase::Ok
                    } else {
                        Phase::Idle
                    };
                }
            } else {
                rt.phase = Phase::Waiting;
                rt.fails += 1;
                rt.waiting = Some(shown_text.clone());
                rt.waiting_kind = Some(waiting_kind.to_string());
                rt.error = None;
                rt.details = details.clone();
            }
        });
        {
            // Every folder waits for the same retry time. The others are set
            // aside, and while the wait is shown they show it too.
            let s = self.core.settings();
            let mut inner = self.inner.lock().unwrap();
            let queued: Vec<Job> = inner.queue.drain(..).collect();
            for job in queued {
                Self::hold_job(&mut inner, job);
            }
            for p in s.pairs.iter().filter(|p| !p.paused) {
                let rt = inner.rt.entry(p.id.clone()).or_default();
                let drive_wait = rt.phase == Phase::Waiting && rt.waiting_kind.as_deref() == Some("drive");
                if p.id == pair.id || (!quiet && rt.phase != Phase::Approval && !drive_wait) {
                    rt.next_due = Some(until);
                } else {
                    rt.next_due = Some(rt.next_due.map_or(until, |t| t.max(until)));
                }
                if rt.phase == Phase::Waiting && !drive_wait {
                    rt.retry_at = Some(retry_at);
                }
                if quiet || p.id == pair.id || rt.phase == Phase::Approval || drive_wait {
                    continue;
                }
                rt.phase = Phase::Waiting;
                rt.waiting = Some(shown_text.clone());
                rt.waiting_kind = Some(waiting_kind.to_string());
                rt.details = details.clone();
                rt.error = None;
                rt.retry_at = Some(retry_at);
            }
        }
        self.core.host.state_changed();
        // Brief blips are not worth a notification: only the third shown
        // wait in a row tells the user, and only once until Koofr is back.
        let first_notice = shown >= 3 && self.inner.lock().unwrap().notified.insert(key);
        if first_notice {
            self.core.notify(Notice::Problem, &title, &body);
        }
    }

    /// The system refused to run the sync engine. That is not this folder's
    /// fault: the folder keeps the state it had, and the app reports the
    /// block once for everything (see `Core::engine_blocked`).
    fn engine_blocked(&self, pair: &Pair, interval_min: u32, raw: &str) {
        self.core.log(&format!("[{}] Not synced: the sync engine was blocked", pair.local));
        self.core.engine_blocked(raw);
        let synced_before = pair.last_sync > 0;
        self.finish(&pair.id, interval_min, |rt| {
            if rt.phase != Phase::Approval {
                rt.phase = if rt.error.is_some() {
                    Phase::Error
                } else if synced_before {
                    Phase::Ok
                } else {
                    Phase::Idle
                };
            }
        });
    }

    /// The sync engine runs again after being unavailable, so every folder
    /// is checked soon instead of at its usual time.
    pub fn engine_back(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            for rt in inner.rt.values_mut() {
                rt.next_due = None;
            }
        }
        self.core.host.state_changed();
    }

    /// Stops a folder with one of SyncHopper's own messages.
    fn fail(&self, pair: &Pair, interval_min: u32, message: String, details: Vec<String>) {
        let friendly = friendly_error(&message);
        self.fail_with(pair, interval_min, friendly, &message, details);
    }

    /// Stops a folder with an error from the sync engine. Errors SyncHopper
    /// does not recognise get a plain headline, with the engine's own words
    /// kept under the technical details.
    fn fail_raw(&self, pair: &Pair, interval_min: u32, raw: String, mut details: Vec<String>) {
        let headline = friendly_known(&raw).unwrap_or_else(|| UNEXPECTED.to_string());
        if !details.iter().any(|d| d == &raw) {
            details.insert(0, raw.clone());
        }
        self.fail_with(pair, interval_min, headline, &raw, details);
    }

    fn fail_with(&self, pair: &Pair, interval_min: u32, friendly: String, message: &str, details: Vec<String>) {
        if is_blocked_message(message) || is_blocked_message(&friendly) {
            return self.engine_blocked(pair, interval_min, message);
        }
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
        // rclone compared both sides to get here, so Koofr is answering.
        self.koofr_answered();
        let name = file_name(&pair.local);
        let body = if a.changed {
            "Every file in this folder looks changed since the last sync. SyncHopper is waiting for your OK before syncing them.".to_string()
        } else if a.everything {
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
            // A name Koofr refuses can never sync as it is, whatever the
            // error looked like.
            let (kind, reason) = match koofr_name_problem(&pair.remote, &path) {
                Some(why) => ("koofr_name", why),
                None => {
                    let kind = issue_kind(&f.msg);
                    (kind, self.reason_for(kind, &f.object_type, &f.msg))
                }
            };
            // A file that was just put back for a retry and failed again keeps
            // its history, so long-running trouble is still noticed.
            if let Some(prev) = recent.iter().find(|k| k.path == path) {
                let tries = prev.tries.saturating_add(1);
                add.push(Skipped {
                    reason,
                    detail: f.msg.clone(),
                    kind: kind.to_string(),
                    next_retry: now.saturating_add(retry_delay(kind, tries)),
                    tries,
                    ..prev.clone()
                });
                continue;
            }
            add.push(Skipped {
                reason,
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

    /// The reason shown for a left-out file. A file Koofr turned away for
    /// too many requests only points to Limit requests to Koofr while that
    /// setting is off.
    fn reason_for(&self, kind: &str, object_type: &str, raw: &str) -> String {
        if kind == "throttled" && self.core.settings().limit_requests {
            return "Koofr was too busy to take this file.".into();
        }
        file_reason(kind, object_type, raw)
    }

    /// Leaves out the names in a folder that Koofr would refuse, before
    /// anything is sent, and drops the ones that are gone (renamed, moved or
    /// deleted) so whatever replaced them syncs normally. Returns true when
    /// the list of left-out files changed.
    ///
    /// Entries this check owns have no engine message. One that came from a
    /// failed sync instead keeps its message and is never dropped here: its
    /// path is rclone's, which can differ from the name on disk (rclone
    /// shows a full-width "？" in a Windows name as "?", for example), so not
    /// finding it on disk proves nothing. Dropping it anyway would only have
    /// it fail, and be announced, all over again.
    fn update_koofr_names(&self, pair: &Pair, s: &Settings, recent: &[Skipped]) -> bool {
        let found = find_koofr_name_problems(Path::new(&pair.local), &pair.remote, &s.ignore_patterns);
        // A capped list may be missing some, so nothing is dropped then.
        let complete = found.len() < KOOFR_NAME_SCAN_MAX;
        let now = now_secs();
        let mut changed = false;
        let mut added: Vec<String> = Vec::new();
        let mut dropped: Vec<String> = Vec::new();
        self.core.update_pair(&pair.id, |p| {
            if complete {
                p.skipped.retain(|k| {
                    let keep = k.kind != "koofr_name" || !k.detail.is_empty() || found.iter().any(|(path, _)| path == &k.path);
                    if !keep {
                        dropped.push(k.path.clone());
                    }
                    keep
                });
            }
            for (path, why) in &found {
                if p.ignored.iter().any(|k| &k.path == path) {
                    continue;
                }
                if let Some(k) = p.skipped.iter_mut().find(|k| &k.path == path) {
                    // Found on disk, so from now on this check owns it.
                    if k.kind != "koofr_name" || &k.reason != why || !k.detail.is_empty() {
                        k.kind = "koofr_name".into();
                        k.reason = why.clone();
                        k.detail.clear();
                        k.next_retry = now.saturating_add(retry_delay("koofr_name", k.tries));
                        changed = true;
                    }
                    continue;
                }
                // Put back a moment ago by "Try again": keep its history, so
                // it isn't announced as new.
                let prev = recent.iter().find(|k| &k.path == path);
                p.skipped.push(Skipped {
                    path: path.clone(),
                    reason: why.clone(),
                    detail: String::new(),
                    kind: "koofr_name".into(),
                    first_failed: prev.map(|k| k.first_failed).unwrap_or(now),
                    next_retry: now.saturating_add(retry_delay("koofr_name", 1)),
                    tries: prev.map(|k| k.tries.saturating_add(1)).unwrap_or(1),
                    base_pc: None,
                    base_koofr: None,
                    notified: prev.map(|k| k.notified).unwrap_or(false),
                });
                added.push(path.clone());
            }
        });
        for path in &added {
            self.core.log(&format!("[{}] Leaving out {path}: Koofr doesn't accept its name", pair.local));
        }
        for path in &dropped {
            self.core.log(&format!("[{}] {path} is gone, so it is no longer left out", pair.local));
        }
        changed || !added.is_empty() || !dropped.is_empty()
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
            if !all && (never_retried(&k.kind) || now < k.next_retry) {
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
                    if kind == "blocked" {
                        // Nothing to do with this file: leave it as it was.
                        outage = Some((kind.to_string(), e));
                        continue;
                    }
                    if waits(kind) && !still_locked {
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
                        next.reason = self.reason_for(kind, "", &e);
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
            // The whole drive or network share is gone, not just the folder:
            // wait for it to come back rather than calling the folder missing.
            if let Some(root) = drive_root(&first.local) {
                if !Path::new(&root).exists() {
                    let raw = format!("The drive {root} is not available.");
                    return self.wait_outage(&first, interval, "drive", raw.clone(), vec![raw], job.manual);
                }
            }
            let early = self.with_rt(&first.id, |rt| rt.missing_since.get_or_insert_with(Instant::now).elapsed() < MISSING_GRACE);
            if early {
                // Most likely renamed a moment ago. Look again shortly
                // instead of reporting it as missing straight away.
                self.core.log(&format!("[{}] The folder is not there right now. Checking again shortly.", first.local));
                let synced_before = first.last_sync > 0;
                self.finish(&first.id, interval, |rt| {
                    if rt.phase == Phase::Checking {
                        rt.phase = if synced_before { Phase::Ok } else { Phase::Idle };
                    }
                });
                self.with_rt(&first.id, |rt| rt.next_due = Some(Instant::now() + Duration::from_secs(30)));
                self.core.host.state_changed();
                return;
            }
            let msg = format!("This folder is missing on this PC: {}. Nothing was changed in Koofr.", first.local);
            return self.fail(&first, interval, msg, vec![]);
        }
        self.with_rt(&first.id, |rt| rt.missing_since = None);
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
            return self.wait_outage(&first, interval, &kind, raw.clone(), vec![raw], job.manual);
        }
        if cancel.is_cancelled() {
            return self.finish(&first.id, interval, |rt| rt.phase = Phase::Idle);
        }

        // Names Koofr would refuse are left out before anything is sent.
        let names_changed = self.update_koofr_names(&first, &s, &put_back);

        let mut want_resync = job.resync;
        let mut force = job.force;
        // Set once a run that stopped because every file looked changed is
        // repeated with rclone's safety checks off.
        let mut forced_changed = false;
        // Set when the list of left-out files changed in a way rclone can
        // take without a full rebuild of the sync record: files put back
        // after being synced on their own, files newly left out, and
        // refused names that are gone.
        let mut soft_filters = (!put_back.is_empty() || names_changed) && listing_ready(&work);
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
                        return self.wait_outage(&pair, interval, kind, e.clone(), vec![e], job.manual);
                    }
                    return self.fail_raw(&pair, interval, e.clone(), vec![e]);
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
                if forced_changed {
                    " (every file looked changed, syncing anyway)"
                } else if force {
                    " (large delete allowed)"
                } else {
                    ""
                }
            ));
            let me = self.clone();
            let id = pair.id.clone();
            let label = pair.local.clone();
            let mut found: Vec<String> = Vec::new();
            // Files rclone counted as modified on each side: PC, Koofr.
            let mut modified: [Option<u64>; 2] = [None, None];
            let res: RunResult = rc
                .run_logged(&args, &cancel, |line| {
                    if let Some(st) = &line.stats {
                        me.on_stats(&id, st);
                        return;
                    }
                    if let Some((side, n)) = parse_change_summary(&line.msg) {
                        modified[if side == "pc" { 0 } else { 1 }] = Some(n);
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
            // The checks are only off for the one run straight after a
            // normal run that stopped on every file looking changed. Any
            // further run starts normally again, so rclone's large-delete
            // check runs before anything else is let through.
            if std::mem::take(&mut forced_changed) {
                force = job.force;
            }

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
                        if !resync {
                            rt.rebuilt_out_of_sync = false;
                        }
                        for c in conflicts.drain(..) {
                            if !rt.conflicts.contains(&c) && rt.conflicts.len() < 100 {
                                rt.conflicts.push(c);
                            }
                        }
                    });
                    {
                        // Koofr and this folder's drive work again, so a new
                        // problem with them is worth telling.
                        let mut inner = self.inner.lock().unwrap();
                        for key in ["koofr", "throttled", "cert"] {
                            inner.notified.remove(key);
                        }
                        inner.notified.remove(&format!("drive:{}", pair.id));
                    }
                    self.koofr_answered();
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
                Verdict::AllChanged(side) => {
                    // rclone stops when every file looks changed, which is
                    // meant to catch clock changes. A few files are synced
                    // anyway, since a person could easily have edited them
                    // all; more need the user's OK. rclone's large-delete
                    // check ran before this one, so the repeat skips nothing
                    // the user hasn't been asked about.
                    let count = match side.as_str() {
                        "pc" => modified[0],
                        "koofr" => modified[1],
                        _ => modified[0].max(modified[1]),
                    };
                    let few = count.map(|n| n <= ALL_CHANGED_AUTO).unwrap_or(false);
                    if !force && pass < 4 && (few || job.allow_all_changed) {
                        self.core.log(&format!(
                            "[{}] Every file looked changed ({} files). Syncing them anyway.",
                            pair.local,
                            count.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
                        ));
                        force = true;
                        forced_changed = true;
                        continue;
                    }
                    let n = count.unwrap_or(0);
                    self.core.log(&format!("[{}] Stopped: every file looked changed ({n} files).", pair.local));
                    let a = Approval { deleted: n, total: n, side, everything: false, changed: true };
                    return self.ask_approval(&pair, interval, a);
                }
                Verdict::OutOfSync { message, details } => {
                    let again = self.with_rt(&pair.id, |rt| rt.rebuilt_out_of_sync);
                    if !resync && !again && pass < 4 {
                        // rclone's check after the sync found the two sides'
                        // records out of step. Rebuilding the record fixes
                        // that; it never deletes anything.
                        self.core.log(&format!("[{}] The sync record was out of step. Rebuilding it.", pair.local));
                        self.with_rt(&pair.id, |rt| rt.rebuilt_out_of_sync = true);
                        self.core.update_pair(&pair.id, |p| p.needs_resync = true);
                        want_resync = true;
                        continue;
                    }
                    // It happened again before an ordinary sync could finish.
                    // Rebuilding every time would keep bringing back deleted
                    // files, so the folder stops instead, the same way it
                    // always has for this.
                    let restored = !resync && restore_records(&work);
                    if !restored {
                        self.core.update_pair(&pair.id, |p| p.needs_resync = true);
                    }
                    return self.fail_raw(&pair, interval, message, details);
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
                            let a = Approval { deleted: n, total: n, side, everything: true, changed: false };
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
                    // The system refused to run the engine: nothing happened
                    // to this folder, so nothing about it is changed.
                    if is_blocked_message(&message) {
                        return self.engine_blocked(&pair, interval, &message);
                    }
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
                        return self.fail_raw(&pair, interval, m, details);
                    }
                    // The drive the folder is on can't be used, which is not
                    // about any one file either.
                    let drive_problem = std::iter::once(message.as_str())
                        .chain(files.iter().map(|f| f.msg.as_str()))
                        .find(|m| issue_kind(m) == "drive")
                        .map(|m| m.to_string());
                    if let Some(m) = drive_problem {
                        return self.wait_outage(&pair, interval, "drive", m, details, job.manual);
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
                        return self.wait_outage(&pair, interval, kind, message, details, job.manual);
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
                    return self.fail_raw(&pair, interval, message, details);
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
        assert_eq!(a, Approval { deleted: 100, total: 140, side: "pc".into(), everything: false, changed: false });
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

        // Every file changed, as rclone 1.76 logs it.
        let notice = |m: &str| LogLine { level: "notice".into(), msg: m.into(), ..Default::default() };
        r = RunResult { exit: 1, ..Default::default() };
        r.errors.push(err("Safety abort: all files were changed on Path1 \"/x/a/\". Run with --force if desired.", "", ""));
        r.notices.push(notice("Bisync aborted. Please try again."));
        r.notices.push(notice("Failed to bisync: all files were changed"));
        assert_eq!(classify(&r), Verdict::AllChanged("pc".into()));
        r = RunResult { exit: 1, ..Default::default() };
        r.errors.push(err("Safety abort: all files were changed on Path2 \"koofr:Docs/\". Run with --force if desired.", "", ""));
        assert_eq!(classify(&r), Verdict::AllChanged("koofr".into()));
        r.errors.push(err("Safety abort: all files were changed on Path1 \"/x/a/\". Run with --force if desired.", "", ""));
        assert_eq!(classify(&r), Verdict::AllChanged("both".into()));
        // The large-delete check comes first, as it does in rclone.
        r.errors.insert(0, err("too many deletes (>50%, 2 of 3) on Path1 \"/a\". Run with --force if desired.", "", ""));
        assert!(matches!(classify(&r), Verdict::TooManyDeletes(_)));

        // rclone's check after a sync found the records out of step.
        r = RunResult { exit: 7, ..Default::default() };
        r.errors.push(err("-                   Path1 file not found in Path2               - pics/a.jpg", "", ""));
        r.errors.push(err("Bisync critical error: path1 and path2 are out of sync, run --resync to recover", "", ""));
        r.errors.push(err("Bisync aborted. Must run --resync to recover.", "", ""));
        match classify(&r) {
            Verdict::OutOfSync { message, details } => {
                assert_eq!(message, "path1 and path2 are out of sync, run --resync to recover");
                assert!(details.iter().any(|d| d.contains("pics/a.jpg")), "{details:?}");
            }
            v => panic!("unexpected {v:?}"),
        }
    }

    #[test]
    fn change_summaries() {
        assert_eq!(parse_change_summary("Path1:    4 changes:    1 new,    3 modified,    0 deleted"), Some(("pc", 3)));
        assert_eq!(parse_change_summary("Path2:    1 changes:    0 new,    1 modified,    0 deleted"), Some(("koofr", 1)));
        assert_eq!(parse_change_summary("Path1:  120 changes:    0 new,  120 modified,    0 deleted"), Some(("pc", 120)));
        assert_eq!(parse_change_summary("(Modified:    3 newer,    0 older,    3 larger,    0 smaller)"), None);
        assert_eq!(parse_change_summary("Path1 checking for diffs"), None);
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
        // A blocked engine is never taken for a file or Koofr problem, even
        // though the system's wording can sound like one.
        let blocked = format!("{} (/x/rclone): Permission denied (os error 13)", crate::rclone::BLOCKED_PREFIX);
        assert_eq!(issue_kind(&blocked), "blocked");
        assert_eq!(issue_kind(crate::rclone::BLOCKED_TEXT), "blocked");
        assert_eq!(retry_delay("server", 1), 300);
        assert_eq!(retry_delay("server", 9), 3600);
        assert!(retry_delay("permanent", 1) > 10 * 365 * 86_400);
        assert!(file_reason("server", "*koofr.Object", "x").starts_with("Koofr couldn't send"));

        // Koofr turning requests away, and its wording for a refused file.
        let koofr = |code: u32| format!("Failed to copy: Invalid response status! Got {code}, expected 200; headers: map[Content-Length:[42]], content: x");
        assert_eq!(issue_kind(&koofr(429)), "throttled");
        assert_eq!(issue_kind("HTTP 429: Too Many Requests"), "throttled");
        assert_eq!(issue_kind(&koofr(400)), "rejected");
        assert_eq!(issue_kind(&koofr(503)), "server");
        // Secure connection problems, as Go words them.
        let expired = "Get \"https://app.koofr.net/api/v2/mounts\": tls: failed to verify certificate: x509: certificate has expired or is not yet valid: current time 2019-01-01T00:00:00Z is before 2026-01-01T00:00:00Z";
        let unknown = "Get \"https://app.koofr.net/api/v2/mounts\": tls: failed to verify certificate: x509: certificate signed by unknown authority";
        assert_eq!(issue_kind(expired), "cert");
        assert_eq!(issue_kind(unknown), "cert");
        assert!(cert_hint(expired).contains("date and time"));
        assert!(cert_hint(unknown).contains("Wi-Fi sign-in page"));
        // Windows' own wording for each file problem.
        let win = |m: &str| format!("failed to open source object: open C:\\Users\\r\\x.txt: {m}");
        assert_eq!(issue_kind(&win("The process cannot access the file because another process has locked a portion of the file.")), "locked");
        assert_eq!(issue_kind(&win("The requested operation cannot be performed on a file with a user-mapped section open.")), "locked");
        assert_eq!(issue_kind(&win("Operation did not complete successfully because the file contains a virus or potentially unwanted software.")), "virus");
        assert_eq!(issue_kind(&win("The cloud file provider is not running.")), "onedrive");
        assert_eq!(issue_kind(&win("The file cannot be accessed by the system.")), "cantaccess");
        assert_eq!(issue_kind(&win("The media is write protected.")), "drive");
        assert_eq!(issue_kind(&win("The device is not ready.")), "drive");
        assert_eq!(issue_kind(&win("The device is not connected.")), "drive");
        assert_eq!(issue_kind(&win("Data error (cyclic redundancy check).")), "unreadable");
        assert_eq!(issue_kind(&win("The request could not be performed because of an I/O device error.")), "unreadable");
        assert_eq!(issue_kind(&win("The file or directory is corrupted and unreadable.")), "unreadable");
        assert_eq!(issue_kind(&win("Access is denied.")), "denied");

        // A name in the message never decides the kind: these all used to be
        // read as a wrong password, a full Koofr or a Koofr outage.
        let busy = "The process cannot access the file because it is being used by another process.";
        assert_eq!(issue_kind(&format!("open C:\\Users\\r\\Docs\\Invoice 401.pdf: {busy}")), "busy");
        assert_eq!(issue_kind(&format!("open C:\\Users\\r\\Docs\\Sales quota 2026.xlsx: {busy}")), "busy");
        assert_eq!(issue_kind(&format!("open \\\\?\\C:\\Users\\r\\Server 503 logs\\a.log: {busy}")), "busy");
        assert_eq!(issue_kind("failed to open source object: open /home/r/disk full backup.zip: permission denied"), "denied");
        assert_eq!(issue_kind(&format!("open C:\\Music\\Broken Pipe - Live.mp3: {busy}")), "busy");
        assert_eq!(issue_kind("open C:\\Unauthorized\\a.jpg: Access is denied."), "denied");
        assert_eq!(issue_kind("Failed to copy: Invalid response status! Got 400, expected 200; headers: map[Content-Length:[401]], content: "), "rejected");
        assert_eq!(issue_kind("Failed to copy: Invalid response status! Got 400, expected 200; headers: map[Content-Length:[503]], content: "), "rejected");
        assert_eq!(issue_kind("Post \"https://app.koofr.net/content/api/v2/mounts/x/files/put?path=%2FQuota\": dial tcp: lookup app.koofr.net: no such host"), "network");
        // The OneDrive hint still comes from where the file is.
        assert!(file_reason("cantaccess", "", "open C:\\Users\\r\\OneDrive\\b.txt: The file cannot be accessed by the system.").contains("start OneDrive"));
        assert_eq!(drive_problem_text("open E:\\write protected stuff\\a.txt: The device is not ready."), "Windows can't reach the drive this folder is on. It may be unplugged.");

        // What the user reads for each.
        assert!(file_reason("locked", "", "").contains("open in another program"));
        assert!(file_reason("virus", "", "").contains("Protection history"));
        assert!(file_reason("onedrive", "", "").contains("Always keep on this device"));
        assert_eq!(file_reason("cantaccess", "", "open D:\\stuff\\a.txt: The file cannot be accessed by the system."), "Windows couldn't open this file.");
        assert!(file_reason("cantaccess", "", "open C:\\Users\\r\\OneDrive\\a.txt: The file cannot be accessed by the system.").contains("start OneDrive"));
        assert!(file_reason("unreadable", "", "").contains("drive may be failing"));
        assert!(file_reason("rejected", "", "").starts_with("Koofr refused this file."));
        assert!(file_reason("throttled", "", "").contains("Limit requests to Koofr"));
        assert_eq!(drive_problem_text("The device is not ready."), "Windows can't reach the drive this folder is on. It may be unplugged.");
        assert!(drive_problem_text("The media is write protected.").contains("write-protected"));
        // Anything not recognised gets one plain line; the engine's words go
        // under the technical details.
        assert_eq!(file_reason("unknown", "", "Failed to copy IMG_15001.jpg: odd"), FILE_UNEXPECTED);
        assert!(file_reason("unknown", "", "open x: Access is denied.").contains("Windows"));

        // Which ones are tried again, and how soon.
        for k in ["permanent", "koofr_name", "virus"] {
            assert!(never_retried(k), "{k}");
            assert!(retry_delay(k, 1) > 10 * 365 * 86_400, "{k}");
        }
        for k in ["rejected", "locked", "unreadable", "onedrive", "cantaccess", "throttled", "unknown"] {
            assert!(!never_retried(k), "{k}");
        }
        assert_eq!(retry_delay("rejected", 1), 24 * 3600);
        assert_eq!(retry_delay("unreadable", 3), 3600);
        assert_eq!([1, 2, 3, 4].map(|t| retry_delay("locked", t)), [300, 900, 1800, 3600]);
        assert_eq!(retry_delay("busy", 5), 120);
    }

    #[test]
    fn names_koofr_refuses() {
        assert_eq!(koofr_name_problem("Docs", "notes.txt"), None);
        assert_eq!(koofr_name_problem("Docs", "Caf\u{e9} \u{2600}/r\u{e9}sum\u{e9}.txt"), None);
        let file = koofr_name_problem("Docs", "Trip/beach \u{1F3D6}.jpg").unwrap();
        assert!(file.contains("Rename this file"), "{file}");
        let dir = koofr_name_problem("Docs", "Party \u{1F389}/").unwrap();
        assert!(dir.contains("Rename this folder"), "{dir}");
        let inside = koofr_name_problem("Docs", "Party \u{1F389}/a.jpg").unwrap();
        assert!(inside.contains("Rename the folder it's in"), "{inside}");
        // The whole Koofr path counts, the synced folder's own path included.
        let deep = format!("{}/{}.txt", "d".repeat(500), "f".repeat(515));
        assert_eq!(koofr_name_problem("", &deep), None, "1021 characters with the leading slash");
        assert!(koofr_name_problem("Docs", &deep).unwrap().contains("longer than Koofr allows"));
        assert!(koofr_name_problem("Docs", &format!("{}/", "d".repeat(1020))).unwrap().contains("This folder's path"));
    }

    #[test]
    fn scan_for_names_koofr_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mk = |p: &str| std::fs::create_dir_all(root.join(p)).unwrap();
        let put = |p: &str| std::fs::write(root.join(p), "x").unwrap();
        mk("fine/deeper");
        put("fine/deeper/ok.txt");
        put("fine/bad \u{1F600}.txt");
        mk("Party \u{1F389}/inside");
        put("Party \u{1F389}/inside/a.jpg");
        mk("empty \u{1F4E6}");
        put("~$lock \u{1F600}.docx");
        let mut found = find_koofr_name_problems(root, "Docs", &["~$*".to_string()]);
        found.sort();
        let paths: Vec<&str> = found.iter().map(|(p, _)| p.as_str()).collect();
        // A refused folder is listed once, without what's inside it; names
        // on the ignore list are left alone.
        assert_eq!(paths, vec!["Party \u{1F389}/", "empty \u{1F4E6}/", "fine/bad \u{1F600}.txt"]);
        assert!(found[0].1.contains("Rename this folder"));
        assert!(found[2].1.contains("Rename this file"));
    }

    #[test]
    fn drive_roots() {
        assert_eq!(drive_root("E:\\Photos\\2026").as_deref(), Some("E:\\"));
        assert_eq!(drive_root("e:/Photos").as_deref(), Some("e:\\"));
        assert_eq!(drive_root("\\\\nas\\share\\Photos").as_deref(), Some("\\\\nas\\share\\"));
        assert_eq!(drive_root("//nas/share/Photos").as_deref(), Some("\\\\nas\\share\\"));
        assert_eq!(drive_root("\\\\?\\C:\\long"), None);
        assert_eq!(drive_root("\\\\nas"), None);
        assert_eq!(drive_root("C:"), None);
        assert_eq!(drive_root("/home/r/Photos"), None);
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

        // A name Koofr refuses is called that, whatever the error said, and
        // isn't tried again by itself.
        let refused = vec![FailedFile {
            path: "trip/beach \u{1F3D6}.jpg".into(),
            msg: "Failed to copy: Invalid response status! Got 400, expected 200".into(),
            object_type: "*local.Object".into(),
        }];
        assert!(engine.update_skips(&get(), &refused, &work, false, &[]));
        let k = get().skipped.into_iter().find(|k| k.path.starts_with("trip/")).unwrap();
        assert_eq!(k.kind, "koofr_name");
        assert!(k.reason.contains("Rename this file"));
        assert!(file_view(&k).flagged && !file_view(&k).auto_retry);
        // The check before each sync doesn't find it on disk, but it came
        // from a failed sync, so it stays. One the check added and can no
        // longer find is dropped.
        core.update_pair("p1", |p| {
            p.skipped.push(Skipped { path: "gone \u{1F600}.txt".into(), kind: "koofr_name".into(), reason: "x".into(), ..Default::default() })
        });
        assert!(engine.update_koofr_names(&get(), &core.settings(), &[]));
        assert!(get().skipped.iter().any(|k| k.path.starts_with("trip/")));
        assert!(!get().skipped.iter().any(|k| k.path.starts_with("gone")));
        // Found on disk, an entry from a failed sync becomes the check's own.
        std::fs::create_dir_all(dir.path().join("trip")).unwrap();
        std::fs::write(dir.path().join("trip/beach \u{1F3D6}.jpg"), "x").unwrap();
        assert!(engine.update_koofr_names(&get(), &core.settings(), &[]));
        let k = get().skipped.into_iter().find(|k| k.path.starts_with("trip/")).unwrap();
        assert!(k.detail.is_empty() && k.kind == "koofr_name");
        assert!(!engine.update_koofr_names(&get(), &core.settings(), &[]), "nothing changes the second time");

        // Koofr turning a file away points to the request limit only while
        // it is off.
        let busy = |path: &str| FailedFile { path: path.into(), msg: "Invalid response status! Got 429, expected 200".into(), object_type: "*local.Object".into() };
        assert!(engine.update_skips(&get(), &[busy("one.txt")], &work, false, &[]));
        core.update(|s| s.limit_requests = true);
        assert!(engine.update_skips(&get(), &[busy("two.txt")], &work, false, &[]));
        let reason = |path: &str| get().skipped.into_iter().find(|k| k.path == path).unwrap().reason;
        assert!(reason("one.txt").contains("turn on Limit requests to Koofr"));
        assert_eq!(reason("two.txt"), "Koofr was too busy to take this file.");

        // Un-ignoring a file that is never retried by itself makes it due.
        let mut virus = Skipped { path: "v.exe".into(), kind: "virus".into(), next_retry: u64::MAX / 4, ..Default::default() };
        virus.notified = true;
        core.update_pair("p1", |p| p.ignored.push(virus));
        engine.unignore_file("p1", "v.exe").unwrap();
        let back = get().skipped.into_iter().find(|k| k.path == "v.exe").unwrap();
        assert_eq!((back.kind.as_str(), back.next_retry, back.notified), ("unknown", 0, false));
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
        p.skipped.push(Skipped { path: "Party \u{1F389}/".into(), kind: "koofr_name".into(), ..Default::default() });
        p.ignored.push(Skipped { path: "old*/".into(), ..Default::default() });
        let t = filters_text(&s, &p);
        assert!(t.contains("- ~$*\n"));
        assert!(t.contains("- /sub dir/we\\[ird\\] name.txt\n"));
        // A left-out folder leaves out everything in it, and the folder too.
        assert!(t.contains("- /Party \u{1F389}/**\n"), "{t}");
        assert!(t.contains("- /old\\*/**\n"), "{t}");
    }
}
