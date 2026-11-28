//! Settings that are saved to disk.
//!
//! The file holds the settings shared by every account (`General`) and one
//! entry per connected cloud account (`Account`). The sync engine works on
//! one account at a time and sees a flat `Settings`: the general settings
//! plus that account's own. `Store::view` builds it and `Store::put` writes
//! it back.

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
    /// What went wrong, as `engine::issue_kind` sorts it ("server",
    /// "busy", "koofr_name" and so on).
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

/// One synced folder: a folder on this PC paired with a folder in the cloud
/// account. Names with "koofr" in them here and in the engine mean the cloud
/// side, whichever provider it is; they are kept so saved files stay readable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pair {
    pub id: String,
    pub local: String,
    /// Path inside the account with forward slashes. Empty means the whole account.
    pub remote: String,
    pub paused: bool,
    pub needs_resync: bool,
    /// Unix time of the last successful sync, 0 when never synced.
    pub last_sync: u64,
    pub skipped: Vec<Skipped>,
    /// Identifies the PC folder itself, so a rename can be recognised. Empty
    /// when not known yet.
    pub local_id: String,
    /// Files the user chose to stop syncing. Kept with their sync state so
    /// syncing them again later picks up where it left off.
    pub ignored: Vec<Skipped>,
}

/// What one account's sync engine, drive and uploads work from: the general
/// settings plus the account's own. This is also the shape version 0.4 and
/// earlier saved, when there was only ever one account.
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
    /// New folders put straight into the base folder become synced folders
    /// by themselves. When off, SyncHopper only points them out.
    pub auto_add_folders: bool,

    // Advanced
    pub use_http2: bool,
    /// Keeps each rclone run to the request rate Koofr recommends, for when
    /// Koofr says SyncHopper is sending too many requests.
    pub limit_requests: bool,
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
    /// Names of the folders and files directly in the base folder that
    /// SyncHopper has already seen, so only new ones are acted on.
    pub base_known: Vec<String>,
    /// False until the base folder was looked at for the first time. What is
    /// there at that point is never treated as new.
    pub base_known_ready: bool,
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
            auto_add_folders: true,
            use_http2: false,
            limit_requests: false,
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
            base_known: Vec::new(),
            base_known_ready: false,
            pairs: Vec::new(),
        }
    }
}

impl Settings {
    /// Puts the general settings back to their defaults: the delete check,
    /// limits, conflicts, ignored files, pausing, notifications and
    /// updates. Everything that belongs to one account is left alone, and
    /// so are starting with Windows and the right-click menu.
    pub fn restore_general_defaults(&mut self) {
        let d = Settings::default();
        self.max_delete_percent = d.max_delete_percent;
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

    /// Flags added to every rclone run. With "Limit requests to Koofr" on,
    /// each run sends at most 40 requests a second, the rate Koofr
    /// recommends. rclone's --tpslimit-burst already defaults to 1, so
    /// requests are spaced evenly without saying so.
    pub fn engine_extra_flags(&self) -> Vec<String> {
        if self.limit_requests {
            vec!["--tpslimit".into(), KOOFR_REQUEST_LIMIT.to_string()]
        } else {
            Vec::new()
        }
    }
}

/// Requests per second Koofr recommends rclone users stay under.
pub const KOOFR_REQUEST_LIMIT: u32 = 40;

/// The shape of the settings file. Version 0.4 and earlier saved a flat
/// `Settings` with no number; those are format 1.
pub const FORMAT: u32 = 2;

/// The id, engine name and sidebar place the account from version 0.4 and
/// earlier keeps, so its sync records and File Explorer entry carry over.
pub const FIRST_ACCOUNT: &str = "koofr";

/// Settings shared by every account.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub feature_context_menu: bool,
    pub autostart: bool,
    pub max_delete_percent: u32,
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
    pub engine_checked_at: u64,
    pub tray_hint_shown: bool,
    /// The SyncHopper version a "there is an update" notification was last
    /// shown for, so each version is announced once.
    pub update_notified: String,
}

