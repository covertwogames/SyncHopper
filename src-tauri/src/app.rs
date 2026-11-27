//! Ties the pieces together and holds everything the screens ask for. This
//! file knows nothing about windows, so it can be tested on its own.
//!
//! `App` is one cloud account with its own sync engine, drive, uploads and
//! new-folder watcher. `Commons` is what the accounts share besides their
//! settings: the sync engine program, the saved sign-ins and the right-click
//! menu. `Hub` is the set of accounts, and what the window talks to.

use crate::config::{self, Account, Settings};
use crate::core::{Core, Host, Shared};
use crate::engine::Engine;
use crate::mount::Drive;
use crate::newfolders::NewFolders;
use crate::platform;
use crate::providers;
use crate::rclone::{self, friendly_error, About, Rclone};
use crate::uploads::Uploads;
use crate::util::{clean_remote, file_name, new_id, norm_local, now_secs, paths_overlap, system_folder_reason, Cancel};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

pub const WINFSP_URL: &str = "https://winfsp.dev/rel/";
pub const RCLONE_URL: &str = "https://rclone.org";

/// How often the engine is looked at again on its own when it isn't ready,
/// or isn't the signed copy yet.
const ENGINE_RETRY: Duration = Duration::from_secs(5 * 60);
/// A copy the system refused to run is not run again on its own for this
/// long, because each try can pop up a Windows Security warning.
const BLOCKED_RETRY: Duration = Duration::from_secs(6 * 3600);
/// How often a newer signed engine is looked for.
const ENGINE_UPDATE_EVERY: u64 = 24 * 3600;
/// After the signed engine list could not be reached, how long the daily
/// update check waits before trying again.
const CHANNEL_RETRY: Duration = Duration::from_secs(3600);

/// Why the engine is being looked at.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum EngineCheck {
    /// When SyncHopper starts.
    Startup,
    /// Someone pressed Try again.
    Manual,
    /// The regular check while SyncHopper runs. Doesn't show "checking".
    Background,
}

enum EngineOutcome {
    Ready { loc: rclone::Located, signed: bool },
    Blocked(String),
    Failed(String),
}

/// What the accounts share besides their settings: the sync engine program
/// (finding it, keeping it current), the saved sign-ins and the right-click
/// menu.
pub struct Commons {
    pub shared: Arc<Shared>,
    /// Whether the Windows 11 right-click entry is in use. Worked out once.
    menu_modern: Mutex<Option<bool>>,
    download_base: String,
    /// Set in release builds: where the signed engine comes from.
    engine_channel: Mutex<Option<rclone::EngineChannel>>,
    /// Only one engine check at a time.
    engine_lock: tokio::sync::Mutex<()>,
    /// A copy the system refused to run: its checksum, when, and the error.
    blocked_file: Mutex<Option<(String, Instant, String)>>,
    /// When the signed engine list was last tried.
    channel_tried: Mutex<Option<Instant>>,
    /// Only one change to the saved sign-ins at a time.
    signin_lock: tokio::sync::Mutex<()>,
    /// The accounts, to tell when the engine comes back.
    apps: Mutex<Vec<Weak<App>>>,
}

/// One cloud account and everything that runs for it.
pub struct App {
    pub core: Arc<Core>,
    pub engine: Arc<Engine>,
    pub uploads: Arc<Uploads>,
    pub drive: Arc<Drive>,
    pub folders: Arc<NewFolders>,
    pub commons: Arc<Commons>,
    about: Mutex<Option<About>>,
    add_request: Mutex<Option<AddRequest>>,
    started: AtomicBool,
    /// A sign-in through the browser that is waiting for the user.
    browser: Mutex<Option<BrowserSignIn>>,
}

/// A sign-in on the service's own page, in the user's browser.
#[derive(Clone)]
struct BrowserSignIn {
    cancel: Cancel,
    /// The page to open, once the engine has said where it is.
    link: Option<String>,
}

/// What someone typed to sign in. Which parts are used depends on the
/// service: Koofr takes an email and an app password, Filen an email, a
/// password and an API key, and pCloud none of them (it signs in on its own
/// page in the browser).
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct SignIn {
    pub email: String,
    pub password: String,
    pub api_key: String,
}

/// A folder someone asked to sync from outside the main window, for example
/// with "Sync folder to Koofr" in File Explorer. The window picks it up and
/// opens the Add a folder dialog with it filled in.
#[derive(Clone, Debug, Serialize)]
pub struct AddRequest {
    pub local: String,
    /// Suggested folder in the account.
    pub remote: String,
    /// Shown in the dialog, for example that the account already has this folder.
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
    /// Top-level folders in the account to keep on this PC.
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

/// The file the key to the saved sign-ins is kept in, itself locked so only
/// this Windows user can read it.
const KEY_FILE: &str = "signin.key";

impl Commons {
    pub fn new(shared: Arc<Shared>, download_base: String) -> Arc<Commons> {
        Arc::new(Commons {
            shared,
            menu_modern: Mutex::new(None),
            download_base,
            engine_channel: Mutex::new(None),
            engine_lock: tokio::sync::Mutex::new(()),
            blocked_file: Mutex::new(None),
            channel_tried: Mutex::new(None),
            signin_lock: tokio::sync::Mutex::new(()),
            apps: Mutex::new(Vec::new()),
        })
    }

    /// Release builds get the engine only from the signed channel. Call
    /// before starting.
    pub fn use_engine_channel(&self, channel: rclone::EngineChannel) {
        *self.engine_channel.lock().unwrap() = Some(channel);
    }

    pub fn has_engine_channel(&self) -> bool {
        self.engine_channel.lock().unwrap().is_some()
    }

    fn register(&self, app: &Arc<App>) {
        let mut apps = self.apps.lock().unwrap();
        apps.retain(|a| a.strong_count() > 0);
        apps.push(Arc::downgrade(app));
    }

    /// The accounts that still exist.
    fn apps(&self) -> Vec<Arc<App>> {
        self.apps.lock().unwrap().iter().filter_map(|a| a.upgrade()).collect()
    }

    // ----- the sync engine program --------------------------------------

    /// Looks at the sync engine program: is it there, does it run, is there
    /// a newer one. Gets it when needed.
    pub async fn check_engine(&self, why: EngineCheck) {
        // Only one check at a time; a second one would only repeat it.
        let Ok(_guard) = self.engine_lock.try_lock() else { return };
        let before = self.shared.engine_info();
        if why != EngineCheck::Background {
            self.shared.set_engine(|e| {
                e.state = "checking".into();
                e.message.clear();
            });
        }
        rclone::tidy_engine_folder(&self.shared.data);
        let channel = self.engine_channel.lock().unwrap().clone();
        let outcome = match channel {
            Some(ch) => self.engine_signed(&ch, why).await,
            None => self.engine_unsigned(why).await,
        };
        match outcome {
            EngineOutcome::Ready { loc, signed } => {
                let was_ready = before.state == "ready" && before.exe.is_some();
                if !was_ready || before.version != loc.version || before.signed != signed {
                    let kind = if signed { "signed copy" } else { "unsigned copy" };
                    self.shared.log(&format!("Sync engine ready: rclone {} ({kind}) at {}", loc.version, loc.exe.display()));
                }
                self.shared.set_engine(|e| {
                    e.state = "ready".into();
                    e.version = loc.version.clone();
                    e.exe = Some(loc.exe.clone());
                    e.signed = signed;
                    e.percent = 0;
                    e.message.clear();
                });
                self.shared.engine_runs_again();
                if !was_ready {
                    for app in self.apps() {
                        if app.started.load(Ordering::SeqCst) {
                            app.engine.engine_back();
                            app.drive.apply().await;
                        }
                    }
                }
            }
            EngineOutcome::Blocked(raw) => self.shared.engine_blocked(&raw),
            EngineOutcome::Failed(msg) => {
                self.shared.log(&format!("Sync engine not available: {msg}"));
                self.shared.set_engine(|e| {
                    e.state = "error".into();
                    e.exe = None;
                    e.signed = false;
                    e.percent = 0;
                    e.message = msg.clone();
                });
            }
        }
    }

