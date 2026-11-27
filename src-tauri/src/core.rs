//! State shared by every part of the app: where things live on disk, the
//! saved settings, the sync engine program and the log file (`Shared`), and
//! one account's view of all that (`Core`).

use crate::config::{self, Account, General, Pair, Settings, Store};
use crate::providers::{self, Provider};
use crate::rclone::{self, Rclone};
use crate::util::now_secs;
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// What the engine needs from the window layer. Tests supply a dummy.
pub trait Host: Send + Sync {
    /// Something the screens show has changed.
    fn state_changed(&self);
    fn notify(&self, title: &str, body: &str);
    /// A notification with a "Sync it" button for a folder on this PC.
    /// Hosts without buttons show a plain notification.
    fn notify_folder(&self, title: &str, body: &str, folder: &str) {
        let _ = folder;
        self.notify(title, body);
    }
    fn set_autostart(&self, on: bool) -> Result<(), String>;
    fn exe_path(&self) -> Option<PathBuf>;
    /// Brings the window forward, optionally on a named screen.
    fn show_window(&self, view: Option<&str>);
}

#[derive(Clone, Copy, PartialEq)]
pub enum Notice {
    /// Needs the user's attention.
    Problem,
    /// Nice to know.
    Info,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EngineInfo {
    /// "checking", "ready", "missing", "downloading", "error" or "blocked"
    /// (Windows refuses to run it).
    pub state: String,
    pub version: String,
    /// True for a copy signed by Cover Two, from the signed engine channel.
    pub signed: bool,
    pub percent: u8,
    pub message: String,

    #[serde(skip)]
    pub exe: Option<PathBuf>,
}

/// What every account shares: where things live on disk, the saved
/// settings, the sync engine program and the log file.
pub struct Shared {
    pub data: PathBuf,
    pub test_mode: bool,
    pub host: Arc<dyn Host>,
    store: Mutex<Store>,
    engine: Mutex<EngineInfo>,
    /// Set once the user has been told the engine is blocked, until it runs
    /// again, so they hear it once and not every time it is retried.
    blocked_told: std::sync::atomic::AtomicBool,
    log_lock: Mutex<()>,
    runtime: std::sync::OnceLock<tokio::runtime::Handle>,
    /// What the saved sign-ins are locked with, once they are. See
    /// `rclone::Signins`.
    config_pass: Mutex<Option<String>>,
}

impl Shared {
    /// Reads the settings. A setup from version 0.4 or earlier becomes the
    /// first account, and is saved in the current shape right away.
    pub fn open(data: PathBuf, test_mode: bool, host: Arc<dyn Host>) -> Arc<Shared> {
        let _ = std::fs::create_dir_all(&data);
        let opened = config::open(&data.join("settings.json"));
        let shared = Arc::new(Shared {
            data,
            test_mode,
            host,
            store: Mutex::new(opened.store),
            engine: Mutex::new(EngineInfo { state: "checking".into(), ..Default::default() }),
            blocked_told: std::sync::atomic::AtomicBool::new(false),
            log_lock: Mutex::new(()),
            runtime: std::sync::OnceLock::new(),
            config_pass: Mutex::new(None),
        });
        if opened.migrated {
            shared.log(&format!(
                "Settings from an earlier version were moved into the new shape. A copy of the old file is kept as {}",
                config::LEGACY_BACKUP
            ));
            shared.change(|_| {});
        }
        shared
    }

