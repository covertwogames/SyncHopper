//! Ties the pieces together and holds everything the screens ask for. This
//! file knows nothing about windows, so it can be tested on its own.

use crate::config::Settings;
use crate::core::{Core, Host};
use crate::engine::Engine;
use crate::mount::Drive;
use crate::newfolders::NewFolders;
use crate::platform;
use crate::rclone::{self, friendly_error, About, Rclone};
use crate::uploads::Uploads;
use crate::util::{clean_remote, file_name, norm_local, now_secs, system_folder_reason};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const KOOFR_PASSWORD_URL: &str = "https://app.koofr.net/app/admin/preferences/password";
pub const WINFSP_URL: &str = "https://winfsp.dev/rel/";

/// How often a failed engine download is tried again on its own.
const ENGINE_RETRY: Duration = Duration::from_secs(5 * 60);

pub struct App {
    pub core: Arc<Core>,
    pub engine: Arc<Engine>,
    pub uploads: Arc<Uploads>,
    pub drive: Arc<Drive>,
    pub folders: Arc<NewFolders>,
    about: Mutex<Option<About>>,
    add_request: Mutex<Option<AddRequest>>,
    /// Whether the Windows 11 right-click entry is in use. Worked out once.
    menu_modern: Mutex<Option<bool>>,
    started: AtomicBool,
    download_base: String,
}

/// A folder someone asked to sync from outside the main window, for example
/// with "Sync folder to Koofr" in File Explorer. The window picks it up and
/// opens the Add a folder dialog with it filled in.
#[derive(Clone, Debug, Serialize)]
pub struct AddRequest {
    pub local: String,
    /// Suggested Koofr folder.
    pub remote: String,
    /// Shown in the dialog, for example that Koofr already has this folder.
    pub note: Option<String>,
    /// Set when the folder can't be synced. The window shows this instead of
    /// the dialog.
    pub blocked: Option<String>,
}

#[derive(Deserialize)]
pub struct SetupChoices {
    pub feature_sync: bool,
    pub feature_drive: bool,
    pub autostart: bool,
    pub base_folder: String,
    /// Top-level Koofr folders to keep on this PC.
    pub folders: Vec<String>,
    /// Every top-level folder the setup screen listed, so unticked ones can
    /// stop syncing.
    pub listed: Vec<String>,
}

#[derive(Serialize)]
pub struct Preview {
    pub remote_count: u64,
    pub remote_bytes: u64,
    pub local_count: u64,
    pub local_bytes: u64,
    pub free_bytes: Option<u64>,
}

fn count_local(dir: &Path) -> (u64, u64) {
    let mut count = 0;
    let mut bytes = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => {
                    count += 1;
                    bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    (count, bytes)
}

fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut p = Some(path);
    while let Some(cur) = p {
        if cur.is_dir() {
            return Some(cur.to_path_buf());
        }
        p = cur.parent();
    }
    None
}

fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((name, domain)) => format!("{}***@{}", name.chars().next().unwrap_or('*'), domain),
        None => "(none)".to_string(),
    }
}

impl App {
    pub fn new(data: PathBuf, remote_root: String, test_mode: bool, host: Arc<dyn Host>) -> Arc<App> {
        Self::with_download_base(data, remote_root, test_mode, host, rclone::DOWNLOAD_BASE.to_string())
    }

    pub fn with_download_base(
        data: PathBuf,
        remote_root: String,
        test_mode: bool,
        host: Arc<dyn Host>,
        download_base: String,
    ) -> Arc<App> {
        let core = Core::new(data, remote_root, test_mode, host);
        let engine = Engine::new(core.clone());
        Arc::new(App {
            folders: NewFolders::new(core.clone(), engine.clone()),
            engine,
            uploads: Uploads::new(core.clone()),
            drive: Drive::new(core.clone()),
            core,
            about: Mutex::new(None),
            add_request: Mutex::new(None),
            menu_modern: Mutex::new(None),
            started: AtomicBool::new(false),
            download_base,
        })
    }