    /// Release builds: only a copy from the signed channel is wanted. A
    /// copy from before SyncHopper used signed copies is replaced, whatever
    /// the update setting says. It is only used while no signed copy can be
    /// had, and only if this PC lets it run.
    async fn engine_signed(&self, ch: &rclone::EngineChannel, why: EngineCheck) -> EngineOutcome {
        let data = self.shared.data.clone();
        // Reads the whole program to check it, so off the async threads.
        let installed = tokio::task::spawn_blocking(move || rclone::installed_source(&data)).await.ok().flatten();
        let mut signed = installed.as_ref().is_some_and(|s| s.source == "signed");
        let s = self.shared.general();
        let due = now_secs().saturating_sub(s.engine_checked_at) > ENGINE_UPDATE_EVERY;
        let mut problem: Option<String> = None;
        if !signed || (s.auto_update_engine && due) {
            *self.channel_tried.lock().unwrap() = Some(Instant::now());
            match self.fetch_signed(ch, installed.as_ref(), why != EngineCheck::Background).await {
                // Either it was already the current signed copy or one was
                // just put in place.
                Ok(()) => signed = true,
                Err(e) => {
                    self.shared.log(&format!("Signed sync engine: {e}"));
                    problem = Some(e);
                }
            }
        }
        match self.probe_engine(why).await {
            rclone::Found::Ready(loc) => EngineOutcome::Ready { loc, signed },
            rclone::Found::Blocked(raw) => EngineOutcome::Blocked(raw),
            rclone::Found::Missing => EngineOutcome::Failed(match problem {
                Some(e) => e,
                None => "The sync engine was downloaded but would not start.".into(),
            }),
        }
    }