    /// Remembers the async runtime so background work can be started from
    /// any thread, including ones the window system calls us on.
    pub fn note_runtime(&self) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = self.runtime.set(handle);
        }
    }

    /// Starts background work. Safe to call from any thread.
    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = self.runtime.set(handle.clone());
            handle.spawn(future);
        } else if let Some(handle) = self.runtime.get() {
            handle.spawn(future);
        } else {
            self.log("Background work could not start because the app is still starting up.");
        }
    }

    pub fn store(&self) -> Store {
        self.store.lock().unwrap().clone()
    }

    pub fn general(&self) -> General {
        self.store.lock().unwrap().general.clone()
    }

    pub fn accounts(&self) -> Vec<Account> {
        self.store.lock().unwrap().accounts.clone()
    }

    pub fn account(&self, id: &str) -> Option<Account> {
        self.store.lock().unwrap().account(id).cloned()
    }

    pub fn account_count(&self) -> usize {
        self.store.lock().unwrap().accounts.len()
    }

    /// Changes anything in the settings file, saves it and refreshes the
    /// screens.
    pub fn change<R>(&self, change: impl FnOnce(&mut Store) -> R) -> R {
        let (result, saved) = {
            let mut s = self.store.lock().unwrap();
            let r = change(&mut s);
            s.sanitize();
            // Saved before the lock is let go. Accounts change settings at
            // the same moment, and saving afterwards let two saves run into
            // each other: one failed, or the older one landed last.
            (r, config::save(&self.data.join("settings.json"), &s))
        };
        if let Err(e) = saved {
            self.log(&format!("Could not save settings: {e}"));
        }
        self.host.state_changed();
        result
    }

    pub fn engine_info(&self) -> EngineInfo {
        self.engine.lock().unwrap().clone()
    }

    pub fn set_engine(&self, change: impl FnOnce(&mut EngineInfo)) {
        change(&mut self.engine.lock().unwrap());
        self.host.state_changed();
    }

    /// The system refused to run the sync engine. Nothing syncs until it
    /// runs again, and the user is told once, not once per folder.
    pub fn engine_blocked(&self, raw: &str) {
        let changed = {
            let mut e = self.engine.lock().unwrap();
            let changed = e.state != "blocked";
            e.state = "blocked".into();
            e.exe = None;
            e.percent = 0;
            e.message = rclone::BLOCKED_TEXT.into();
            changed
        };
        if changed {
            self.log(&format!("The sync engine can't run: {raw}"));
        }
        if !self.blocked_told.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.notify(Notice::Problem, "SyncHopper can't sync", rclone::BLOCKED_TEXT);
        }
        self.host.state_changed();
    }

    /// The engine runs again, so a later block is worth telling about.
    pub fn engine_runs_again(&self) {
        self.blocked_told.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// The sync engine's own settings file, which holds the saved sign-ins.
    pub fn config_path(&self) -> PathBuf {
        self.data.join("rclone.conf")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.data.join("cache")
    }

    pub fn config_pass(&self) -> Option<String> {
        self.config_pass.lock().unwrap().clone()
    }

    pub fn set_config_pass(&self, pass: Option<String>) {
        *self.config_pass.lock().unwrap() = pass;
    }

    /// The sync engine, set up to run with `extra` added to every command.
    pub fn rclone(&self, extra: Vec<String>) -> Result<Rclone, String> {
        let info = self.engine.lock().unwrap().clone();
        let exe = match info.exe {
            Some(exe) => exe,
            None if info.state == "blocked" => return Err(info.message),
            None => return Err("The sync engine is not installed yet.".to_string()),
        };
        Ok(Rclone { exe, config: self.config_path(), cache: self.cache_dir(), extra, pass: self.config_pass() })
    }

    pub fn log_path(&self) -> PathBuf {
        self.data.join("logs").join("synchopper.log")
    }

    /// Adds a line to the log file. The file is kept under about 4 MB.
    pub fn log(&self, line: &str) {
        let _guard = self.log_lock.lock();
        let path = self.log_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::metadata(&path).map(|m| m.len() > 4_000_000).unwrap_or(false) {
            let _ = std::fs::rename(&path, path.with_extension("old.log"));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{} {}", timestamp(now_secs()), line.replace('\n', " "));
        }
    }

    /// Shows a notification if the user's notification choice allows it.
    pub fn notify(&self, kind: Notice, title: &str, body: &str) {
        if self.notice_allowed(kind) {
            self.host.notify(title, body);
        }
    }

    /// Like `notify`, with a "Sync it" button for `folder`.
    pub fn notify_folder(&self, kind: Notice, title: &str, body: &str, folder: &str) {
        if self.notice_allowed(kind) {
            self.host.notify_folder(title, body, folder);
        }
    }

    fn notice_allowed(&self, kind: Notice) -> bool {
        let level = self.store.lock().unwrap().general.notify_level.clone();
        match level.as_str() {
            "none" => false,
            "all" => true,
            _ => kind == Notice::Problem,
        }
    }

    /// True when the notification setting is anything but "Nothing".
    pub fn notices_on(&self) -> bool {
        self.store.lock().unwrap().general.notify_level != "none"
    }
}

/// One account's view of the shared state. The sync engine, the drive, the
/// uploads and the new-folder watcher each hold one and never see any other
/// account.
pub struct Core {
    pub shared: Arc<Shared>,
    pub data: PathBuf,
    pub test_mode: bool,
    pub host: Arc<dyn Host>,
    /// The account's id in the settings file.
    pub account: String,
    /// "koofr:" for the first account. Tests point this at a plain folder.
    pub remote_root: String,
    /// The service the account is with.
    pub provider: &'static Provider,
}

impl Core {
    /// A core that stands on its own with a single account, the way the
    /// whole app worked before it had accounts. Tests are built on this.
    pub fn new(data: PathBuf, remote_root: String, test_mode: bool, host: Arc<dyn Host>) -> Arc<Core> {
        let shared = Shared::open(data, test_mode, host);
        let id = match shared.accounts().first() {
            Some(a) => a.id.clone(),
            None => {
                shared.change(|s| {
                    s.accounts.push(Account {
                        id: config::FIRST_ACCOUNT.into(),
                        provider: "koofr".into(),
                        name: "Koofr".into(),
                        remote: config::FIRST_ACCOUNT.into(),
                        sidebar_slot: Some(0),
                        ..Default::default()
                    })
                });
                config::FIRST_ACCOUNT.to_string()
            }
        };
        Core::for_account(shared, &id, Some(remote_root))
    }

