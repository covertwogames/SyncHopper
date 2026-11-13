//! Settings that are saved to disk.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Size and modification time of a file, as recorded in a sync record.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FileState {
    pub size: u64,
    /// Nanoseconds since 1970.
    pub mtime_ns: i64,
}

impl FileState {
    /// Same size and modified within a second of each other.
    pub fn same_as(&self, other: &FileState) -> bool {
        self.size == other.size && (self.mtime_ns - other.mtime_ns).abs() <= 1_000_000_000
    }
}

/// True when two optional file states describe the same thing: both missing,
/// or both present and the same.
pub fn same_state(a: &Option<FileState>, b: &Option<FileState>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.same_as(y),
        _ => false,
    }
}

/// A file the sync engine could not handle. It is left out of syncing so the
/// rest of the folder keeps working, and tried again on its own schedule.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Skipped {
    /// Path inside the synced folder, with forward slashes.
    pub path: String,
    /// Plain-language reason shown to the user.
    pub reason: String,
    /// The sync engine's own message, shown under "Technical details".
    pub detail: String,
    /// "server", "network", "busy", "denied", "permanent" or "unknown".
    pub kind: String,
    /// Unix time of the first failure in this run of failures.
    pub first_failed: u64,
    /// Unix time after which the file is tried again.
    pub next_retry: u64,
    pub tries: u32,
    /// The file as both sides last agreed on it, if it had been synced
    /// before. Used to finish the job correctly once the file can sync.
    pub base_pc: Option<FileState>,
    pub base_koofr: Option<FileState>,
    /// A notification was already shown for this file.
    pub notified: bool,
}

/// One synced folder: a folder on this PC paired with a folder in Koofr.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pair {
    pub id: String,
    pub local: String,
    /// Path inside Koofr with forward slashes. Empty means the whole account.
    pub remote: String,
    pub paused: bool,
    pub needs_resync: bool,
    /// Unix time of the last successful sync, 0 when never synced.
    pub last_sync: u64,
    pub skipped: Vec<Skipped>,
    /// Files the user chose to stop syncing. Kept with their sync state so
    /// syncing them again later picks up where it left off.
    pub ignored: Vec<Skipped>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub setup_complete: bool,
    pub account_email: String,

    // Features
    pub feature_sync: bool,
    pub feature_drive: bool,
    pub drive_letter: String,
    pub feature_context_menu: bool,
    pub feature_sidebar: bool,
    pub autostart: bool,

    // Syncing
    pub check_interval_min: u32,
    pub transfers: u32,
    pub max_delete_percent: u32,

    // Advanced
    pub use_http2: bool,
    /// Megabytes per second, 0 means no limit.
    pub upload_limit_mbps: f64,
    pub download_limit_mbps: f64,
    /// "keep_both" or "newest".
    pub conflict_policy: String,
    pub ignore_patterns: Vec<String>,
    pub pause_on_battery: bool,
    pub pause_on_metered: bool,
    /// "errors", "all" or "none".
    pub notify_level: String,
    pub auto_update_engine: bool,
    pub auto_update_app: bool,

    // State
    pub base_folder: String,
    pub upload_dest: String,
    pub engine_checked_at: u64,
    pub tray_hint_shown: bool,
    pub pairs: Vec<Pair>,
}

pub fn default_ignores() -> Vec<String> {
    ["~$*", "*.tmp", "Thumbs.db", "desktop.ini", ".DS_Store", "*.partial", "*.crdownload"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            setup_complete: false,
            account_email: String::new(),
            feature_sync: true,
            feature_drive: false,
            drive_letter: "K".into(),
            feature_context_menu: true,
            feature_sidebar: true,
            autostart: true,
            check_interval_min: 10,
            transfers: 8,
            max_delete_percent: 50,
            use_http2: false,
            upload_limit_mbps: 0.0,
            download_limit_mbps: 0.0,
            conflict_policy: "keep_both".into(),
            ignore_patterns: default_ignores(),
            pause_on_battery: false,
            pause_on_metered: true,
            notify_level: "errors".into(),
            auto_update_engine: true,
            auto_update_app: true,
            base_folder: String::new(),
            upload_dest: String::new(),
            engine_checked_at: 0,
            tray_hint_shown: false,
            pairs: Vec::new(),
        }
    }
}

impl Settings {
    /// Puts the advanced options back to their defaults. Account, features,
    /// folders and the basic sync options are left alone.
    pub fn restore_advanced_defaults(&mut self) {
        let d = Settings::default();
        self.use_http2 = d.use_http2;
        self.upload_limit_mbps = d.upload_limit_mbps;
        self.download_limit_mbps = d.download_limit_mbps;
        self.conflict_policy = d.conflict_policy;
        self.ignore_patterns = d.ignore_patterns;
        self.pause_on_battery = d.pause_on_battery;
        self.pause_on_metered = d.pause_on_metered;
        self.notify_level = d.notify_level;
        self.auto_update_engine = d.auto_update_engine;
        self.auto_update_app = d.auto_update_app;
    }