    /// Gets the current signed engine if this PC doesn't have it yet.
    async fn fetch_signed(
        &self,
        ch: &rclone::EngineChannel,
        installed: Option<&rclone::EngineSource>,
        show: bool,
    ) -> Result<(), String> {
        let manifest = rclone::channel_manifest(ch).await?;
        let file = manifest
            .files
            .get(&rclone::platform_key())
            .cloned()
            .ok_or_else(|| "No signed sync engine is published for this kind of PC yet.".to_string())?;
        let have = installed.filter(|i| i.source == "signed");
        let current = have.is_some_and(|i| i.sha256.eq_ignore_ascii_case(&file.sha256));
        // Never go back to an older one.
        let older = have.is_some_and(|i| rclone::version_triple(&manifest.version) < rclone::version_triple(&i.version));
        if current || older {
            self.shared.change(|s| s.general.engine_checked_at = now_secs());
            return Ok(());
        }
        match have {
            Some(i) => self.shared.log(&format!("Updating the sync engine from {} to {}", i.version, manifest.version)),
            None => self.shared.log(&format!("Getting the signed sync engine (rclone {})", manifest.version)),
        }
        let license = match &manifest.license {
            Some(l) => Some(rclone::channel_file(ch, l, |_| {}).await?),
            None => None,
        };
        if show {
            self.shared.set_engine(|e| {
                e.state = "downloading".into();
                e.percent = 0;
            });
        }
        let core = self.shared.clone();
        let bytes = rclone::channel_file(ch, &file, move |pct| {
            if show {
                core.set_engine(|e| e.percent = pct);
            }
        })
        .await?;
        let data = self.shared.data.clone();
        let version = manifest.version.clone();
        tokio::task::spawn_blocking(move || {
            rclone::install_engine(&data, &bytes, "signed", &version)?;
            if let Some(l) = license {
                rclone::install_license(&data, &l)?;
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| e.to_string())??;
        self.shared.change(|s| s.general.engine_checked_at = now_secs());
        Ok(())
    }

    /// Builds made on a PC (no update key): rclone straight from rclone.org.
    async fn engine_unsigned(&self, why: EngineCheck) -> EngineOutcome {
        let mut found = self.probe_engine(why).await;
        if let rclone::Found::Ready(loc) = &found {
            // Keep it current, at most one check a day.
            let s = self.shared.general();
            let due = now_secs().saturating_sub(s.engine_checked_at) > ENGINE_UPDATE_EVERY;
            if s.auto_update_engine && due && !self.shared.test_mode {
                self.shared.change(|s| s.general.engine_checked_at = now_secs());
                if let Ok(latest) = rclone::latest_version(&self.download_base).await {
                    if rclone::version_triple(&latest) > rclone::version_triple(&loc.version) {
                        self.shared.log(&format!("Updating the sync engine from {} to {latest}", loc.version));
                        if self.download_unsigned(why != EngineCheck::Background).await.is_ok() {
                            found = self.probe_engine(why).await;
                        }
                    }
                }
            }
        }
        match found {
            rclone::Found::Ready(loc) => EngineOutcome::Ready { loc, signed: false },
            rclone::Found::Blocked(raw) => EngineOutcome::Blocked(raw),
            rclone::Found::Missing => match self.download_unsigned(why != EngineCheck::Background).await {
                Err(e) => EngineOutcome::Failed(e),
                Ok(()) => match self.probe_engine(why).await {
                    rclone::Found::Ready(loc) => EngineOutcome::Ready { loc, signed: false },
                    rclone::Found::Blocked(raw) => EngineOutcome::Blocked(raw),
                    rclone::Found::Missing => EngineOutcome::Failed("The sync engine was downloaded but would not start.".into()),
                },
            },
        }
    }

    async fn download_unsigned(&self, show: bool) -> Result<(), String> {
        if show {
            self.shared.set_engine(|e| {
                e.state = "downloading".into();
                e.percent = 0;
            });
        }
        let core = self.shared.clone();
        let result = rclone::download(&self.shared.data, &self.download_base, move |pct| {
            if show {
                core.set_engine(|e| e.percent = pct);
            }
        })
        .await;
        if let Err(e) = &result {
            self.shared.log(&format!("Sync engine download failed: {e}"));
        }
        result.map(|_| ())
    }

    /// Runs the installed engine to see whether it works. A copy the system
    /// refused to run isn't tried again on its own for a while, since each
    /// try can bring up a Windows Security warning; Try again always tries.
    async fn probe_engine(&self, why: EngineCheck) -> rclone::Found {
        let exe = rclone::managed_path(&self.shared.data);
        if !exe.is_file() {
            return rclone::Found::Missing;
        }
        let hash = |exe: PathBuf| async move { tokio::task::spawn_blocking(move || rclone::file_sha256(&exe)).await.ok().flatten().unwrap_or_default() };
        let known = self.blocked_file.lock().unwrap().clone();
        if let Some((sha, when, raw)) = known {
            if why != EngineCheck::Manual && when.elapsed() < BLOCKED_RETRY && hash(exe.clone()).await == sha {
                return rclone::Found::Blocked(raw);
            }
        }
        let found = rclone::find(&self.shared.data).await;
        let blocked = match &found {
            rclone::Found::Blocked(raw) => Some((hash(exe).await, Instant::now(), raw.clone())),
            _ => None,
        };
        *self.blocked_file.lock().unwrap() = blocked;
        found
    }

    /// Whether the regular check has something to do.
    fn engine_check_due(&self) -> bool {
        let info = self.shared.engine_info();
        let channel = self.engine_channel.lock().unwrap().is_some();
        match info.state.as_str() {
            "error" | "blocked" => true,
            "ready" if channel => {
                let s = self.shared.general();
                let rested = self.channel_tried.lock().unwrap().map(|t| t.elapsed() >= CHANNEL_RETRY).unwrap_or(true);
                let update_due = s.auto_update_engine && now_secs().saturating_sub(s.engine_checked_at) > ENGINE_UPDATE_EVERY;
                !info.signed || (update_due && rested)
            }
            _ => false,
        }
    }

    // ----- the right-click menu -----------------------------------------

    /// Puts File Explorer's right-click entries in line with the settings.
    /// On Windows 11 with a signed release the entry is in the main menu;
    /// otherwise the classic entries are used.
    pub async fn apply_menu(&self) -> Result<(), String> {
        let Some(exe) = self.shared.host.exe_path() else { return Ok(()) };
        if !cfg!(windows) || self.shared.test_mode {
            return Ok(());
        }
        let ready: Vec<Account> = self.shared.accounts().into_iter().filter(|a| a.setup_complete).collect();
        let wanted = !ready.is_empty() && self.shared.general().feature_context_menu;
        let add_folder = wanted && ready.iter().any(|a| a.feature_sync);
        let labels = menu_labels(&ready);
        if let Err(e) = platform::set_menu_flags(wanted, add_folder, &exe, &labels).await {
            self.shared.log(&format!("Could not save the right-click menu settings: {e}"));
        }
        let cached = *self.menu_modern.lock().unwrap();
        let modern = match cached {
            Some(m) => m,
            None => {
                let m = match platform::ensure_menu_package(&exe, env!("CARGO_PKG_VERSION")).await {
                    Ok(m) => m,
                    Err(e) => {
                        self.shared.log(&format!("The Windows 11 right-click entry could not be set up, using the classic one: {e}"));
                        false
                    }
                };
                *self.menu_modern.lock().unwrap() = Some(m);
                m
            }
        };
        platform::set_legacy_menu(wanted && !modern, add_folder, &exe, &labels).await
    }

    // ----- saved sign-ins -----------------------------------------------

    /// True when this PC can lock a key so only the signed-in Windows user
    /// can read it back. The screens say how sign-ins are kept from this.
    pub fn can_lock(&self) -> bool {
        const PROBE: &[u8] = b"synchopper";
        cfg!(windows) && platform::protect(PROBE).and_then(|locked| platform::unprotect(&locked)).as_deref() == Some(PROBE)
    }

    /// The key the saved sign-ins are locked with, read from its file.
    fn load_key(&self) -> Option<String> {
        use base64::Engine as _;
        let text = std::fs::read_to_string(self.shared.data.join(KEY_FILE)).ok()?;
        let locked = base64::engine::general_purpose::STANDARD.decode(text.trim()).ok()?;
        let key = String::from_utf8(platform::unprotect(&locked)?).ok()?;
        (key.len() >= 32 && key.chars().all(|c| c.is_ascii_alphanumeric())).then_some(key)
    }

    /// Makes a new key and saves it so only this Windows user can read it.
    /// None when Windows can't lock it, in which case the sign-ins stay as
    /// the engine writes them (scrambled, as in earlier versions).
    fn make_key(&self) -> Option<String> {
        use base64::Engine as _;
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).ok()?;
        let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let locked = platform::protect(key.as_bytes())?;
        let text = base64::engine::general_purpose::STANDARD.encode(locked);
        std::fs::write(self.shared.data.join(KEY_FILE), text).ok()?;
        // Only trust it once it reads back.
        (self.load_key().as_deref() == Some(key.as_str())).then_some(key)
    }

    /// Makes sure the saved sign-ins are locked with a key only this
    /// Windows user can read. Called once at startup, before any account
    /// runs. Sign-ins saved by version 0.4 and earlier are locked here.
    pub async fn secure_signins(&self) {
        let _guard = self.signin_lock.lock().await;
        let path = self.shared.config_path();
        let key = self.load_key();
        if rclone::config_locked(&path) {
            if key.is_none() {
                self.shared.log("The saved sign-ins can't be read on this PC (their key is missing or was made on another PC). Each account needs to be reconnected.");
            }
            self.shared.set_config_pass(key);
            return;
        }
        if !path.is_file() {
            // Nothing saved yet. The first sign-in is locked when it is saved.
            self.shared.set_config_pass(None);
            return;
        }
        let Some(key) = key.or_else(|| self.make_key()) else {
            self.shared.log("Windows could not lock the key for the saved sign-ins, so they stay as they were.");
            return;
        };
        let Ok(plain) = self.shared.rclone(Vec::new()) else { return };
        let text = match plain.signins().await {
            Ok(t) => t,
            Err(e) => return self.shared.log(&format!("Could not read the saved sign-ins to lock them: {e}")),
        };
        let locking = Rclone { pass: Some(key.clone()), ..plain };
        match locking.save_signins(&text).await {
            Ok(()) => {
                self.shared.set_config_pass(Some(key));
                self.shared.log("The saved sign-ins are now locked with a key only this Windows user can read.");
            }
            Err(e) => self.shared.log(&format!("Could not lock the saved sign-ins, so they stay as they were: {e}")),
        }
    }

    /// Adds, replaces or (with `lines` None) removes one account's saved
    /// sign-in. `name` is the account's name in the engine's settings.
    pub async fn save_signin(&self, name: &str, lines: Option<&str>) -> Result<(), String> {
        let _guard = self.signin_lock.lock().await;
        let mut rc = self.shared.rclone(Vec::new())?;
        let current = match rc.signins().await {
            Ok(text) => text,
            Err(e) if rclone::is_locked_signins(&e) => {
                // The old file can't be read on this PC any more, so the
                // other accounts in it are lost either way. Start a new one.
                self.shared.log("Starting a new saved sign-in file, because the old one can't be read on this PC.");
                self.shared.set_config_pass(None);
                rc.pass = None;
                String::new()
            }
            Err(e) => return Err(e),
        };
        if rc.pass.is_none() && lines.is_some() {
            if let Some(key) = self.load_key().or_else(|| self.make_key()) {
                self.shared.set_config_pass(Some(key.clone()));
                rc.pass = Some(key);
            }
        }
        rc.save_signins(&rclone::with_section(&current, name, lines)).await
    }
}

impl App {
    /// An app that stands on its own with a single account, the way the
    /// whole of SyncHopper worked before it had accounts. Tests are built
    /// on this; the real window uses `Hub`.
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
        let commons = Commons::new(core.shared.clone(), download_base);
        App::assemble(core, commons)
    }