    /// Everything the screens need, in one piece.
    pub fn snapshot(&self) -> serde_json::Value {
        let s = self.core.settings();
        let about = self.about.lock().unwrap().clone();
        let mut settings = serde_json::to_value(&s).unwrap_or_else(|_| json!({}));
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("pairs");
            obj.remove("base_known");
        }
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "windows": cfg!(windows),
            "test_mode": self.core.test_mode,
            "setup_complete": s.setup_complete,
            "account": { "email": s.account_email, "used": about.as_ref().and_then(|a| a.used), "total": about.as_ref().and_then(|a| a.total) },
            "engine": self.core.engine_info(),
            "settings": settings,
            "sync": self.engine.sync_view(),
            "pairs": self.engine.pair_views(),
            "drive": self.drive.view(),
            "uploads": self.uploads.views(),
            "default_ignores": crate::config::default_ignores(),
            "folder_notices": self.folders.notices(),
            "add_request": self.add_request.lock().unwrap().clone(),
            "now": now_secs(),
        })
    }

    /// One-line status for the tray icon.
    pub fn status_line(&self) -> String {
        let s = self.core.settings();
        let sync = self.engine.sync_view();
        if !s.setup_complete {
            "SyncHopper: setup not finished".into()
        } else if self.engine.has_problems() {
            "SyncHopper: needs attention".into()
        } else if self.engine.waiting_for_koofr() {
            "SyncHopper: waiting for Koofr".into()
        } else if self.engine.busy() {
            "SyncHopper: syncing".into()
        } else if self.uploads.active() > 0 {
            "SyncHopper: uploading".into()
        } else if sync.paused_all || sync.auto_pause.is_some() {
            "SyncHopper: paused".into()
        } else {
            "SyncHopper: up to date".into()
        }
    }

    // ----- the sync engine program --------------------------------------

    /// Finds the sync engine, downloading it when it is missing.
    pub async fn ensure_engine(self: &Arc<Self>) {
        {
            let info = self.core.engine_info();
            if info.state == "downloading" {
                return;
            }
        }
        self.core.set_engine(|e| {
            e.state = "checking".into();
            e.message.clear();
        });
        let mut found = rclone::locate(&self.core.data).await;

        // Keep the app's own copy current, at most one check a day.
        if let Some(loc) = &found {
            let s = self.core.settings();
            let due = now_secs().saturating_sub(s.engine_checked_at) > 24 * 3600;
            if s.auto_update_engine && due && !self.core.test_mode {
                self.core.update(|s| s.engine_checked_at = now_secs());
                if let Ok(latest) = rclone::latest_version(&self.download_base).await {
                    let newer = rclone::version_triple(&latest) > rclone::version_triple(&loc.version);
                    if newer {
                        self.core.log(&format!("Updating the sync engine from {} to {latest}", loc.version));
                        if self.download_engine().await.is_ok() {
                            found = rclone::locate(&self.core.data).await;
                        }
                    }
                }
            }
        }

        match found {
            Some(loc) => {
                self.core.log(&format!("Sync engine ready: rclone {} at {}", loc.version, loc.exe.display()));
                self.core.set_engine(|e| {
                    e.state = "ready".into();
                    e.version = loc.version.clone();
                    e.exe = Some(loc.exe.clone());
                    e.message.clear();
                });
            }
            None => {
                if self.download_engine().await.is_ok() {
                    if let Some(loc) = rclone::locate(&self.core.data).await {
                        self.core.log(&format!("Sync engine downloaded: rclone {}", loc.version));
                        self.core.set_engine(|e| {
                            e.state = "ready".into();
                            e.version = loc.version.clone();
                            e.exe = Some(loc.exe.clone());
                            e.message.clear();
                        });
                    } else {
                        self.core.set_engine(|e| {
                            e.state = "error".into();
                            e.message = "The sync engine was downloaded but would not start.".into();
                        });
                    }
                }
            }
        }
    }

    async fn download_engine(self: &Arc<Self>) -> Result<(), String> {
        let had = self.core.engine_info().exe;
        self.core.set_engine(|e| {
            e.state = "downloading".into();
            e.percent = 0;
            e.message.clear();
        });
        let core = self.core.clone();
        let result = rclone::download(&self.core.data, &self.download_base, move |pct| {
            core.set_engine(|e| e.percent = pct);
        })
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) => {
                self.core.log(&format!("Sync engine download failed: {e}"));
                self.core.set_engine(|info| {
                    if had.is_some() {
                        info.state = "ready".into();
                    } else {
                        info.state = "error".into();
                        info.message = e.clone();
                    }
                });
                Err(e)
            }
        }
    }

    // ----- startup -------------------------------------------------------

    /// Called once when the app opens.
    pub async fn start(self: &Arc<Self>) {
        self.core.note_runtime();
        self.core.log(&format!("SyncHopper {} starting", env!("CARGO_PKG_VERSION")));
        platform::kill_orphans(&self.core.config_path()).await;
        self.ensure_engine().await;
        // If the engine could not be downloaded (for example the PC was
        // offline), keep trying in the background instead of waiting for
        // someone to press Try again.
        let me = self.clone();
        self.core.spawn(async move {
            loop {
                tokio::time::sleep(ENGINE_RETRY).await;
                if me.core.engine_info().state == "error" {
                    me.ensure_engine().await;
                }
            }
        });
        if self.core.settings().setup_complete {
            self.start_services().await;
        }
    }

    /// Starts syncing, the drive and the Explorer entry. Safe to call again.
    async fn start_services(self: &Arc<Self>) {
        if !self.started.swap(true, Ordering::SeqCst) {
            self.engine.start();
            self.folders.start();
            let me = self.clone();
            self.core.spawn(async move {
                loop {
                    me.refresh_about().await;
                    tokio::time::sleep(Duration::from_secs(15 * 60)).await;
                }
            });
        } else {
            self.engine.rewatch();
            self.folders.rewatch();
        }
        // Applied at every start so both always point at the current program
        // file, for example after an update moved it.
        let s = self.core.settings();
        if let Err(e) = self.core.host.set_autostart(s.autostart) {
            self.core.log(&format!("Could not apply the start with Windows setting: {e}"));
        }
        if let Err(e) = self.apply_menu().await {
            self.core.log(&e);
        }
        if let Err(e) = self.apply_sidebar().await {
            self.core.log(&e);
        }
        self.drive.apply().await;
    }

    /// Puts File Explorer's right-click entries in line with the settings.
    /// On Windows 11 with a signed release the entry is in the main menu;
    /// otherwise the classic entries are used.
    pub async fn apply_menu(&self) -> Result<(), String> {
        let s = self.core.settings();
        let Some(exe) = self.core.host.exe_path() else { return Ok(()) };
        if !cfg!(windows) || self.core.test_mode {
            return Ok(());
        }
        let wanted = s.setup_complete && s.feature_context_menu;
        let add_folder = wanted && s.feature_sync;
        if let Err(e) = platform::set_menu_flags(wanted, add_folder, &exe).await {
            self.core.log(&format!("Could not save the right-click menu settings: {e}"));
        }
        let cached = *self.menu_modern.lock().unwrap();
        let modern = match cached {
            Some(m) => m,
            None => {
                let m = match platform::ensure_menu_package(&exe, env!("CARGO_PKG_VERSION")).await {
                    Ok(m) => m,
                    Err(e) => {
                        self.core.log(&format!("The Windows 11 right-click entry could not be set up, using the classic one: {e}"));
                        false
                    }
                };
                *self.menu_modern.lock().unwrap() = Some(m);
                m
            }
        };
        platform::set_legacy_menu(wanted && !modern, add_folder, &exe).await
    }

    /// Adds or removes the Koofr entry in File Explorer's sidebar to match
    /// the settings. It points at the folder that holds the synced folders.
    pub async fn apply_sidebar(&self) -> Result<(), String> {
        let s = self.core.settings();
        let Some(exe) = self.core.host.exe_path() else { return Ok(()) };
        let wanted = s.setup_complete && s.feature_sync && s.feature_sidebar && !s.base_folder.trim().is_empty();
        if wanted {
            let folder = PathBuf::from(s.base_folder.trim());
            std::fs::create_dir_all(&folder).map_err(|e| format!("Could not create {}: {e}", folder.display()))?;
            platform::set_sidebar(Some(&folder), &exe).await
        } else {
            platform::set_sidebar(None, &exe).await
        }
    }

    pub async fn refresh_about(&self) {
        if !self.core.settings().setup_complete {
            return;
        }
        if let Ok(rc) = self.core.rclone() {
            if let Ok(a) = rc.about(&self.core.remote("")).await {
                *self.about.lock().unwrap() = Some(a);
                self.core.host.state_changed();
            }
        }
    }

    // ----- account -------------------------------------------------------

    /// Checks an email and app password against Koofr and saves them when
    /// they work.
    pub async fn connect(&self, email: &str, password: &str) -> Result<About, String> {
        let email = email.trim().to_string();
        let rc = self.core.rclone()?;
        if self.core.test_mode {
            let about = rc.about(&self.core.remote("")).await?;
            *self.about.lock().unwrap() = Some(about.clone());
            self.core.update(|s| s.account_email = email);
            return Ok(about);
        }
        if email.is_empty() || !email.contains('@') || email.chars().any(|c| c.is_control() || c == '[' || c == ']') {
            return Err("Enter the email address you use for Koofr.".into());
        }
        if password.trim().is_empty() {
            return Err("Enter a Koofr app password.".into());
        }
        let obscured = rc.obscure(password.trim()).await?;
        let pending = self.core.data.join("rclone.conf.pending");
        let text = format!("[koofr]\ntype = koofr\nprovider = koofr\nuser = {email}\npassword = {obscured}\n");
        std::fs::write(&pending, text).map_err(|e| format!("Could not save the connection: {e}"))?;
        let probe = Rclone { config: pending.clone(), ..rc };
        match probe.about("koofr:").await {
            Ok(about) => {
                let dest = self.core.config_path();
                let _ = std::fs::remove_file(&dest);
                std::fs::rename(&pending, &dest).map_err(|e| format!("Could not save the connection: {e}"))?;
                *self.about.lock().unwrap() = Some(about.clone());
                self.core.update(|s| s.account_email = email);
                self.core.log("Connected to Koofr");
                Ok(about)
            }
            Err(raw) => {
                let _ = std::fs::remove_file(&pending);
                self.core.log(&format!("Connection test failed: {raw}"));
                let friendly = friendly_error(&raw);
                Err(if friendly.starts_with("Koofr did not accept") {
                    "Koofr did not accept that email and app password. Make sure it is an app password from Koofr's password page, not your normal Koofr password.".into()
                } else if friendly.starts_with("Could not reach Koofr") {
                    format!("Could not reach Koofr. Details: {}", raw.chars().take(200).collect::<String>())
                } else {
                    format!("The connection test failed. Details: {}", raw.chars().take(300).collect::<String>())
                })
            }
        }
    }

    /// Signs out: stops everything and forgets the account and its folders.
    /// Files stay where they are on this PC and in Koofr.
    pub async fn disconnect(self: &Arc<Self>) {
        self.engine.stop_all();
        self.uploads.cancel_all();
        self.drive.stop();
        self.engine.wait_idle().await;
        let ids: Vec<String> = self.core.settings().pairs.iter().map(|p| p.id.clone()).collect();
        for id in ids {
            self.engine.remove_pair(&id);
        }
        let _ = std::fs::remove_file(self.core.config_path());
        let _ = std::fs::remove_dir_all(self.core.data.join("pairs"));
        *self.about.lock().unwrap() = None;
        let _ = self.core.host.set_autostart(false);
        if let Some(exe) = self.core.host.exe_path() {
            let _ = platform::set_sidebar(None, &exe).await;
        }
        self.core.update(|s| {
            s.setup_complete = false;
            s.account_email.clear();
            s.pairs.clear();
            s.base_known.clear();
            s.base_known_ready = false;
        });
        let _ = self.apply_menu().await;
        self.engine.rewatch();
        self.folders.rewatch();
        self.core.log("Disconnected from Koofr");
    }

    // ----- setup ---------------------------------------------------------

    /// Applies the choices from the setup screens. Returns notes about
    /// anything that could not be done exactly as asked.
    pub async fn finish_setup(self: &Arc<Self>, c: SetupChoices) -> Result<Vec<String>, String> {
        if self.core.settings().account_email.is_empty() {
            return Err("Connect your Koofr account first.".into());
        }
        if self.core.engine_info().exe.is_none() {
            return Err("The sync engine is not ready yet.".into());
        }
        let base = c.base_folder.trim().to_string();
        if c.feature_sync && !c.folders.is_empty() && (base.is_empty() || !Path::new(&base).is_absolute()) {
            return Err("Pick where to keep your folders on this PC.".into());
        }
        let mut notes = Vec::new();
        self.core.update(|s| {
            s.feature_sync = c.feature_sync;
            s.feature_drive = c.feature_drive;
            s.autostart = c.autostart;
            if !base.is_empty() {
                if norm_local(&s.base_folder) != norm_local(&base) {
                    // A different Koofr folder: what is in it now is not new.
                    s.base_known.clear();
                    s.base_known_ready = false;
                }
                s.base_folder = base.clone();
            }
            s.setup_complete = true;
        });
        if let Err(e) = self.core.host.set_autostart(c.autostart) {
            notes.push(format!("Could not change the start with Windows setting: {e}"));
        }
        if !c.feature_sync {
            self.engine.stop_all();
        }
        if c.feature_sync {
            let wanted: Vec<String> = c.folders.iter().map(|f| clean_remote(f)).filter(|f| !f.is_empty()).collect();
            let listed: Vec<String> = c.listed.iter().map(|f| clean_remote(f)).collect();
            for p in self.core.settings().pairs {
                if listed.contains(&p.remote) && !wanted.contains(&p.remote) {
                    self.engine.remove_pair(&p.id);
                }
            }
            for name in wanted {
                if self.core.settings().pairs.iter().any(|p| p.remote.eq_ignore_ascii_case(&name)) {
                    continue;
                }
                let local = Path::new(&base).join(name.replace('/', std::path::MAIN_SEPARATOR_STR));
                if let Err(e) = self.engine.add_pair(&local.to_string_lossy(), &name) {
                    notes.push(format!("{name}: {e}"));
                }
            }
        }
        self.start_services().await;
        if c.feature_drive && self.drive.view().state == "needs_driver" {
            let drive = self.drive.clone();
            self.core.spawn(async move { drive.install_driver().await });
        }
        let me = self.clone();
        self.core.spawn(async move { me.refresh_about().await });
        Ok(notes)
    }

    /// Saves changed settings and applies what follows from them.
    pub async fn update_settings(self: &Arc<Self>, patch: serde_json::Value) -> Result<(), String> {
        const EDITABLE: [&str; 20] = [
            "feature_sync", "feature_drive", "drive_letter", "feature_context_menu", "feature_sidebar", "autostart",
            "check_interval_min", "transfers", "max_delete_percent", "auto_add_folders", "use_http2", "upload_limit_mbps",
            "download_limit_mbps", "conflict_policy", "ignore_patterns", "pause_on_battery",
            "pause_on_metered", "notify_level", "auto_update_engine", "auto_update_app",
        ];
        let old = self.core.settings();
        let mut value = serde_json::to_value(&old).map_err(|e| e.to_string())?;
        if let (Some(target), Some(source)) = (value.as_object_mut(), patch.as_object()) {
            for key in EDITABLE {
                if let Some(v) = source.get(key) {
                    target.insert(key.to_string(), v.clone());
                }
            }
        }
        let mut new: Settings = serde_json::from_value(value).map_err(|e| format!("That setting could not be read: {e}"))?;
        new.sanitize();
        // Only the editable settings are written back, so anything changed in
        // the meantime (such as the list of folders) is kept.
        let applied = serde_json::to_value(&new).map_err(|e| e.to_string())?;
        self.core.update(move |s| {
            if let Ok(mut current) = serde_json::to_value(&*s) {
                if let (Some(target), Some(source)) = (current.as_object_mut(), applied.as_object()) {
                    for key in EDITABLE {
                        if let Some(v) = source.get(key) {
                            target.insert(key.to_string(), v.clone());
                        }
                    }
                }
                if let Ok(merged) = serde_json::from_value::<Settings>(current) {
                    *s = merged;
                }
            }
        });
        let mut problems: Vec<String> = Vec::new();

        if old.autostart != new.autostart {
            if let Err(e) = self.core.host.set_autostart(new.autostart) {
                self.core.update(|s| s.autostart = old.autostart);
                problems.push(format!("Could not change the start with Windows setting: {e}"));
            }
        }
        if old.feature_context_menu != new.feature_context_menu || old.feature_sync != new.feature_sync {
            if let Err(e) = self.apply_menu().await {
                self.core.update(|s| s.feature_context_menu = old.feature_context_menu);
                let _ = self.apply_menu().await;
                problems.push(e);
            }
        }
        if old.feature_sync != new.feature_sync {
            if !new.feature_sync {
                self.engine.stop_all();
            }
            self.engine.rewatch();
            self.folders.rewatch();
        }
        if old.feature_sidebar != new.feature_sidebar || old.feature_sync != new.feature_sync {
            if let Err(e) = self.apply_sidebar().await {
                self.core.update(|s| s.feature_sidebar = old.feature_sidebar);
                problems.push(e);
            }
        }
        if old.ignore_patterns != new.ignore_patterns {
            self.engine.filters_changed();
        }
        if old.feature_drive != new.feature_drive || old.drive_letter != new.drive_letter || old.use_http2 != new.use_http2 {
            let drive = self.drive.clone();
            let turned_on = !old.feature_drive && new.feature_drive;
            self.core.spawn(async move {
                drive.restart().await;
                if turned_on && drive.view().state == "needs_driver" {
                    drive.install_driver().await;
                }
            });
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join(" "))
        }
    }

    pub async fn restore_defaults(self: &Arc<Self>) -> Result<(), String> {
        let mut defaults = self.core.settings();
        defaults.restore_advanced_defaults();
        let patch = serde_json::to_value(&defaults).map_err(|e| e.to_string())?;
        let keep = ["use_http2", "upload_limit_mbps", "download_limit_mbps", "conflict_policy", "ignore_patterns", "pause_on_battery", "pause_on_metered", "notify_level", "auto_update_engine", "auto_update_app"];
        let only: serde_json::Map<String, serde_json::Value> = patch
            .as_object()
            .map(|o| o.iter().filter(|(k, _)| keep.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        self.update_settings(serde_json::Value::Object(only)).await
    }

    // ----- browsing and previews ----------------------------------------

    pub async fn list_remote(&self, path: &str) -> Result<Vec<String>, String> {
        let rc = self.core.rclone()?;
        rc.list_dirs(&self.core.remote(path)).await.map_err(|e| {
            self.core.log(&format!("Listing {path} failed: {e}"));
            friendly_error(&e)
        })
    }

    pub async fn make_remote_dir(&self, path: &str) -> Result<(), String> {
        let path = clean_remote(path);
        if path.is_empty() {
            return Err("Type a name for the new folder.".into());
        }
        let rc = self.core.rclone()?;
        rc.mkdir(&self.core.remote(&path)).await.map_err(|e| friendly_error(&e))
    }

    /// What a first sync of this pairing would involve.
    pub async fn preview(&self, local: &str, remote: &str) -> Result<Preview, String> {
        let rc = self.core.rclone()?;
        let (remote_count, remote_bytes) = match rc.size(&self.core.remote(remote)).await {
            Ok(v) => v,
            Err(e) if e.to_lowercase().contains("not found") => (0, 0),
            Err(e) => return Err(friendly_error(&e)),
        };
        let local_path = PathBuf::from(local);
        let lp = local_path.clone();
        let (local_count, local_bytes) = tokio::task::spawn_blocking(move || count_local(&lp)).await.unwrap_or((0, 0));
        let free_bytes = match existing_ancestor(&local_path) {
            Some(dir) => rc.about(&dir.to_string_lossy()).await.ok().and_then(|a| a.free),
            None => None,
        };
        Ok(Preview { remote_count, remote_bytes, local_count, local_bytes, free_bytes })
    }

    // ----- uploads from outside -----------------------------------------

    /// Handles command-line arguments, including the ones Explorer's
    /// right-click entry sends. Returns true when uploads were queued.
    pub fn handle_args(self: &Arc<Self>, args: &[String]) -> bool {
        let mut paths = Vec::new();
        let mut add = None;
        let mut iter = args.iter();
        while let Some(a) = iter.next() {
            if a == "--upload" {
                if let Some(p) = iter.next() {
                    paths.push(p.clone());
                }
            } else if a == "--add-folder" {
                add = iter.next().cloned();
            }
        }
        if let Some(folder) = add {
            self.request_add_folder(&folder);
            return true;
        }
        if paths.is_empty() {
            return false;
        }
        let s = self.core.settings();
        if !s.setup_complete {
            self.core.host.show_window(None);
            return false;
        }
        self.uploads.add(paths, &s.upload_dest);
        self.core.host.show_window(Some("upload"));
        true
    }

    /// Asks the window to offer syncing `folder`, with the Add a folder
    /// dialog filled in, or to explain why it can't be synced.
    pub fn request_add_folder(self: &Arc<Self>, folder: &str) {
        let s = self.core.settings();
        if !s.setup_complete {
            self.core.host.show_window(None);
            return;
        }
        let local = folder.trim().to_string();
        let name = file_name(&local);
        let nl = norm_local(&local);
        let sep = std::path::MAIN_SEPARATOR;
        let holds = |outer: &str, inner: &str| inner == outer || (inner.starts_with(outer) && inner[outer.len()..].starts_with(sep));
        let base = s.base_folder.trim();
        let blocked = if !s.feature_sync {
            Some("Syncing folders is turned off. Turn it on in Settings first.".to_string())
        } else if !base.is_empty() && norm_local(base) == nl {
            Some(if s.auto_add_folders {
                "This is your Koofr folder, where your synced folders live. New folders you put in it sync by themselves.".to_string()
            } else {
                "This is your Koofr folder, where your synced folders live. Pick a folder inside it to sync.".to_string()
            })
        } else if let Some(p) = s.pairs.iter().find(|p| holds(&norm_local(&p.local), &nl)) {
            Some(if norm_local(&p.local) == nl {
                format!("\"{name}\" is already synced with Koofr.")
            } else {
                format!("This folder is inside \"{}\", which is already synced with Koofr.", file_name(&p.local))
            })
        } else if let Some(p) = s.pairs.iter().find(|p| holds(&nl, &norm_local(&p.local))) {
            Some(format!("This folder holds \"{}\", which is already synced with Koofr. Pick a folder that doesn't overlap it.", file_name(&p.local)))
        } else if let Some(why) = system_folder_reason(&local, cfg!(windows), &|n| std::env::var(n).ok()) {
            Some(why)
        } else if !Path::new(&local).is_dir() {
            Some("That folder doesn't exist any more.".to_string())
        } else {
            None
        };
        let note = self
            .folders
            .notices()
            .into_iter()
            .find(|n| n.kind == "clash" && norm_local(&n.local) == nl)
            .map(|n| n.message);
        *self.add_request.lock().unwrap() = Some(AddRequest { local, remote: name, note, blocked });
        self.core.host.show_window(None);
        self.core.host.state_changed();
    }

    /// The window took the request.
    pub fn clear_add_request(&self) {
        *self.add_request.lock().unwrap() = None;
        self.core.host.state_changed();
    }

    pub fn set_upload_dest(&self, dest: &str) {
        let dest = clean_remote(dest);
        self.core.update(|s| s.upload_dest = dest);
    }

    // ----- troubleshooting ----------------------------------------------

    /// Plain-text details for a support request. No passwords, and the
    /// email address is masked.
    pub fn diagnostics(&self) -> String {
        let s = self.core.settings();
        let e = self.core.engine_info();
        let d = self.drive.view();
        let mut out = String::new();
        out.push_str(&format!("SyncHopper {}\n", env!("CARGO_PKG_VERSION")));
        out.push_str(&format!("System: {} {}\n", std::env::consts::OS, std::env::consts::ARCH));
        out.push_str(&format!("Sync engine: rclone {} ({})\n", e.version, e.state));
        out.push_str(&format!("Account: {}\n", mask_email(&s.account_email)));
        out.push_str(&format!(
            "Features: sync {}, drive {} ({}), right-click {}, sidebar {}, start with Windows {}, app updates {}\n",
            s.feature_sync, s.feature_drive, d.state, s.feature_context_menu, s.feature_sidebar, s.autostart, s.auto_update_app
        ));
        out.push_str(&format!(
            "Sync: every {} min, {} at a time, HTTP/2 {}, limits up {} down {} MB/s, conflicts {}, delete check {}%\n",
            s.check_interval_min, s.transfers, if s.use_http2 { "on" } else { "off" }, s.upload_limit_mbps, s.download_limit_mbps, s.conflict_policy, s.max_delete_percent
        ));
        out.push_str(&format!("Ignored: {}\n", s.ignore_patterns.join(", ")));
        let sync = self.engine.sync_view();
        out.push_str(&format!("Paused: {} {}\n", sync.paused_all, sync.auto_pause.unwrap_or_default()));
        for p in self.engine.pair_views() {
            out.push_str(&format!(
                "Folder: {} <-> Koofr:/{} | {:?}{} | last synced {} | left out {} | ignored {} | {}{}\n",
                p.local,
                p.remote,
                p.phase,
                if p.paused { " (paused)" } else { "" },
                if p.last_sync == 0 { "never".to_string() } else { crate::core::timestamp(p.last_sync) },
                p.files.len(),
                p.ignored.len(),
                p.error.unwrap_or_default(),
                p.waiting.unwrap_or_default()
            ));
            for f in p.files.iter().take(5) {
                out.push_str(&format!("    left out: {} ({}): {}\n", f.path, f.kind, f.detail));
            }
            for line in p.details.iter().take(5) {
                out.push_str(&format!("    {line}\n"));
            }
        }
        out.push_str(&format!("Log file: {}\n", self.core.log_path().display()));
        out
    }
}
