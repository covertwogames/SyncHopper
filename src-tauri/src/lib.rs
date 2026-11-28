//! SyncHopper: two-way sync for cloud storage accounts, built on rclone.
//!
//! This file is the window layer: the tray icon, the commands the screens
//! call, and the plumbing between Tauri and the app logic in `app.rs`.

pub mod app;
pub mod colors;
pub mod config;
pub mod core;
pub mod engine;
pub mod filen;
pub mod mount;
pub mod newfolders;
pub mod platform;
pub mod providers;
pub mod rclone;
pub mod uploads;
pub mod util;

use crate::app::{Hub, Preview, SetupChoices, SignIn};
use crate::core::Host;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, DragDropEvent, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_dialog::DialogExt as _;
use tauri_plugin_notification::NotificationExt as _;
use tauri_plugin_opener::OpenerExt as _;
use tauri_plugin_updater::UpdaterExt as _;

struct TauriHost {
    handle: AppHandle,
    dirty: Arc<AtomicBool>,
}

fn show_main(handle: &AppHandle, view: Option<&str>) {
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    if let Some(v) = view {
        let _ = handle.emit("navigate", v);
    }
}

impl Host for TauriHost {
    fn state_changed(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    fn notify(&self, title: &str, body: &str) {
        let _ = self.handle.notification().builder().title(title).body(body).show();
    }

    fn notify_folder(&self, title: &str, body: &str, folder: &str) {
        let _ = self
            .handle
            .notification()
            .builder()
            .title(title)
            .body(body)
            .action_type_id(FOLDER_ACTIONS)
            .extra("folder", folder)
            .show();
    }

    fn set_autostart(&self, on: bool) -> Result<(), String> {
        let auto = self.handle.autolaunch();
        let result = if on { auto.enable() } else { auto.disable() };
        match result {
            Ok(()) => Ok(()),
            // Turning it off when it was never on is not a problem.
            Err(_) if !on => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn exe_path(&self) -> Option<PathBuf> {
        std::env::current_exe().ok()
    }

    fn show_window(&self, view: Option<&str>) {
        show_main(&self.handle, view);
    }
}

/// The notification type that carries a "Sync it" button.
const FOLDER_ACTIONS: &str = "folder";

/// Carried by a notification whose click should open a particular screen.
const NOTICE_VIEW: &str = "view";

/// Sets up the "Sync it" button on notifications about folders and what a
/// click on a notification does. Windows only reports clicks while the
/// notification is on screen; the main window shows the same choices.
fn setup_notification_actions(handle: &AppHandle) {
    use tauri_plugin_notification::{Action, ActionType};
    let notes = handle.notification();
    let _ = notes.register_action_types(vec![ActionType::builder(FOLDER_ACTIONS)
        .actions(vec![Action::builder("sync", "Sync it").build()])
        .build()]);
    let handle = handle.clone();
    let _ = notes.on_action(move |performed| {
        let extra = |key: &str| performed.notification().and_then(|n| n.extra().get(key).and_then(|v| v.as_str()).map(|s| s.to_string()));
        let folder = extra("folder");
        // The screen a click should open, for notifications that name one.
        let view = extra(NOTICE_VIEW);
        let hub = handle.state::<Arc<Hub>>().inner().clone();
        let handle = handle.clone();
        let action = performed.action_id().to_string();
        tauri::async_runtime::spawn(async move {
            match (action.as_str(), folder) {
                ("sync", Some(folder)) => hub.request_add_folder(&folder),
                _ => show_main(&handle, view.as_deref()),
            }
        });
    });
}

struct Tray {
    pause_item: Mutex<Option<MenuItem<tauri::Wry>>>,
}

// ----- updates to SyncHopper itself ------------------------------------

/// Where SyncHopper is in checking for and installing its own updates.
#[derive(Clone, Default, serde::Serialize)]
struct UpdateView {
    /// False in builds made without the update signing key, such as test
    /// builds made on a PC. Those can't update themselves.
    enabled: bool,
    /// "idle", "checking", "current", "available", "downloading",
    /// "installing" or "error".
    status: String,
    version: String,
    notes: String,
    percent: u8,
    message: String,
    checked_at: u64,
}

struct Updates {
    view: Mutex<UpdateView>,
    pending: tokio::sync::Mutex<Option<tauri_plugin_updater::Update>>,
    dirty: Arc<AtomicBool>,
}

impl Updates {
    fn set(&self, change: impl FnOnce(&mut UpdateView)) {
        change(&mut self.view.lock().unwrap());
        self.dirty.store(true, Ordering::SeqCst);
    }

    fn view(&self) -> UpdateView {
        self.view.lock().unwrap().clone()
    }
}

/// The public key this build checks updates with, if it has one. Release
/// builds do; builds made on a PC don't.
fn update_pubkey(config: &tauri::Config) -> Option<String> {
    config
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// Release builds get the sync engine from the signed channel, checked with
/// the same key as app updates.
fn engine_channel(pubkey: Option<String>) -> Option<rclone::EngineChannel> {
    let pubkey = pubkey?;
    #[allow(unused_mut)]
    let mut manifest_url = rclone::ENGINE_CHANNEL_URL.to_string();
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("SYNCHOPPER_ENGINE_URL") {
        manifest_url = url;
    }
    Some(rclone::EngineChannel { manifest_url, pubkey })
}

/// What the notification about a new version says. Clicking it opens the
/// window, where the update is one more click.
const UPDATE_NOTICE_TITLE: &str = "SyncHopper update";
const UPDATE_NOTICE_BODY: &str = "An update for SyncHopper is available. Click here to open SyncHopper and install it.";

/// Asks the release page whether a newer SyncHopper exists.
async fn check_for_update(handle: &AppHandle, manual: bool) -> Result<(), String> {
    let updates = handle.state::<Arc<Updates>>().inner().clone();
    if !updates.view().enabled {
        return Err("This copy of SyncHopper was built on this PC, so it can't update itself. Install the release version to get updates.".into());
    }
    if matches!(updates.view().status.as_str(), "checking" | "downloading" | "installing") {
        return Ok(());
    }
    updates.set(|u| {
        u.status = "checking".into();
        u.message.clear();
    });
    let mut builder = handle.updater_builder();
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("SYNCHOPPER_UPDATE_URL") {
        let parsed = url.parse().map_err(|e| format!("{e}"))?;
        builder = builder.endpoints(vec![parsed]).map_err(|e| e.to_string())?;
    }
    let result = match builder.build() {
        Ok(updater) => updater.check().await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    let hub = handle.state::<Arc<Hub>>().inner().clone();
    match result {
        Ok(Some(update)) => {
            let version = update.version.clone();
            let notes = update.body.clone().unwrap_or_default();
            hub.shared.log(&format!("SyncHopper {version} is available"));
            *updates.pending.lock().await = Some(update);
            updates.set(|u| {
                u.status = "available".into();
                u.version = version.clone();
                u.notes = notes;
                u.checked_at = util::now_secs();
            });
            // Each new version is announced once, ever, and not at all when
            // notifications are set to Nothing. Someone who just pressed
            // Check for updates is looking at the answer already.
            let first_time = hub.shared.general().update_notified != version;
            if first_time {
                if !manual && hub.shared.notices_on() {
                    // A click opens the main screen, where the install
                    // button is, whatever screen the window was left on.
                    let _ = handle.notification().builder().title(UPDATE_NOTICE_TITLE).body(UPDATE_NOTICE_BODY).extra(NOTICE_VIEW, "main").show();
                }
                if manual || hub.shared.notices_on() {
                    let seen = version.clone();
                    hub.shared.change(move |s| s.general.update_notified = seen);
                }
            }
            Ok(())
        }
        Ok(None) => {
            *updates.pending.lock().await = None;
            updates.set(|u| {
                u.status = "current".into();
                u.version.clear();
                u.checked_at = util::now_secs();
            });
            Ok(())
        }
        Err(e) => {
            hub.shared.log(&format!("Update check failed: {e}"));
            updates.set(|u| {
                u.status = if manual { "error".into() } else { "idle".into() };
                u.message = format!("Couldn't check for updates: {e}");
            });
            if manual {
                Err(format!("Couldn't check for updates: {e}"))
            } else {
                Ok(())
            }
        }
    }
}

/// Downloads and installs the update found by the last check. On Windows
/// the installer closes SyncHopper, installs, and opens it again.
async fn install_update(handle: &AppHandle) -> Result<(), String> {
    let updates = handle.state::<Arc<Updates>>().inner().clone();
    let Some(update) = updates.pending.lock().await.clone() else {
        return Err("There is no update waiting to be installed. Check for updates first.".into());
    };
    let hub = handle.state::<Arc<Hub>>().inner().clone();
    hub.shared.log(&format!("Installing SyncHopper {}", update.version));
    // Let anything in progress stop cleanly first. Interrupted syncs pick up
    // where they left off after the restart.
    let apps = hub.apps();
    for app in &apps {
        app.engine.stop_all();
        app.uploads.cancel_all();
        app.drive.stop();
    }
    for app in &apps {
        app.engine.wait_idle().await;
    }
    updates.set(|u| {
        u.status = "downloading".into();
        u.percent = 0;
        u.message.clear();
    });
    let total = Arc::new(Mutex::new((0u64, None::<u64>)));
    let progress = updates.clone();
    let counter = total.clone();
    let finished = updates.clone();
    let result = update
        .download_and_install(
            move |chunk, length| {
                let mut t = counter.lock().unwrap();
                t.0 += chunk as u64;
                t.1 = length;
                let pct = match t.1 {
                    Some(len) if len > 0 => ((t.0 * 100) / len).min(100) as u8,
                    _ => 0,
                };
                progress.set(|u| u.percent = pct);
            },
            move || finished.set(|u| u.status = "installing".into()),
        )
        .await;
    match result {
        Ok(()) => {
            // Reached only where the installer does not close the app itself.
            handle.restart();
        }
        Err(e) => {
            hub.shared.log(&format!("Update install failed: {e}"));
            updates.set(|u| {
                u.status = "error".into();
                u.message = format!("The update could not be installed: {e}");
            });
            // Put back what was stopped for the install.
            for app in hub.ready() {
                app.drive.apply().await;
                app.engine.sync_now(None);
            }
            Err(format!("The update could not be installed: {e}"))
        }
    }
}

/// Checks for updates a minute after starting, then twice a day.
async fn update_loop(handle: AppHandle) {
    tokio::time::sleep(Duration::from_secs(60)).await;
    loop {
        let hub = handle.state::<Arc<Hub>>().inner().clone();
        if hub.shared.general().auto_update_app {
            let _ = check_for_update(&handle, false).await;
        }
        tokio::time::sleep(Duration::from_secs(12 * 3600)).await;
    }
}

/// The app state plus the update state, as one object for the screens.
fn full_state(handle: &AppHandle) -> serde_json::Value {
    let hub = handle.state::<Arc<Hub>>().inner().clone();
    let mut v = hub.snapshot();
    if let (Some(obj), Some(updates)) = (v.as_object_mut(), handle.try_state::<Arc<Updates>>()) {
        obj.insert("update".into(), serde_json::to_value(updates.view()).unwrap_or_default());
    }
    v
}

type Ctx<'a> = State<'a, Arc<Hub>>;

/// What the screens get back from signing in to a new account.
#[derive(serde::Serialize)]
struct Connected {
    account: String,
    about: rclone::About,
}

// ----- commands the screens call ---------------------------------------

#[tauri::command]
fn get_state(handle: AppHandle) -> serde_json::Value {
    full_state(&handle)
}

#[tauri::command]
fn request_add_folder(hub: Ctx<'_>, account: Option<String>, folder: String) -> Result<(), String> {
    match account {
        // The window picked the account: the request moves to it.
        Some(id) => {
            let app = hub.app(&id)?;
            for other in hub.apps() {
                if other.id() != id {
                    other.clear_add_request();
                }
            }
            app.request_add_folder(&folder);
        }
        None => hub.request_add_folder(&folder),
    }
    Ok(())
}

#[tauri::command]
fn clear_add_request(hub: Ctx<'_>) {
    for app in hub.apps() {
        app.clear_add_request();
    }
}

#[tauri::command]
fn clear_pending(hub: Ctx<'_>) {
    hub.clear_pending();
}

#[tauri::command]
fn dismiss_folder_notice(hub: Ctx<'_>, account: String, id: String) -> Result<(), String> {
    hub.app(&account)?.folders.dismiss(&id);
    Ok(())
}

#[tauri::command]
async fn check_updates(handle: AppHandle) -> Result<(), String> {
    check_for_update(&handle, true).await
}

#[tauri::command]
async fn install_app_update(handle: AppHandle) -> Result<(), String> {
    install_update(&handle).await
}

#[tauri::command]
async fn ignore_file(hub: Ctx<'_>, account: String, id: String, path: String) -> Result<(), String> {
    hub.app(&account)?.engine.ignore_file(&id, &path)
}

#[tauri::command]
async fn unignore_file(hub: Ctx<'_>, account: String, id: String, path: String) -> Result<(), String> {
    hub.app(&account)?.engine.unignore_file(&id, &path)
}

#[tauri::command]
async fn retry_engine(hub: Ctx<'_>) -> Result<(), String> {
    hub.commons.check_engine(crate::app::EngineCheck::Manual).await;
    Ok(())
}

/// Signs in to a new account. For services that sign in on their own page
/// this returns once the user has finished there.
#[tauri::command]
async fn add_account(hub: Ctx<'_>, provider: String, signin: SignIn) -> Result<Connected, String> {
    let (account, about) = hub.add_account(&provider, signin).await?;
    Ok(Connected { account, about })
}

#[tauri::command]
async fn reconnect(hub: Ctx<'_>, account: String, signin: SignIn) -> Result<rclone::About, String> {
    hub.reconnect(&account, signin).await
}

#[tauri::command]
fn cancel_sign_in(hub: Ctx<'_>) {
    for app in hub.apps() {
        app.cancel_sign_in();
    }
}

#[tauri::command]
async fn remove_account(hub: Ctx<'_>, account: String) -> Result<(), String> {
    hub.inner().remove_account(&account).await
}

#[tauri::command]
async fn rename_account(hub: Ctx<'_>, account: String, name: String) -> Result<(), String> {
    hub.rename_account(&account, &name).await
}

#[tauri::command]
async fn set_account_color(hub: Ctx<'_>, account: String, color: String) -> Result<(), String> {
    hub.set_account_color(&account, &color).await
}

/// Where a new or existing account would keep its folders on this PC.
#[tauri::command]
fn default_base_folder(hub: Ctx<'_>, account: String) -> Result<String, String> {
    let app = hub.app(&account)?;
    let saved = app.core.settings().base_folder;
    Ok(if saved.is_empty() {
        platform::default_base_folder(account == config::FIRST_ACCOUNT, &app.core.label())
    } else {
        saved
    })
}

#[tauri::command]
async fn list_remote(hub: Ctx<'_>, account: String, path: String) -> Result<Vec<String>, String> {
    hub.app(&account)?.list_remote(&path).await
}

#[tauri::command]
async fn make_remote_dir(hub: Ctx<'_>, account: String, path: String) -> Result<(), String> {
    hub.app(&account)?.make_remote_dir(&path).await
}

#[tauri::command]
async fn preview_pair(hub: Ctx<'_>, account: String, local: String, remote: String) -> Result<Preview, String> {
    hub.app(&account)?.preview(&local, &remote).await
}

#[tauri::command]
async fn finish_setup(hub: Ctx<'_>, account: String, choices: SetupChoices) -> Result<Vec<String>, String> {
    hub.app(&account)?.finish_setup(choices).await
}

#[tauri::command]
async fn update_settings(hub: Ctx<'_>, account: String, patch: serde_json::Value) -> Result<(), String> {
    hub.app(&account)?.update_settings(patch).await
}

#[tauri::command]
async fn restore_defaults(hub: Ctx<'_>, account: String) -> Result<(), String> {
    hub.app(&account)?.restore_defaults().await
}

#[tauri::command]
async fn add_pair(hub: Ctx<'_>, account: String, local: String, remote: String) -> Result<(), String> {
    hub.app(&account)?.engine.add_pair(&local, &remote).map(|_| ())
}

#[tauri::command]
async fn remove_pair(hub: Ctx<'_>, account: String, id: String) -> Result<(), String> {
    hub.app(&account)?.engine.remove_pair(&id);
    Ok(())
}

#[tauri::command]
async fn pair_action(hub: Ctx<'_>, account: String, id: String, action: String) -> Result<(), String> {
    let app = hub.app(&account)?;
    let e = &app.engine;
    match action.as_str() {
        "sync" => e.sync_now(Some(&id)),
        "pause" => e.set_paused(&id, true),
        "resume" => e.set_paused(&id, false),
        "approve" => e.approve_deletes(&id),
        "approve_changed" => e.approve_changed(&id),
        "restore" => e.restore_deleted(&id),
        "repair" => e.repair(&id),
        "retry_skipped" => e.retry_skipped(&id),
        "dismiss_conflicts" => e.dismiss_conflicts(&id),
        other => return Err(format!("Unknown action: {other}")),
    }
    Ok(())
}

#[tauri::command]
async fn sync_all(hub: Ctx<'_>, account: String) -> Result<(), String> {
    hub.app(&account)?.engine.sync_now(None);
    Ok(())
}

#[tauri::command]
async fn set_paused_all(hub: Ctx<'_>, account: String, paused: bool) -> Result<(), String> {
    hub.app(&account)?.engine.set_paused_all(paused);
    Ok(())
}

#[tauri::command]
async fn upload_add(hub: Ctx<'_>, account: String, paths: Vec<String>, dest: String) -> Result<usize, String> {
    let app = hub.app(&account)?;
    app.set_upload_dest(&dest);
    Ok(app.uploads.add(paths, &dest))
}

#[tauri::command]
async fn upload_action(hub: Ctx<'_>, account: String, action: String, id: Option<u64>) -> Result<(), String> {
    let app = hub.app(&account)?;
    match (action.as_str(), id) {
        ("cancel_all", _) => app.uploads.cancel_all(),
        ("clear", _) => app.uploads.clear_finished(),
        ("retry", Some(id)) => app.uploads.retry(id),
        _ => return Err("Unknown upload action.".into()),
    }
    Ok(())
}

#[tauri::command]
fn set_upload_dest(hub: Ctx<'_>, account: String, dest: String) -> Result<(), String> {
    hub.app(&account)?.set_upload_dest(&dest);
    Ok(())
}

#[tauri::command]
async fn drive_action(hub: Ctx<'_>, account: String, action: String) -> Result<(), String> {
    let app = hub.app(&account)?;
    match action.as_str() {
        "install" => {
            let drive = app.drive.clone();
            tauri::async_runtime::spawn(async move { drive.install_driver().await });
        }
        "retry" => {
            let drive = app.drive.clone();
            tauri::async_runtime::spawn(async move { drive.restart().await });
        }
        other => return Err(format!("Unknown action: {other}")),
    }
    Ok(())
}

async fn pick(handle: &AppHandle, folder: bool, many: bool, start: Option<String>) -> Vec<String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<String>>();
    let mut dialog = handle.dialog().file();
    if let Some(w) = handle.get_webview_window("main") {
        dialog = dialog.set_parent(&w);
    }
    if let Some(dir) = start.filter(|d| std::path::Path::new(d).is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    let to_text = |p: tauri_plugin_dialog::FilePath| p.into_path().ok().map(|p| p.to_string_lossy().to_string());
    if folder && many {
        dialog.pick_folders(move |r| {
            let _ = tx.send(r.unwrap_or_default().into_iter().filter_map(to_text).collect());
        });
    } else if folder {
        dialog.pick_folder(move |r| {
            let _ = tx.send(r.into_iter().filter_map(to_text).collect());
        });
    } else {
        dialog.pick_files(move |r| {
            let _ = tx.send(r.unwrap_or_default().into_iter().filter_map(to_text).collect());
        });
    }
    rx.await.unwrap_or_default()
}

#[tauri::command]
async fn pick_folder(handle: AppHandle, start: Option<String>) -> Result<Option<String>, String> {
    Ok(pick(&handle, true, false, start).await.into_iter().next())
}

#[tauri::command]
async fn pick_upload(handle: AppHandle, folders: bool) -> Result<Vec<String>, String> {
    Ok(pick(&handle, folders, true, None).await)
}

/// Opens a web page, file or folder. `account` says whose drive or folder
/// is meant, for the targets that belong to one account.
#[tauri::command]
fn open_target(hub: Ctx<'_>, handle: AppHandle, kind: String, account: Option<String>, value: Option<String>) -> Result<(), String> {
    let opener = handle.opener();
    let settings = || -> Result<config::Settings, String> { Ok(hub.app(account.as_deref().unwrap_or_default())?.core.settings()) };
    let result = match kind.as_str() {
        "koofr_password_page" => opener.open_url(providers::KOOFR_PASSWORD_URL, None::<&str>),
        "winfsp_page" => opener.open_url(app::WINFSP_URL, None::<&str>),
        "rclone_site" => opener.open_url(app::RCLONE_URL, None::<&str>),
        // The sign-in page the engine is waiting on. It is always on this PC.
        "signin_link" => {
            let link = value.unwrap_or_default();
            if !link.starts_with("http://127.0.0.1:") && !link.starts_with("http://localhost:") {
                return Err("That sign-in link can't be opened.".into());
            }
            opener.open_url(link, None::<&str>)
        }
        "log" => {
            let path = hub.shared.log_path();
            if !path.exists() {
                hub.shared.log("Log opened");
            }
            opener.open_path(path.to_string_lossy(), None::<&str>)
        }
        "drive" => opener.open_path(format!("{}:\\", settings()?.drive_letter), None::<&str>),
        "base_folder" => {
            let base = settings()?.base_folder.trim().to_string();
            if base.is_empty() {
                return Err("This account has no folder on this PC yet.".into());
            }
            opener.open_path(base, None::<&str>)
        }
        "pair_folder" => {
            let id = value.unwrap_or_default();
            match settings()?.pairs.iter().find(|p| p.id == id) {
                Some(p) => opener.open_path(p.local.clone(), None::<&str>),
                None => return Err("That folder is no longer synced.".into()),
            }
        }
        other => return Err(format!("Unknown target: {other}")),
    };
    result.map_err(|e| e.to_string())
}

#[tauri::command]
fn diagnostics(hub: Ctx<'_>) -> String {
    hub.diagnostics()
}

#[tauri::command]
fn hide_window(handle: AppHandle) {
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.hide();
    }
}

// ----- tray --------------------------------------------------------------

fn build_tray(handle: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(handle, "open", "Open SyncHopper", true, None::<&str>)?;
    let sync = MenuItem::with_id(handle, "sync", "Sync now", true, None::<&str>)?;
    let pause = MenuItem::with_id(handle, "pause", "Pause syncing", true, None::<&str>)?;
    let upload = MenuItem::with_id(handle, "upload", "Upload files", true, None::<&str>)?;
    let quit = MenuItem::with_id(handle, "quit", "Quit SyncHopper", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(handle)?;
    let sep2 = PredefinedMenuItem::separator(handle)?;
    let menu = Menu::with_items(handle, &[&open, &sep1, &sync, &pause, &upload, &sep2, &quit])?;
    handle.manage(Tray { pause_item: Mutex::new(Some(pause)) });

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("SyncHopper")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|handle, event| {
            let hub = handle.state::<Arc<Hub>>().inner().clone();
            match event.id.as_ref() {
                "open" => show_main(handle, None),
                "upload" => {
                    let view = if hub.ready().is_empty() { None } else { Some("upload") };
                    show_main(handle, view);
                }
                // The tray speaks for every account at once.
                "sync" => hub.ready().iter().for_each(|app| app.engine.sync_now(None)),
                "pause" => {
                    let paused = hub.paused_all();
                    hub.ready().iter().for_each(|app| app.engine.set_paused_all(!paused));
                }
                "quit" => {
                    for app in hub.apps() {
                        app.cancel_sign_in();
                        app.engine.stop_all();
                        app.uploads.cancel_all();
                        app.drive.stop();
                    }
                    let handle = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(600)).await;
                        handle.exit(0);
                    });
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle(), None);
            }
        });
    if let Some(icon) = handle.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(handle)?;
    Ok(())
}

/// Sends fresh state to the screens and keeps the tray text current,
/// at most a few times a second.
async fn state_pump(handle: AppHandle, hub: Arc<Hub>, dirty: Arc<AtomicBool>) {
    let mut last_tip = String::new();
    let mut last_paused = None;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if !dirty.swap(false, Ordering::SeqCst) {
            continue;
        }
        let _ = handle.emit("state", full_state(&handle));
        let tip = hub.status_line();
        if tip != last_tip {
            if let Some(tray) = handle.tray_by_id("main") {
                let _ = tray.set_tooltip(Some(tip.as_str()));
            }
            last_tip = tip;
        }
        let paused = hub.paused_all();
        if last_paused != Some(paused) {
            if let Some(tray) = handle.try_state::<Tray>() {
                if let Some(item) = tray.pause_item.lock().unwrap().as_ref() {
                    let _ = item.set_text(if paused { "Resume syncing" } else { "Pause syncing" });
                }
            }
            last_paused = Some(paused);
        }
    }
}