    /// Puts together everything that runs for one account.
    fn assemble(core: Arc<Core>, commons: Arc<Commons>) -> Arc<App> {
        let engine = Engine::new(core.clone());
        let app = Arc::new(App {
            folders: NewFolders::new(core.clone(), engine.clone()),
            engine,
            uploads: Uploads::new(core.clone()),
            drive: Drive::new(core.clone()),
            core,
            commons: commons.clone(),
            about: Mutex::new(None),
            add_request: Mutex::new(None),
            started: AtomicBool::new(false),
            browser: Mutex::new(None),
        });
        commons.register(&app);
        app
    }

    pub fn id(&self) -> &str {
        &self.core.account
    }

    /// Release builds get the engine only from the signed channel. Call
    /// before starting.
    pub fn use_engine_channel(&self, channel: rclone::EngineChannel) {
        self.commons.use_engine_channel(channel);
    }

    /// Makes sure the sync engine is ready, getting it when needed.
    pub async fn ensure_engine(self: &Arc<Self>) {
        self.commons.check_engine(EngineCheck::Startup).await;
    }

    pub async fn check_engine(self: &Arc<Self>, why: EngineCheck) {
        self.commons.check_engine(why).await;
    }

    pub async fn apply_menu(&self) -> Result<(), String> {
        self.commons.apply_menu().await
    }