    /// The core for one account. `remote_root` is where its files are, when
    /// that isn't the account itself (tests and development builds).
    pub fn for_account(shared: Arc<Shared>, id: &str, remote_root: Option<String>) -> Arc<Core> {
        let account = shared.account(id).unwrap_or_default();
        Arc::new(Core {
            data: shared.data.clone(),
            test_mode: shared.test_mode,
            host: shared.host.clone(),
            account: id.to_string(),
            remote_root: remote_root.unwrap_or_else(|| format!("{}:", account.remote)),
            provider: providers::get(&account.provider),
            shared,
        })
    }

    /// The service's name, for messages: "Koofr", "pCloud", "Filen".
    pub fn cloud(&self) -> &'static str {
        self.provider.name
    }

    /// The account's own name, as the window shows it.
    pub fn label(&self) -> String {
        self.shared.account(&self.account).map(|a| a.name).unwrap_or_else(|| self.provider.name.to_string())
    }

    /// Drive letters the other accounts' drives are set to.
    pub fn other_letters(&self) -> Vec<String> {
        self.shared.accounts().into_iter().filter(|a| a.id != self.account && a.feature_drive).map(|a| a.drive_letter).collect()
    }

    pub fn note_runtime(&self) {
        self.shared.note_runtime();
    }

    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.shared.spawn(future);
    }

    /// The general settings together with this account's own.
    pub fn settings(&self) -> Settings {
        self.shared.store.lock().unwrap().view(&self.account)
    }

    /// Changes the settings, saves them and refreshes the screens.
    pub fn update<R>(&self, change: impl FnOnce(&mut Settings) -> R) -> R {
        let account = self.account.clone();
        self.shared.change(move |store| {
            let mut view = store.view(&account);
            let r = change(&mut view);
            view.sanitize();
            store.put(&account, view);
            r
        })
    }

    pub fn pair(&self, id: &str) -> Option<Pair> {
        let store = self.shared.store.lock().unwrap();
        store.account(&self.account).and_then(|a| a.pairs.iter().find(|p| p.id == id).cloned())
    }

    pub fn update_pair(&self, id: &str, change: impl FnOnce(&mut Pair)) {
        self.update(|s| {
            if let Some(p) = s.pairs.iter_mut().find(|p| p.id == id) {
                change(p);
            }
        });
    }

    pub fn engine_info(&self) -> EngineInfo {
        self.shared.engine_info()
    }

    pub fn set_engine(&self, change: impl FnOnce(&mut EngineInfo)) {
        self.shared.set_engine(change);
    }

    pub fn engine_blocked(&self, raw: &str) {
        self.shared.engine_blocked(raw);
    }

    pub fn engine_runs_again(&self) {
        self.shared.engine_runs_again();
    }

    pub fn config_path(&self) -> PathBuf {
        self.shared.config_path()
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.shared.cache_dir()
    }

    /// The sync engine, with what this account's service needs added to
    /// every command.
    pub fn rclone(&self) -> Result<Rclone, String> {
        let s = self.settings();
        let mut extra = Vec::new();
        if let (true, Some(limit)) = (s.limit_requests, self.provider.request_limit) {
            // rclone's --tpslimit-burst already defaults to 1, so requests
            // are spaced evenly without saying so.
            extra.push("--tpslimit".to_string());
            extra.push(limit.to_string());
        }
        self.shared.rclone(extra)
    }

    /// The full rclone path for a folder inside the account.
    pub fn remote(&self, rel: &str) -> String {
        let rel = crate::util::clean_remote(rel);
        if self.remote_root.ends_with(':') {
            format!("{}{}", self.remote_root, rel)
        } else if rel.is_empty() {
            self.remote_root.clone()
        } else {
            format!("{}/{}", self.remote_root.trim_end_matches('/'), rel)
        }
    }

    pub fn pair_dir(&self, id: &str) -> PathBuf {
        self.data.join("pairs").join(id)
    }

    pub fn log_path(&self) -> PathBuf {
        self.shared.log_path()
    }

    /// Adds a line to the log file, marked with the account's name once
    /// there is more than one account.
    pub fn log(&self, line: &str) {
        if self.shared.account_count() > 1 {
            self.shared.log(&format!("({}) {line}", self.label()));
        } else {
            self.shared.log(line);
        }
    }

    /// With more than one account, notifications say which one they are
    /// about.
    fn titled(&self, title: &str) -> String {
        if self.shared.account_count() > 1 {
            format!("{}: {title}", self.label())
        } else {
            title.to_string()
        }
    }

    /// Shows a notification if the user's notification choice allows it.
    pub fn notify(&self, kind: Notice, title: &str, body: &str) {
        self.shared.notify(kind, &self.titled(title), body);
    }

    /// Like `notify`, with a "Sync it" button for `folder`.
    pub fn notify_folder(&self, kind: Notice, title: &str, body: &str, folder: &str) {
        self.shared.notify_folder(kind, &self.titled(title), body, folder);
    }
}

/// Unix seconds as "2026-11-12 19:31:02Z".
pub fn timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from a day count, after Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00Z");
        assert_eq!(timestamp(1_794_526_262), "2026-11-12 23:31:02Z");
        assert_eq!(timestamp(951_782_400), "2000-02-29 00:00:00Z");
    }
}