fn data_dir(handle: &AppHandle) -> PathBuf {
    #[cfg(debug_assertions)]
    if let Ok(dir) = std::env::var("SYNCHOPPER_DATA_DIR") {
        return PathBuf::from(dir);
    }
    handle
        .path()
        .app_local_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("SyncHopper"))
}

/// Development builds can point the app at a plain folder instead of a cloud
/// service, so the whole flow can be tried without an account.
fn test_remote() -> Option<String> {
    #[cfg(debug_assertions)]
    if let Ok(dir) = std::env::var("SYNCHOPPER_TEST_REMOTE") {
        return Some(dir);
    }
    None
}

pub fn run() {
    let context = tauri::generate_context!();
    let pubkey = update_pubkey(context.config());
    let can_update = pubkey.is_some();
    let mut builder = tauri::Builder::default();
    if can_update {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    }
    builder
        .plugin(tauri_plugin_single_instance::init(|handle, argv, _cwd| {
            // A second launch lands here, inside the copy that is already
            // running. Hand the work to the async runtime and return at once
            // so the second copy can exit.
            let hub = handle.state::<Arc<Hub>>().inner().clone();
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                if !hub.handle_args(&argv) {
                    show_main(&handle, None);
                }
            });
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |tauri_app| {
            let handle = tauri_app.handle().clone();
            let dirty = Arc::new(AtomicBool::new(true));
            handle.manage(Arc::new(Updates {
                view: Mutex::new(UpdateView { enabled: can_update, status: "idle".into(), ..Default::default() }),
                pending: tokio::sync::Mutex::new(None),
                dirty: dirty.clone(),
            }));
            let host = Arc::new(TauriHost { handle: handle.clone(), dirty: dirty.clone() });
            let hub = Hub::new(data_dir(&handle), test_remote(), host);
            if let Some(channel) = engine_channel(pubkey.clone()) {
                hub.commons.use_engine_channel(channel);
            }
            handle.manage(hub.clone());
            setup_notification_actions(&handle);
            build_tray(&handle)?;

            let args: Vec<String> = std::env::args().collect();
            let minimized = args.iter().any(|a| a == "--minimized");
            let setup_done = !hub.ready().is_empty();
            if !(minimized && setup_done) {
                show_main(&handle, None);
            }

            tauri::async_runtime::spawn(state_pump(handle.clone(), hub.clone(), dirty));
            if can_update {
                tauri::async_runtime::spawn(update_loop(handle.clone()));
            }
            let starter = hub.clone();
            tauri::async_runtime::spawn(async move {
                starter.start().await;
                starter.handle_args(&args);
            });
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                // Closing the window keeps the app running in the tray.
                api.prevent_close();
                let _ = window.hide();
                let hub = window.state::<Arc<Hub>>().inner().clone();
                let g = hub.shared.general();
                if !g.tray_hint_shown && g.notify_level != "none" {
                    hub.shared.change(|s| s.general.tray_hint_shown = true);
                    hub.shared.host.notify(
                        "SyncHopper is still running",
                        "It keeps syncing from the tray, next to the clock. Right-click its icon to quit.",
                    );
                }
            }
            WindowEvent::DragDrop(DragDropEvent::Enter { .. }) => {
                let _ = window.emit("drag-hover", true);
            }
            WindowEvent::DragDrop(DragDropEvent::Leave) => {
                let _ = window.emit("drag-hover", false);
            }
            WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
                let list: Vec<String> = paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
                let _ = window.emit("files-dropped", list);
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            retry_engine,
            add_account,
            reconnect,
            cancel_sign_in,
            remove_account,
            rename_account,
            set_account_color,
            default_base_folder,
            list_remote,
            make_remote_dir,
            preview_pair,
            finish_setup,
            update_settings,
            restore_defaults,
            add_pair,
            remove_pair,
            pair_action,
            sync_all,
            set_paused_all,
            upload_add,
            upload_action,
            set_upload_dest,
            drive_action,
            pick_folder,
            pick_upload,
            open_target,
            diagnostics,
            hide_window,
            check_updates,
            install_app_update,
            ignore_file,
            unignore_file,
            request_add_folder,
            clear_add_request,
            clear_pending,
            dismiss_folder_notice,
        ])
        .run(context)
        .expect("error while running SyncHopper");
}