    /// Everything the screens need about this account, in one piece.
    pub fn snapshot(&self) -> serde_json::Value {
        let s = self.core.settings();
        let about = self.about.lock().unwrap().clone();
        let mut settings = serde_json::to_value(&s).unwrap_or_else(|_| json!({}));
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("pairs");
            obj.remove("base_known");
        }
        let browser = self.browser.lock().unwrap().clone();
        json!({
            "id": self.core.account,
            "provider": self.core.provider.id,
            "cloud": self.core.cloud(),
            "name": self.core.label(),
            "version": env!("CARGO_PKG_VERSION"),
            "windows": cfg!(windows),
            "test_mode": self.core.test_mode,
            "setup_complete": s.setup_complete,
            "account": { "email": s.account_email, "used": about.as_ref().and_then(|a| a.used), "total": about.as_ref().and_then(|a| a.total) },
            "signing_in": browser.map(|b| json!({ "link": b.link })),
            "engine": self.core.engine_info(),
            "engine_channel": self.commons.has_engine_channel(),
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
        } else if matches!(self.core.engine_info().state.as_str(), "blocked" | "error") {
            // The sync engine can't run, so nothing syncs.
            "SyncHopper: can't sync right now".into()
        } else if self.engine.has_problems() {
            "SyncHopper: needs attention".into()
        } else if self.engine.waiting_for_koofr() {
            format!("SyncHopper: waiting for {}", self.core.cloud())
        } else if self.engine.waiting_for_drive() {
            "SyncHopper: waiting for a drive".into()
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

    // ----- startup -------------------------------------------------------

    /// Starts syncing, the drive and the Explorer entry. Safe to call again.
    pub async fn start_services(self: &Arc<Self>) {
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

    /// Adds or removes the account's entry in File Explorer's sidebar to
    /// match the settings. It points at the folder that holds the account's
    /// synced folders, and carries the account's name.
    pub async fn apply_sidebar(&self) -> Result<(), String> {
        let s = self.core.settings();
        let Some(exe) = self.core.host.exe_path() else { return Ok(()) };
        // An account without a place among the entries has none to change.
        let Some(slot) = self.core.shared.account(&self.core.account).and_then(|a| a.sidebar_slot) else { return Ok(()) };
        let wanted = s.setup_complete && s.feature_sync && s.feature_sidebar && !s.base_folder.trim().is_empty();
        if wanted {
            let folder = PathBuf::from(s.base_folder.trim());
            std::fs::create_dir_all(&folder).map_err(|e| format!("Could not create {}: {e}", folder.display()))?;
            platform::set_sidebar(slot, &self.core.label(), Some(&folder), &exe).await
        } else {
            platform::set_sidebar(slot, "", None, &exe).await
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

    /// Checks an email and app password and saves them when they work. The
    /// short form of `sign_in` for services that sign in that way.
    pub async fn connect(&self, email: &str, password: &str) -> Result<About, String> {
        self.sign_in(&SignIn { email: email.to_string(), password: password.to_string(), ..Default::default() }).await
    }

    /// Signs the account in the way its service does, asks the service
    /// whether it worked, and only then saves the sign-in. Nothing is saved
    /// when it didn't.
    pub async fn sign_in(&self, f: &SignIn) -> Result<About, String> {
        let p = self.core.provider;
        let cloud = p.name;
        let email = f.email.trim().to_string();
        let rc = self.core.rclone()?;
        if self.core.test_mode {
            let about = rc.about(&self.core.remote("")).await?;
            *self.about.lock().unwrap() = Some(about.clone());
            self.core.update(|s| s.account_email = email);
            return Ok(about);
        }
        if p.sign_in != "browser" {
            if email.is_empty() || !email.contains('@') || email.chars().any(|c| c.is_control() || c == '[' || c == ']') {
                return Err(format!("Enter the email address you use for {cloud}."));
            }
            if f.password.trim().is_empty() {
                return Err(match p.sign_in {
                    "password" => format!("Enter a {cloud} app password."),
                    _ => format!("Enter your {cloud} password."),
                });
            }
            if p.sign_in == "api_key" && f.api_key.split_whitespace().next().is_none() {
                return Err(format!("Enter your {cloud} API key."));
            }
        }
        let name = self.core.shared.account(&self.core.account).map(|a| a.remote).filter(|r| !r.is_empty()).unwrap_or_else(|| self.core.account.clone());
        // The sign-in is tried out in a file of its own first, and only
        // goes in with the saved sign-ins once the service accepts it.
        let trial = self.core.data.join(format!("signin-{}.conf", self.core.account));
        let _ = std::fs::remove_file(&trial);
        let probe = Rclone { config: trial.clone(), pass: None, extra: Vec::new(), ..rc.clone() };
        let result = self.try_sign_in(&rc, &probe, &trial, &name, &email, f).await;
        let _ = std::fs::remove_file(&trial);
        let (about, lines) = result?;
        self.commons.save_signin(&name, Some(&lines)).await?;
        *self.about.lock().unwrap() = Some(about.clone());
        if p.sign_in != "browser" {
            self.core.update(|s| s.account_email = email);
        }
        self.core.log(&format!("Connected to {cloud}"));
        Ok(about)
    }

    /// Builds the sign-in in the trial file and asks the service. Returns
    /// the account's space and the lines to save.
    async fn try_sign_in(&self, rc: &Rclone, probe: &Rclone, trial: &Path, name: &str, email: &str, f: &SignIn) -> Result<(About, String), String> {
        let p = self.core.provider;
        let cloud = p.name;
        let cant_save = |e: std::io::Error| format!("Could not save the connection: {e}");
        match p.sign_in {
            "browser" => self.browser_sign_in(probe, name).await?,
            "api_key" => {
                let password = rc.obscure(f.password.trim()).await?;
                // A pasted key often carries a line break or spaces.
                let key: String = f.api_key.split_whitespace().collect();
                let key = rc.obscure(&key).await?;
                std::fs::write(trial, format!("[{name}]\ntype = {}\nemail = {email}\npassword = {password}\napi_key = {key}\n", p.kind)).map_err(cant_save)?;
            }
            _ => {
                let password = rc.obscure(f.password.trim()).await?;
                std::fs::write(trial, format!("[{name}]\ntype = {}\nprovider = koofr\nuser = {email}\npassword = {password}\n", p.kind)).map_err(cant_save)?;
            }
        }
        match probe.about(&format!("{name}:")).await {
            Ok(about) => {
                let text = std::fs::read_to_string(trial).map_err(cant_save)?;
                let lines = rclone::section(&text, name).ok_or_else(|| "The sync engine did not keep the connection.".to_string())?;
                Ok((about, lines))
            }
            Err(raw) => {
                self.core.log(&format!("Connection test failed: {raw}"));
                let friendly = friendly_error(p, &raw);
                Err(if friendly.contains("did not accept") {
                    match p.sign_in {
                        "password" => format!("{cloud} did not accept that email and app password. Make sure it is an app password from {cloud}'s password page, not your normal {cloud} password."),
                        "api_key" => format!("{cloud} did not accept that email, password and API key. The API key comes from the Filen command-line tool, and changes when you change your {cloud} password."),
                        _ => format!("{cloud} did not accept the sign-in. Try again."),
                    }
                } else if friendly.starts_with("Could not reach") {
                    format!("Could not reach {cloud}. Details: {}", raw.chars().take(200).collect::<String>())
                } else {
                    format!("The connection test failed. Details: {}", raw.chars().take(300).collect::<String>())
                })
            }
        }
    }

    /// Signs in on the service's own page: the engine opens the browser and
    /// waits there for the answer. The window shows a link to the same page
    /// in case the browser did not open, and can cancel the wait.
    async fn browser_sign_in(&self, probe: &Rclone, name: &str) -> Result<(), String> {
        let cloud = self.core.cloud();
        let cancel = Cancel::default();
        {
            let mut slot = self.browser.lock().unwrap();
            if slot.is_some() {
                return Err(format!("A {cloud} sign-in is already waiting in your browser."));
            }
            *slot = Some(BrowserSignIn { cancel: cancel.clone(), link: None });
        }
        self.core.host.state_changed();
        let result = tokio::time::timeout(
            BROWSER_SIGN_IN_WAIT,
            probe.browser_sign_in(name, self.core.provider.kind, &cancel, |link| {
                if let Some(b) = self.browser.lock().unwrap().as_mut() {
                    b.link = Some(link.to_string());
                }
                self.core.host.state_changed();
            }),
        )
        .await;
        if result.is_err() {
            // Nobody answered. Stop the engine's wait too.
            cancel.cancel();
        }
        *self.browser.lock().unwrap() = None;
        self.core.host.state_changed();
        match result {
            Err(_) => Err(format!("The {cloud} sign-in was not finished in time. Try again.")),
            Ok(Err(e)) if e == rclone::SIGN_IN_CANCELLED => Err("The sign-in was cancelled.".into()),
            Ok(Err(e)) => {
                self.core.log(&format!("Browser sign-in failed: {e}"));
                Err(if e.contains("address already in use") || e.contains("Only one usage of each socket address") {
                    "Another sign-in is still waiting in your browser. Finish or close it, then try again.".to_string()
                } else {
                    format!("The {cloud} sign-in did not work. Details: {}", e.chars().take(300).collect::<String>())
                })
            }
            Ok(Ok(())) => Ok(()),
        }
    }

    /// Stops waiting for a sign-in in the browser.
    pub fn cancel_sign_in(&self) {
        if let Some(b) = self.browser.lock().unwrap().as_ref() {
            b.cancel.cancel();
        }
    }

    /// Signs out: stops everything this account runs and forgets its
    /// sign-in and its folders. Files stay where they are on this PC and in
    /// the account. The account's entry in the settings is left for `Hub`
    /// to take out.
    pub async fn disconnect(self: &Arc<Self>) {
        let cloud = self.core.cloud();
        self.cancel_sign_in();
        self.engine.stop_all();
        self.uploads.cancel_all();
        self.drive.stop();
        self.engine.wait_idle().await;
        let ids: Vec<String> = self.core.settings().pairs.iter().map(|p| p.id.clone()).collect();
        for id in &ids {
            self.engine.remove_pair(id);
        }
        let account = self.core.shared.account(&self.core.account);
        let name = account.as_ref().map(|a| a.remote.clone()).filter(|r| !r.is_empty()).unwrap_or_else(|| self.core.account.clone());
        if let Err(e) = self.commons.save_signin(&name, None).await {
            self.core.log(&format!("Could not remove the saved sign-in: {e}"));
        }
        for id in &ids {
            let _ = std::fs::remove_dir_all(self.core.pair_dir(id));
        }
        *self.about.lock().unwrap() = None;
        if let (Some(exe), Some(slot)) = (self.core.host.exe_path(), account.and_then(|a| a.sidebar_slot)) {
            let _ = platform::set_sidebar(slot, "", None, &exe).await;
        }
        self.core.update(|s| {
            s.setup_complete = false;
            s.account_email.clear();
            s.pairs.clear();
            s.base_known.clear();
            s.base_known_ready = false;
        });
        self.started.store(false, Ordering::SeqCst);
        let _ = self.apply_menu().await;
        self.engine.rewatch();
        self.folders.rewatch();
        self.core.log(&format!("Disconnected from {cloud}"));
    }

    // ----- setup ---------------------------------------------------------

    /// Applies the choices from the setup screens. Returns notes about
    /// anything that could not be done exactly as asked.
    pub async fn finish_setup(self: &Arc<Self>, c: SetupChoices) -> Result<Vec<String>, String> {
        let signed_in = !self.core.settings().account_email.is_empty() || self.about.lock().unwrap().is_some();
        if !signed_in {
            return Err(format!("Connect your {} account first.", self.core.cloud()));
        }
        if self.core.engine_info().exe.is_none() {
            return Err("The sync engine is not ready yet.".into());
        }
        let base = c.base_folder.trim().to_string();
        if c.feature_sync && !c.folders.is_empty() && (base.is_empty() || !Path::new(&base).is_absolute()) {
            return Err("Pick where to keep your folders on this PC.".into());
        }
        if !base.is_empty() {
            // Each account keeps its folders in a place of its own.
            let nb = norm_local(&base);
            let sep = std::path::MAIN_SEPARATOR;
            for other in self.core.shared.accounts().into_iter().filter(|a| a.id != self.core.account) {
                let theirs = other.base_folder.trim();
                let clash = (!theirs.is_empty() && paths_overlap(&nb, &norm_local(theirs), sep)) || other.pairs.iter().any(|p| paths_overlap(&nb, &norm_local(&p.local), sep));
                if clash {
                    return Err(format!("That folder overlaps the folders {} keeps on this PC. Pick a different place for this account.", other.name));
                }
            }
        }
        let mut notes = Vec::new();
        self.core.update(|s| {
            s.feature_sync = c.feature_sync;
            s.feature_drive = c.feature_drive;
            s.autostart = c.autostart;
            if !base.is_empty() {
                if norm_local(&s.base_folder) != norm_local(&base) {
                    // A different folder: what is in it now is not new.
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
        const EDITABLE: [&str; 21] = [
            "feature_sync", "feature_drive", "drive_letter", "feature_context_menu", "feature_sidebar", "autostart",
            "check_interval_min", "transfers", "max_delete_percent", "auto_add_folders", "use_http2", "limit_requests",
            "upload_limit_mbps", "download_limit_mbps", "conflict_policy", "ignore_patterns", "pause_on_battery",
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
        // A service without a request limit or an HTTP/2 choice keeps what
        // it started with, whatever was asked for.
        let p = self.core.provider;
        if (p.request_limit.is_none() && new.limit_requests) || (!p.http2_choice && new.use_http2 != p.http2_default) {
            self.core.update(|s| {
                if p.request_limit.is_none() {
                    s.limit_requests = false;
                }
                if !p.http2_choice {
                    s.use_http2 = p.http2_default;
                }
            });
        }
        let new = self.core.settings();
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
        // The drive is one long-running engine, so it has to reconnect to pick
        // up a change to how it talks to the service.
        if old.feature_drive != new.feature_drive
            || old.drive_letter != new.drive_letter
            || old.use_http2 != new.use_http2
            || old.limit_requests != new.limit_requests
        {
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

    /// Puts the general settings back to their defaults: limits, conflicts,
    /// ignored files, pausing, notifications and updates. What belongs to
    /// one account, and the choices about starting with Windows and the
    /// right-click menu, are left alone.
    pub async fn restore_defaults(self: &Arc<Self>) -> Result<(), String> {
        let mut defaults = self.core.settings();
        defaults.restore_general_defaults();
        let patch = serde_json::to_value(&defaults).map_err(|e| e.to_string())?;
        let keep = [
            "max_delete_percent", "upload_limit_mbps", "download_limit_mbps", "conflict_policy", "ignore_patterns",
            "pause_on_battery", "pause_on_metered", "notify_level", "auto_update_engine", "auto_update_app",
        ];
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
            friendly_error(self.core.provider, &e)
        })
    }

    pub async fn make_remote_dir(&self, path: &str) -> Result<(), String> {
        let path = clean_remote(path);
        if path.is_empty() {
            return Err("Type a name for the new folder.".into());
        }
        let rc = self.core.rclone()?;
        rc.mkdir(&self.core.remote(&path)).await.map_err(|e| friendly_error(self.core.provider, &e))
    }

    /// What a first sync of this pairing would involve.
    pub async fn preview(&self, local: &str, remote: &str) -> Result<Preview, String> {
        let rc = self.core.rclone()?;
        let (remote_count, remote_bytes) = match rc.size(&self.core.remote(remote)).await {
            Ok(v) => v,
            Err(e) if e.to_lowercase().contains("not found") => (0, 0),
            Err(e) => return Err(friendly_error(self.core.provider, &e)),
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
        let cloud = self.core.cloud();
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
            Some("Syncing folders is turned off for this account. Turn it on in the account's settings first.".to_string())
        } else if !base.is_empty() && norm_local(base) == nl {
            Some(if s.auto_add_folders {
                format!("This is your {cloud} folder, where your synced folders live. New folders you put in it sync by themselves.")
            } else {
                format!("This is your {cloud} folder, where your synced folders live. Pick a folder inside it to sync.")
            })
        } else if let Some(p) = s.pairs.iter().find(|p| holds(&norm_local(&p.local), &nl)) {
            Some(if norm_local(&p.local) == nl {
                format!("\"{name}\" is already synced with {cloud}.")
            } else {
                format!("This folder is inside \"{}\", which is already synced with {cloud}.", file_name(&p.local))
            })
        } else if let Some(p) = s.pairs.iter().find(|p| holds(&nl, &norm_local(&p.local))) {
            Some(format!("This folder holds \"{}\", which is already synced with {cloud}. Pick a folder that doesn't overlap it.", file_name(&p.local)))
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

    /// Plain-text details about the app itself for a support request.
    pub fn diagnostics_head(shared: &Shared) -> String {
        let e = shared.engine_info();
        let mut out = String::new();
        out.push_str(&format!("SyncHopper {}\n", env!("CARGO_PKG_VERSION")));
        out.push_str(&format!("System: {} {}\n", std::env::consts::OS, std::env::consts::ARCH));
        out.push_str(&format!("Sync engine: rclone {} ({})\n", e.version, e.state));
        out.push_str(&format!("Saved sign-ins locked: {}\n", if rclone::config_locked(&shared.config_path()) { "yes" } else { "no" }));
        out
    }

    /// Plain-text details for a support request. No passwords, and the
    /// email address is masked.
    pub fn diagnostics(&self) -> String {
        let mut out = App::diagnostics_head(&self.core.shared);
        out.push_str(&self.diagnostics_account());
        out.push_str(&format!("Log file: {}\n", self.core.log_path().display()));
        out
    }

    /// The part of the details that is about this account.
    pub fn diagnostics_account(&self) -> String {
        let s = self.core.settings();
        let d = self.drive.view();
        let cloud = self.core.cloud();
        let mut out = String::new();
        out.push_str(&format!("Account: {cloud} {}\n", mask_email(&s.account_email)));
        out.push_str(&format!(
            "Features: sync {}, drive {} ({}), right-click {}, sidebar {}, start with Windows {}, app updates {}\n",
            s.feature_sync, s.feature_drive, d.state, s.feature_context_menu, s.feature_sidebar, s.autostart, s.auto_update_app
        ));
        out.push_str(&format!(
            "Sync: every {} min, {} at a time, HTTP/2 {}, request limit {}, limits up {} down {} MB/s, conflicts {}, delete check {}%\n",
            s.check_interval_min,
            s.transfers,
            if s.use_http2 { "on" } else { "off" },
            if s.limit_requests { "on" } else { "off" },
            s.upload_limit_mbps,
            s.download_limit_mbps,
            s.conflict_policy,
            s.max_delete_percent
        ));
        out.push_str(&format!("Ignored: {}\n", s.ignore_patterns.join(", ")));
        let sync = self.engine.sync_view();
        out.push_str(&format!("Paused: {} {}\n", sync.paused_all, sync.auto_pause.unwrap_or_default()));
        for p in self.engine.pair_views() {
            out.push_str(&format!(
                "Folder: {} <-> {cloud}:/{} | {:?}{} | last synced {} | left out {} | ignored {} | {}{}\n",
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
        out
    }
}

/// How the two right-click items are worded: with one account they name its
/// service, with more SyncHopper asks which account.
fn menu_labels(ready: &[Account]) -> platform::MenuLabels {
    match ready {
        [only] => {
            let cloud = providers::get(&only.provider).name;
            platform::MenuLabels { send: format!("Send to {cloud}"), add_folder: format!("Sync folder to {cloud}") }
        }
        _ => platform::MenuLabels { send: "Send to...".into(), add_folder: "Sync folder to...".into() },
    }
}

/// How long a sign-in in the browser is waited for.
const BROWSER_SIGN_IN_WAIT: Duration = Duration::from_secs(10 * 60);


/// Something asked for from outside the window (File Explorer's right-click
/// menu) that needs an account picked before it can go ahead.
#[derive(Clone, Debug, Serialize)]
pub struct Pending {
    /// "upload" so far.
    pub kind: String,
    pub paths: Vec<String>,
}

/// Every account, and what the window talks to.
pub struct Hub {
    pub shared: Arc<Shared>,
    pub commons: Arc<Commons>,
    apps: Mutex<Vec<Arc<App>>>,
    /// Development builds can point the first account at a plain folder
    /// instead of a cloud service, so everything can be tried without one.
    /// Later accounts get folders next to it.
    test_remote: Option<String>,
    pending: Mutex<Option<Pending>>,
    /// Whether sign-ins saved on this PC are locked to the Windows user.
    lock_signins: bool,
}

impl Hub {
    pub fn new(data: PathBuf, test_remote: Option<String>, host: Arc<dyn Host>) -> Arc<Hub> {
        let shared = Shared::open(data, test_remote.is_some(), host);
        let commons = Commons::new(shared.clone(), rclone::DOWNLOAD_BASE.to_string());
        let lock_signins = commons.can_lock();
        let hub = Arc::new(Hub { shared: shared.clone(), commons, apps: Mutex::new(Vec::new()), test_remote, pending: Mutex::new(None), lock_signins });
        let apps: Vec<Arc<App>> = shared.accounts().iter().map(|a| hub.assemble(a)).collect();
        *hub.apps.lock().unwrap() = apps;
        hub
    }

    fn assemble(&self, account: &Account) -> Arc<App> {
        let root = self.test_remote.as_ref().map(|base| {
            if account.remote == config::FIRST_ACCOUNT {
                base.clone()
            } else {
                let folder = format!("{}-{}", base.trim_end_matches(['/', '\\']), account.remote);
                let _ = std::fs::create_dir_all(&folder);
                folder
            }
        });
        App::assemble(Core::for_account(self.shared.clone(), &account.id, root), self.commons.clone())
    }

    /// Every account, in the order they were added.
    pub fn apps(&self) -> Vec<Arc<App>> {
        self.apps.lock().unwrap().clone()
    }

    /// The accounts whose setup is finished.
    pub fn ready(&self) -> Vec<Arc<App>> {
        self.apps().into_iter().filter(|a| a.core.settings().setup_complete).collect()
    }

    pub fn app(&self, id: &str) -> Result<Arc<App>, String> {
        self.apps().into_iter().find(|a| a.id() == id).ok_or_else(|| "That account is no longer connected.".to_string())
    }

    /// Called once when the app opens.
    pub async fn start(self: &Arc<Self>) {
        self.shared.note_runtime();
        self.shared.log(&format!("SyncHopper {} starting", env!("CARGO_PKG_VERSION")));
        platform::kill_orphans(&self.shared.config_path()).await;
        self.commons.check_engine(EngineCheck::Startup).await;
        // Keeps looking after the engine while SyncHopper runs: tries again
        // when it could not be downloaded (for example the PC was offline)
        // or was blocked, swaps in the signed copy once it can be had, and
        // picks up newer signed copies, without an app update.
        let commons = self.commons.clone();
        self.shared.spawn(async move {
            loop {
                tokio::time::sleep(ENGINE_RETRY).await;
                if commons.engine_check_due() {
                    commons.check_engine(EngineCheck::Background).await;
                }
            }
        });
        self.commons.secure_signins().await;
        // An account whose setup was never finished is not kept: it has no
        // folders yet, and adding it again takes a moment.
        self.drop_unfinished().await;
        for app in self.apps() {
            app.start_services().await;
        }
    }

    /// Takes out accounts that signed in but never finished setup.
    async fn drop_unfinished(self: &Arc<Self>) {
        for app in self.apps() {
            if !app.core.settings().setup_complete && app.browser.lock().unwrap().is_none() {
                self.shared.log(&format!("Removing the {} account whose setup was not finished", app.core.cloud()));
                let _ = self.remove_account(app.id()).await;
            }
        }
    }

    /// Everything the screens need, in one piece.
    pub fn snapshot(&self) -> serde_json::Value {
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "windows": cfg!(windows),
            "test_mode": self.shared.test_mode,
            "engine": self.shared.engine_info(),
            "engine_channel": self.commons.has_engine_channel(),
            "general": self.shared.general(),
            "lock_signins": self.lock_signins,
            "providers": providers::ALL,
            "accounts": self.apps().iter().map(|a| a.snapshot()).collect::<Vec<_>>(),
            "default_ignores": config::default_ignores(),
            "pending": self.pending.lock().unwrap().clone(),
            "now": now_secs(),
        })
    }

    /// One-line status for the tray icon: the most pressing thing any
    /// account has to say.
    pub fn status_line(&self) -> String {
        let lines: Vec<String> = self.ready().iter().map(|a| a.status_line()).collect();
        if lines.is_empty() {
            return "SyncHopper: setup not finished".into();
        }
        for part in ["can't sync", "needs attention", "waiting for", "syncing", "uploading", "paused"] {
            if let Some(line) = lines.iter().find(|l| l.contains(part)) {
                return line.clone();
            }
        }
        "SyncHopper: up to date".into()
    }

    /// True when every account that is set up has syncing paused.
    pub fn paused_all(&self) -> bool {
        let ready = self.ready();
        !ready.is_empty() && ready.iter().all(|a| a.engine.sync_view().paused_all)
    }

    // ----- accounts -------------------------------------------------------

    /// A name no other account has: the service's own, with a number when
    /// there is already one.
    fn free_name(&self, wanted: &str) -> String {
        let taken: Vec<String> = self.shared.accounts().iter().map(|a| a.name.to_lowercase()).collect();
        if !taken.contains(&wanted.to_lowercase()) {
            return wanted.to_string();
        }
        (2..).map(|n| format!("{wanted} {n}")).find(|n| !taken.contains(&n.to_lowercase())).unwrap_or_else(|| wanted.to_string())
    }

    /// Signs in to a new account with `provider`. Returns the new account's
    /// id and its space. Nothing is kept when the sign-in fails.
    pub async fn add_account(self: &Arc<Self>, provider: &str, f: SignIn) -> Result<(String, About), String> {
        let p = providers::find(provider).ok_or_else(|| "SyncHopper doesn't work with that service yet.".to_string())?;
        // Only one account is being set up at a time.
        self.drop_unfinished().await;
        let accounts = self.shared.accounts();
        // The first Koofr account keeps the name it has always had in the
        // engine, so a new install matches one that came from version 0.4.
        let id = if p.id == "koofr" && !accounts.iter().any(|a| a.id == config::FIRST_ACCOUNT) {
            config::FIRST_ACCOUNT.to_string()
        } else {
            new_id(rclone::REMOTE_PREFIX)
        };
        let slots: Vec<usize> = accounts.iter().filter_map(|a| a.sidebar_slot).collect();
        let letters: Vec<String> = accounts.iter().map(|a| a.drive_letter.clone()).collect();
        let free = platform::free_letters();
        let letter = std::iter::once(p.letter.to_string())
            .chain(('D'..='Z').rev().map(|c| c.to_string()))
            .find(|l| !letters.contains(l) && (free.is_empty() || free.contains(l)))
            .unwrap_or_else(|| p.letter.to_string());
        let account = Account {
            id: id.clone(),
            provider: p.id.to_string(),
            name: self.free_name(p.name),
            remote: id.clone(),
            sidebar_slot: (0..platform::SIDEBAR_CLSIDS.len()).find(|n| !slots.contains(n)),
            drive_letter: letter,
            use_http2: p.http2_default,
            ..Default::default()
        };
        let saved = account.clone();
        self.shared.change(move |s| s.accounts.push(saved));
        let app = self.assemble(&account);
        self.apps.lock().unwrap().push(app.clone());
        match app.sign_in(&f).await {
            Ok(about) => Ok((id, about)),
            Err(e) => {
                self.apps.lock().unwrap().retain(|a| a.id() != id);
                self.shared.change(|s| s.accounts.retain(|a| a.id != id));
                Err(e)
            }
        }
    }

    /// Signs an account that is already set up in again, for example after
    /// a password change. Its folders and settings stay as they are.
    pub async fn reconnect(&self, id: &str, f: SignIn) -> Result<About, String> {
        let app = self.app(id)?;
        let about = app.sign_in(&f).await?;
        if app.core.settings().setup_complete {
            app.engine.sync_now(None);
            let drive = app.drive.clone();
            self.shared.spawn(async move { drive.restart().await });
        }
        Ok(about)
    }

    /// Disconnects an account and takes it out of SyncHopper. Its files stay
    /// where they are on this PC and in the account.
    pub async fn remove_account(self: &Arc<Self>, id: &str) -> Result<(), String> {
        let app = self.app(id)?;
        let remote = self.shared.account(id).map(|a| a.remote).unwrap_or_default();
        app.disconnect().await;
        // What the network drive kept of this account's files.
        if !remote.is_empty() && !remote.contains(['/', '\\', '.']) {
            for part in ["vfs", "vfsMeta"] {
                let _ = std::fs::remove_dir_all(self.shared.cache_dir().join(part).join(&remote));
            }
        }
        self.apps.lock().unwrap().retain(|a| a.id() != id);
        self.shared.change(|s| s.accounts.retain(|a| a.id != id));
        if self.ready().is_empty() {
            // Nothing left to start with Windows for.
            let _ = self.shared.host.set_autostart(false);
        }
        let _ = self.commons.apply_menu().await;
        Ok(())
    }

    /// Changes the name the window shows for an account.
    pub async fn rename_account(&self, id: &str, name: &str) -> Result<(), String> {
        let app = self.app(id)?;
        let name: String = name.trim().chars().filter(|c| !c.is_control()).collect();
        if name.is_empty() {
            return Err("Type a name for this account.".into());
        }
        if name.chars().count() > 40 {
            return Err("That name is too long. Keep it to 40 characters.".into());
        }
        if self.shared.accounts().iter().any(|a| a.id != id && a.name.eq_ignore_ascii_case(&name)) {
            return Err("Another account already has that name.".into());
        }
        self.shared.change(|s| {
            if let Some(a) = s.account_mut(id) {
                a.name = name;
            }
        });
        // The File Explorer entry carries the name.
        if let Err(e) = app.apply_sidebar().await {
            self.shared.log(&e);
        }
        Ok(())
    }

    // ----- requests from outside -----------------------------------------

    /// The account a folder on this PC most likely belongs with: the one
    /// whose own folder it is in, or else the first.
    fn account_for(&self, folder: &str) -> Option<Arc<App>> {
        let ready = self.ready();
        let nl = norm_local(folder);
        let sep = std::path::MAIN_SEPARATOR;
        ready
            .iter()
            .find(|a| {
                let base = norm_local(a.core.settings().base_folder.trim());
                !base.is_empty() && (nl == base || (nl.starts_with(&base) && nl[base.len()..].starts_with(sep)))
            })
            .or(ready.first())
            .cloned()
    }

    /// Asks the window to offer syncing `folder`. With more than one
    /// account the window lets the user pick which one.
    pub fn request_add_folder(self: &Arc<Self>, folder: &str) {
        match self.account_for(folder) {
            Some(app) => {
                for other in self.apps() {
                    if other.id() != app.id() {
                        *other.add_request.lock().unwrap() = None;
                    }
                }
                app.request_add_folder(folder);
            }
            None => self.shared.host.show_window(None),
        }
    }

    /// Handles command-line arguments, including the ones Explorer's
    /// right-click entry sends. Returns true when there was something to do.
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
        let ready = self.ready();
        match ready.as_slice() {
            [] => {
                self.shared.host.show_window(None);
                false
            }
            [only] => only.handle_args(args),
            _ => {
                // Which account? The window asks. A large selection arrives
                // in several launches, so the lists are joined.
                {
                    let mut pending = self.pending.lock().unwrap();
                    match pending.as_mut() {
                        Some(p) if p.kind == "upload" => p.paths.extend(paths),
                        _ => *pending = Some(Pending { kind: "upload".into(), paths }),
                    }
                }
                self.shared.host.show_window(None);
                self.shared.host.state_changed();
                true
            }
        }
    }

    /// The window dealt with what was waiting for an account to be picked.
    pub fn clear_pending(&self) {
        *self.pending.lock().unwrap() = None;
        self.shared.host.state_changed();
    }

    // ----- troubleshooting ------------------------------------------------

    /// Plain-text details for a support request. No passwords, and email
    /// addresses are masked.
    pub fn diagnostics(&self) -> String {
        let g = self.shared.general();
        let mut out = App::diagnostics_head(&self.shared);
        out.push_str(&format!(
            "General: right-click {}, start with Windows {}, app updates {}, notifications {}\n",
            g.feature_context_menu, g.autostart, g.auto_update_app, g.notify_level
        ));
        let apps = self.apps();
        out.push_str(&format!("Accounts: {}\n", apps.len()));
        for app in apps {
            out.push_str(&app.diagnostics_account());
        }
        out.push_str(&format!("Log file: {}\n", self.shared.log_path().display()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_right_click_items_name_the_service_only_when_there_is_one_account() {
        let account = |provider: &str| Account { provider: provider.into(), ..Default::default() };
        let one = menu_labels(&[account("pcloud")]);
        assert_eq!((one.send.as_str(), one.add_folder.as_str()), ("Send to pCloud", "Sync folder to pCloud"));
        let two = menu_labels(&[account("koofr"), account("pcloud")]);
        assert_eq!((two.send.as_str(), two.add_folder.as_str()), ("Send to...", "Sync folder to..."));
        let none = menu_labels(&[]);
        assert_eq!(none.send, "Send to...");
    }
}