impl Default for General {
    fn default() -> Self {
        let d = Settings::default();
        General {
            feature_context_menu: d.feature_context_menu,
            autostart: d.autostart,
            max_delete_percent: d.max_delete_percent,
            upload_limit_mbps: d.upload_limit_mbps,
            download_limit_mbps: d.download_limit_mbps,
            conflict_policy: d.conflict_policy,
            ignore_patterns: d.ignore_patterns,
            pause_on_battery: d.pause_on_battery,
            pause_on_metered: d.pause_on_metered,
            notify_level: d.notify_level,
            auto_update_engine: d.auto_update_engine,
            auto_update_app: d.auto_update_app,
            engine_checked_at: d.engine_checked_at,
            tray_hint_shown: d.tray_hint_shown,
            update_notified: String::new(),
        }
    }
}

/// One connected cloud account and everything that belongs to it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    pub id: String,
    /// Which service this is: "koofr", "pcloud", "filen" or "kdrive".
    pub provider: String,
    /// The name shown in the window. The user can change it.
    pub name: String,
    /// The account's name in the sync engine's own settings file.
    pub remote: String,
    /// Which of SyncHopper's File Explorer sidebar entries is this
    /// account's. None until it has one.
    pub sidebar_slot: Option<usize>,
    /// The color of the account's icon, one of `colors::ALL`.
    pub color: String,
    /// kDrive only: the number of the kDrive this account is. It is part of
    /// the kDrive's web address, so it is not a secret.
    pub kdrive: String,

    pub setup_complete: bool,
    /// The email address or other name the account signs in with.
    pub account_email: String,
    pub feature_sync: bool,
    pub feature_drive: bool,
    pub drive_letter: String,
    pub feature_sidebar: bool,
    pub check_interval_min: u32,
    pub transfers: u32,
    pub auto_add_folders: bool,
    pub use_http2: bool,
    pub limit_requests: bool,
    pub base_folder: String,
    pub upload_dest: String,
    pub base_known: Vec<String>,
    pub base_known_ready: bool,
    pub pairs: Vec<Pair>,
}

impl Default for Account {
    fn default() -> Self {
        let d = Settings::default();
        Account {
            id: String::new(),
            provider: "koofr".into(),
            name: String::new(),
            remote: String::new(),
            sidebar_slot: None,
            color: String::new(),
            kdrive: String::new(),
            setup_complete: d.setup_complete,
            account_email: d.account_email,
            feature_sync: d.feature_sync,
            feature_drive: d.feature_drive,
            drive_letter: d.drive_letter,
            feature_sidebar: d.feature_sidebar,
            check_interval_min: d.check_interval_min,
            transfers: d.transfers,
            auto_add_folders: d.auto_add_folders,
            use_http2: d.use_http2,
            limit_requests: d.limit_requests,
            base_folder: d.base_folder,
            upload_dest: d.upload_dest,
            base_known: d.base_known,
            base_known_ready: d.base_known_ready,
            pairs: d.pairs,
        }
    }
}

/// Everything in the settings file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub format: u32,
    pub general: General,
    pub accounts: Vec<Account>,
}

impl Store {
    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn account_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    /// The general settings plus one account's own, as the engine uses
    /// them. An id that isn't there gives the general settings with a new
    /// account's defaults.
    pub fn view(&self, id: &str) -> Settings {
        let g = self.general.clone();
        let a = self.account(id).cloned().unwrap_or_default();
        Settings {
            setup_complete: a.setup_complete,
            account_email: a.account_email,
            feature_sync: a.feature_sync,
            feature_drive: a.feature_drive,
            drive_letter: a.drive_letter,
            feature_context_menu: g.feature_context_menu,
            feature_sidebar: a.feature_sidebar,
            autostart: g.autostart,
            check_interval_min: a.check_interval_min,
            transfers: a.transfers,
            max_delete_percent: g.max_delete_percent,
            auto_add_folders: a.auto_add_folders,
            use_http2: a.use_http2,
            limit_requests: a.limit_requests,
            upload_limit_mbps: g.upload_limit_mbps,
            download_limit_mbps: g.download_limit_mbps,
            conflict_policy: g.conflict_policy,
            ignore_patterns: g.ignore_patterns,
            pause_on_battery: g.pause_on_battery,
            pause_on_metered: g.pause_on_metered,
            notify_level: g.notify_level,
            auto_update_engine: g.auto_update_engine,
            auto_update_app: g.auto_update_app,
            base_folder: a.base_folder,
            upload_dest: a.upload_dest,
            engine_checked_at: g.engine_checked_at,
            tray_hint_shown: g.tray_hint_shown,
            base_known: a.base_known,
            base_known_ready: a.base_known_ready,
            pairs: a.pairs,
        }
    }