    /// Keeps values inside the ranges the app supports.
    pub fn sanitize(&mut self) {
        self.check_interval_min = self.check_interval_min.clamp(1, 24 * 60);
        self.transfers = self.transfers.clamp(1, 16);
        self.max_delete_percent = self.max_delete_percent.clamp(1, 100);
        if !self.upload_limit_mbps.is_finite() || self.upload_limit_mbps < 0.0 {
            self.upload_limit_mbps = 0.0;
        }
        if !self.download_limit_mbps.is_finite() || self.download_limit_mbps < 0.0 {
            self.download_limit_mbps = 0.0;
        }
        if self.conflict_policy != "newest" {
            self.conflict_policy = "keep_both".into();
        }
        if !["errors", "all", "none"].contains(&self.notify_level.as_str()) {
            self.notify_level = "errors".into();
        }
        let letter: String = self
            .drive_letter
            .chars()
            .filter(|c| c.is_ascii_alphabetic())
            .take(1)
            .collect::<String>()
            .to_uppercase();
        self.drive_letter = if letter.is_empty() { "K".into() } else { letter };
        self.ignore_patterns = self
            .ignore_patterns
            .iter()
            .map(|p| p.trim().trim_start_matches("- ").trim_start_matches("+ ").trim().to_string())
            .filter(|p| !p.is_empty() && !p.contains('\n'))
            .collect();
        self.ignore_patterns.dedup();
    }

    /// The value for rclone's --bwlimit flag, or None when nothing is limited.
    pub fn bwlimit(&self) -> Option<String> {
        if self.upload_limit_mbps <= 0.0 && self.download_limit_mbps <= 0.0 {
            return None;
        }
        let part = |mbps: f64| {
            if mbps <= 0.0 {
                "off".to_string()
            } else {
                format!("{}k", ((mbps * 1024.0).round() as u64).max(8))
            }
        };
        Some(format!("{}:{}", part(self.upload_limit_mbps), part(self.download_limit_mbps)))
    }
}

pub fn load(path: &Path) -> Settings {
    let mut s: Settings = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    s.sanitize();
    // Entries saved by version 0.1.0 have no kind or timing yet. Give them
    // one and let them be tried again soon.
    let now = crate::util::now_secs();
    for pair in s.pairs.iter_mut() {
        for k in pair.skipped.iter_mut().chain(pair.ignored.iter_mut()) {
            if k.kind.is_empty() {
                k.kind = "unknown".into();
            }
            if k.first_failed == 0 {
                k.first_failed = now;
            }
        }
    }
    s
}

/// Writes the settings through a temp file so a crash cannot leave half a file.
pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bwlimit_values() {
        let mut s = Settings::default();
        assert_eq!(s.bwlimit(), None);
        s.upload_limit_mbps = 2.0;
        assert_eq!(s.bwlimit().as_deref(), Some("2048k:off"));
        s.download_limit_mbps = 0.5;
        assert_eq!(s.bwlimit().as_deref(), Some("2048k:512k"));
    }

    #[test]
    fn old_files_get_new_defaults() {
        let s: Settings = serde_json::from_str(r#"{"setup_complete":true,"account_email":"a@b.c"}"#).unwrap();
        assert!(s.setup_complete);
        assert_eq!(s.transfers, 8);
        assert!(!s.use_http2);
        assert_eq!(s.ignore_patterns, default_ignores());
    }

    #[test]
    fn settings_from_version_0_1_0_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let old = r#"{"setup_complete":true,"account_email":"a@b.c","pairs":[{"id":"p1","local":"/x","remote":"Docs","skipped":[{"path":"a.txt","reason":"r","sticky":true,"tries":3}]}]}"#;
        std::fs::write(&path, old).unwrap();
        let s = load(&path);
        assert!(s.feature_sidebar && s.auto_update_app);
        let k = &s.pairs[0].skipped[0];
        assert_eq!(k.path, "a.txt");
        assert_eq!(k.kind, "unknown");
        assert!(k.first_failed > 0);
        assert_eq!(k.next_retry, 0);
        assert!(s.pairs[0].ignored.is_empty());
    }

    #[test]
    fn file_states_compare_with_a_second_of_slack() {
        let a = FileState { size: 5, mtime_ns: 1_000_000_000_000 };
        let b = FileState { size: 5, mtime_ns: 1_500_000_000_000 };
        let c = FileState { size: 6, mtime_ns: 1_000_000_000_000 };
        assert!(!a.same_as(&b), "500 seconds apart is a change");
        assert!(a.same_as(&FileState { size: 5, mtime_ns: 1_000_000_000_999 }));
        assert!(!a.same_as(&c));
        assert!(same_state(&None, &None));
        assert!(!same_state(&Some(a), &None));
    }

    #[test]
    fn sanitize_fixes_bad_values() {
        let mut s = Settings::default();
        s.transfers = 0;
        s.drive_letter = "k:\\".into();
        s.conflict_policy = "weird".into();
        s.ignore_patterns = vec!["  - *.bak ".into(), "".into()];
        s.sanitize();
        assert_eq!(s.transfers, 1);
        assert_eq!(s.drive_letter, "K");
        assert_eq!(s.conflict_policy, "keep_both");
        assert_eq!(s.ignore_patterns, vec!["*.bak".to_string()]);
    }
}