    /// Writes a view back: the general part always, the account part when
    /// the account still exists.
    pub fn put(&mut self, id: &str, s: Settings) {
        let g = &mut self.general;
        g.feature_context_menu = s.feature_context_menu;
        g.autostart = s.autostart;
        g.max_delete_percent = s.max_delete_percent;
        g.upload_limit_mbps = s.upload_limit_mbps;
        g.download_limit_mbps = s.download_limit_mbps;
        g.conflict_policy = s.conflict_policy;
        g.ignore_patterns = s.ignore_patterns;
        g.pause_on_battery = s.pause_on_battery;
        g.pause_on_metered = s.pause_on_metered;
        g.notify_level = s.notify_level;
        g.auto_update_engine = s.auto_update_engine;
        g.auto_update_app = s.auto_update_app;
        g.engine_checked_at = s.engine_checked_at;
        g.tray_hint_shown = s.tray_hint_shown;
        if let Some(a) = self.account_mut(id) {
            a.setup_complete = s.setup_complete;
            a.account_email = s.account_email;
            a.feature_sync = s.feature_sync;
            a.feature_drive = s.feature_drive;
            a.drive_letter = s.drive_letter;
            a.feature_sidebar = s.feature_sidebar;
            a.check_interval_min = s.check_interval_min;
            a.transfers = s.transfers;
            a.auto_add_folders = s.auto_add_folders;
            a.use_http2 = s.use_http2;
            a.limit_requests = s.limit_requests;
            a.base_folder = s.base_folder;
            a.upload_dest = s.upload_dest;
            a.base_known = s.base_known;
            a.base_known_ready = s.base_known_ready;
            a.pairs = s.pairs;
        }
    }

    /// Keeps every value inside the ranges the app supports.
    pub fn sanitize(&mut self) {
        self.format = FORMAT;
        let ids: Vec<String> = self.accounts.iter().map(|a| a.id.clone()).collect();
        // With no accounts this still runs once, for the general settings.
        for id in ids.iter().map(String::as_str).chain(std::iter::once("")).take(ids.len().max(1)) {
            let mut v = self.view(id);
            v.sanitize();
            self.put(id, v);
        }
        for a in self.accounts.iter_mut() {
            a.name = a.name.trim().chars().filter(|c| !c.is_control()).take(40).collect();
            if a.name.is_empty() {
                a.name = a.provider.clone();
            }
            a.kdrive.retain(|c| c.is_ascii_digit());
        }
        // An account without a color (saved before accounts had colors)
        // gets the first one no other account has, in the order the
        // accounts were added. So the first account keeps the app's teal.
        for i in 0..self.accounts.len() {
            if crate::colors::find(&self.accounts[i].color).is_none() {
                let used: Vec<&str> = self.accounts.iter().map(|a| a.color.as_str()).filter(|c| crate::colors::find(c).is_some()).collect();
                self.accounts[i].color = crate::colors::next_free(&used).to_string();
            }
        }
    }

    /// Turns what version 0.4 and earlier saved into the current shape: its
    /// one Koofr account becomes the first account, with the same folders.
    fn from_legacy(old: Settings) -> Store {
        let mut store = Store { format: FORMAT, ..Default::default() };
        let had_account = old.setup_complete || !old.account_email.trim().is_empty() || !old.pairs.is_empty();
        if had_account {
            store.accounts.push(Account {
                id: FIRST_ACCOUNT.into(),
                provider: "koofr".into(),
                name: "Koofr".into(),
                remote: FIRST_ACCOUNT.into(),
                sidebar_slot: Some(0),
                ..Default::default()
            });
        }
        store.put(FIRST_ACCOUNT, old);
        store
    }
}

/// What `open` found.
pub struct Opened {
    pub store: Store,
    /// The file was in the shape version 0.4 and earlier saved, and has been
    /// read into the current one. A copy of the old file was kept.
    pub migrated: bool,
}

/// The name the old settings file is copied to before it is rewritten in
/// the current shape.
pub const LEGACY_BACKUP: &str = "settings-before-accounts.json";

/// Reads the settings file. A file from version 0.4 or earlier is read into
/// the current shape, after a copy of it is put next to it.
pub fn open(path: &Path) -> Opened {
    let text = std::fs::read_to_string(path).ok();
    let value: Option<serde_json::Value> = text.as_deref().and_then(|t| serde_json::from_str(t).ok());
    let legacy = value.as_ref().and_then(|v| v.as_object()).is_some_and(|o| !o.contains_key("accounts") && !o.contains_key("format"));
    let mut store = match value {
        Some(v) if legacy => Store::from_legacy(serde_json::from_value::<Settings>(v).unwrap_or_default()),
        Some(v) => serde_json::from_value::<Store>(v).unwrap_or_default(),
        None => Store::default(),
    };
    if legacy {
        if let (Some(dir), Some(t)) = (path.parent(), text.as_deref()) {
            let backup = dir.join(LEGACY_BACKUP);
            if !backup.exists() {
                let _ = std::fs::write(backup, t);
            }
        }
    }
    store.sanitize();
    // Entries saved by version 0.1.0 have no kind or timing yet. Give them
    // one and let them be tried again soon.
    let now = crate::util::now_secs();
    for pair in store.accounts.iter_mut().flat_map(|a| a.pairs.iter_mut()) {
        for k in pair.skipped.iter_mut().chain(pair.ignored.iter_mut()) {
            if k.kind.is_empty() {
                k.kind = "unknown".into();
            }
            if k.first_failed == 0 {
                k.first_failed = now;
            }
        }
    }
    Opened { store, migrated: legacy }
}

/// Writes the settings through a temp file so a crash cannot leave half a file.
pub fn save(path: &Path, store: &Store) -> Result<(), String> {
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
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
        assert!(!s.limit_requests);
        assert_eq!(s.ignore_patterns, default_ignores());
    }

    #[test]
    fn request_limit_flags() {
        let mut s = Settings::default();
        assert!(s.engine_extra_flags().is_empty());
        s.limit_requests = true;
        assert_eq!(s.engine_extra_flags(), vec!["--tpslimit".to_string(), "40".to_string()]);
        s.max_delete_percent = 10;
        s.restore_general_defaults();
        assert!(s.limit_requests, "an account's own setting is not a general one");
        assert_eq!(s.max_delete_percent, 50);
    }

    #[test]
    fn settings_from_version_0_1_0_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let old = r#"{"setup_complete":true,"account_email":"a@b.c","pairs":[{"id":"p1","local":"/x","remote":"Docs","skipped":[{"path":"a.txt","reason":"r","sticky":true,"tries":3}]}]}"#;
        std::fs::write(&path, old).unwrap();
        let s = open(&path).store.view(FIRST_ACCOUNT);
        assert!(s.feature_sidebar && s.auto_update_app);
        let k = &s.pairs[0].skipped[0];
        assert_eq!(k.path, "a.txt");
        assert_eq!(k.kind, "unknown");
        assert!(k.first_failed > 0);
        assert_eq!(k.next_retry, 0);
        assert!(s.pairs[0].ignored.is_empty());
    }

    /// A settings file as version 0.4.1 writes it, with everything changed
    /// from its default so nothing can be lost without this noticing.
    fn file_from_0_4_1() -> String {
        let old = Settings {
            setup_complete: true,
            account_email: "robert@example.com".into(),
            feature_sync: true,
            feature_drive: true,
            drive_letter: "M".into(),
            feature_context_menu: false,
            feature_sidebar: false,
            autostart: false,
            check_interval_min: 30,
            transfers: 4,
            max_delete_percent: 20,
            auto_add_folders: false,
            use_http2: true,
            limit_requests: true,
            upload_limit_mbps: 1.5,
            download_limit_mbps: 2.5,
            conflict_policy: "newest".into(),
            ignore_patterns: vec!["*.bak".into()],
            pause_on_battery: true,
            pause_on_metered: false,
            notify_level: "all".into(),
            auto_update_engine: false,
            auto_update_app: false,
            base_folder: r"C:\Users\Robert\Koofr".into(),
            upload_dest: "Uploads".into(),
            engine_checked_at: 1_794_000_000,
            tray_hint_shown: true,
            base_known: vec!["Documents".into(), "Old".into()],
            base_known_ready: true,
            pairs: vec![Pair {
                id: "p18a".into(),
                local: r"C:\Users\Robert\Koofr\Documents".into(),
                remote: "Documents".into(),
                paused: true,
                last_sync: 1_794_000_100,
                local_id: "ab-cd".into(),
                skipped: vec![Skipped { path: "a.txt".into(), kind: "busy".into(), first_failed: 5, ..Default::default() }],
                ignored: vec![Skipped { path: "b.txt".into(), kind: "permanent".into(), first_failed: 6, ..Default::default() }],
                ..Default::default()
            }],
        };
        serde_json::to_string_pretty(&old).unwrap()
    }

    #[test]
    fn a_0_4_1_setup_becomes_the_first_account_with_nothing_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let old_text = file_from_0_4_1();
        std::fs::write(&path, &old_text).unwrap();
        let opened = open(&path);
        assert!(opened.migrated);
        let store = opened.store;
        assert_eq!(store.format, FORMAT);
        assert_eq!(store.accounts.len(), 1);
        let a = &store.accounts[0];
        assert_eq!((a.id.as_str(), a.provider.as_str(), a.name.as_str(), a.remote.as_str()), ("koofr", "koofr", "Koofr", "koofr"));
        assert_eq!(a.sidebar_slot, Some(0));
        // Every value reads back the same through the account's view.
        let before: serde_json::Value = serde_json::from_str(&old_text).unwrap();
        let after = serde_json::to_value(store.view(FIRST_ACCOUNT)).unwrap();
        assert_eq!(before, after);
        // The old file was kept next to the new one.
        assert_eq!(std::fs::read_to_string(dir.path().join(LEGACY_BACKUP)).unwrap(), old_text);
        // Saved and read again, it is the current shape and still the same.
        save(&path, &store).unwrap();
        let again = open(&path);
        assert!(!again.migrated);
        assert_eq!(serde_json::to_value(again.store.view(FIRST_ACCOUNT)).unwrap(), before);
        // A later start never overwrites the copy of the old file.
        std::fs::write(&path, r#"{"setup_complete":true,"account_email":"other@example.com"}"#).unwrap();
        open(&path);
        assert_eq!(std::fs::read_to_string(dir.path().join(LEGACY_BACKUP)).unwrap(), old_text);
    }

    #[test]
    fn a_fresh_install_has_no_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let opened = open(&dir.path().join("settings.json"));
        assert!(!opened.migrated);
        assert!(opened.store.accounts.is_empty());
        // An old file where setup never got as far as signing in: same thing,
        // but its general choices are kept.
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"setup_complete":false,"account_email":"","notify_level":"none"}"#).unwrap();
        let opened = open(&path);
        assert!(opened.migrated);
        assert!(opened.store.accounts.is_empty());
        assert_eq!(opened.store.general.notify_level, "none");
    }

    #[test]
    fn accounts_keep_their_own_settings_and_share_the_general_ones() {
        let mut store = Store::default();
        store.accounts.push(Account { id: "a".into(), provider: "koofr".into(), name: "Koofr".into(), ..Default::default() });
        store.accounts.push(Account { id: "b".into(), provider: "pcloud".into(), name: "pCloud".into(), ..Default::default() });
        let mut a = store.view("a");
        a.transfers = 3;
        a.drive_letter = "Q".into();
        a.notify_level = "all".into();
        a.pairs.push(Pair { id: "p1".into(), ..Default::default() });
        store.put("a", a);
        let b = store.view("b");
        assert_eq!(b.transfers, 8, "the other account's own setting is untouched");
        assert_eq!(b.drive_letter, "K");
        assert!(b.pairs.is_empty());
        assert_eq!(b.notify_level, "all", "a general setting is the same for every account");
        // Writing through an account that is gone only changes the general part.
        let mut gone = store.view("zzz");
        gone.notify_level = "none".into();
        gone.transfers = 1;
        store.put("zzz", gone);
        assert_eq!(store.general.notify_level, "none");
        assert_eq!(store.view("a").transfers, 3);
        assert_eq!(store.accounts.len(), 2);
    }

    #[test]
    fn sanitize_covers_every_account_and_an_empty_store() {
        let mut store = Store::default();
        store.general.conflict_policy = "weird".into();
        store.sanitize();
        assert_eq!(store.general.conflict_policy, "keep_both");
        store.accounts.push(Account { id: "a".into(), transfers: 0, name: "  ".into(), provider: "filen".into(), ..Default::default() });
        store.accounts.push(Account { id: "b".into(), drive_letter: "q:\\".into(), name: "Work\n".into(), ..Default::default() });
        store.sanitize();
        assert_eq!(store.accounts[0].transfers, 1);
        assert_eq!(store.accounts[0].name, "filen");
        assert_eq!(store.accounts[1].drive_letter, "Q");
        assert_eq!(store.accounts[1].name, "Work");
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
