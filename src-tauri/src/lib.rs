//! SyncHopper: two-way sync for Koofr, built on rclone.
//!
//! This file is the window layer: the tray icon, the commands the screens
//! call, and the plumbing between Tauri and the app logic in `app.rs`.

pub mod app;
pub mod config;
pub mod core;
pub mod engine;
pub mod mount;
pub mod platform;
pub mod rclone;
pub mod uploads;
pub mod util;

use crate::app::{App, Preview, SetupChoices};
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

struct Tray {
    pause_item: Mutex<Option<MenuItem<tauri::Wry>>>,
}

type Ctx<'a> = State<'a, Arc<App>>;

// ----- commands the screens call ---------------------------------------

#[tauri::command]
fn get_state(app: Ctx<'_>) -> serde_json::Value {
    app.snapshot()
}

#[tauri::command]
async fn retry_engine(app: Ctx<'_>) -> Result<(), String> {
    app.ensure_engine().await;
    Ok(())
}

#[tauri::command]
async fn connect(app: Ctx<'_>, email: String, password: String) -> Result<rclone::About, String> {
    app.connect(&email, &password).await
}

#[tauri::command]
async fn disconnect(app: Ctx<'_>) -> Result<(), String> {
    app.disconnect().await;
    Ok(())
}

#[tauri::command]
fn default_base_folder(app: Ctx<'_>) -> String {
    let saved = app.core.settings().base_folder;
    if saved.is_empty() {
        platform::default_base_folder()
    } else {
        saved
    }
}

#[tauri::command]
async fn list_remote(app: Ctx<'_>, path: String) -> Result<Vec<String>, String> {
    app.list_remote(&path).await
}

#[tauri::command]
async fn make_remote_dir(app: Ctx<'_>, path: String) -> Result<(), String> {
    app.make_remote_dir(&path).await
}

#[tauri::command]
async fn preview_pair(app: Ctx<'_>, local: String, remote: String) -> Result<Preview, String> {
    app.preview(&local, &remote).await
}

#[tauri::command]
async fn finish_setup(app: Ctx<'_>, choices: SetupChoices) -> Result<Vec<String>, String> {
    app.finish_setup(choices).await
}

#[tauri::command]
async fn update_settings(app: Ctx<'_>, patch: serde_json::Value) -> Result<(), String> {
    app.update_settings(patch).await
}

#[tauri::command]
async fn restore_defaults(app: Ctx<'_>) -> Result<(), String> {
    app.restore_defaults().await
}

#[tauri::command]
async fn add_pair(app: Ctx<'_>, local: String, remote: String) -> Result<(), String> {
    app.engine.add_pair(&local, &remote).map(|_| ())
}

#[tauri::command]
async fn remove_pair(app: Ctx<'_>, id: String) -> Result<(), String> {
    app.engine.remove_pair(&id);
    Ok(())
}

#[tauri::command]
async fn pair_action(app: Ctx<'_>, id: String, action: String) -> Result<(), String> {
    let e = &app.engine;
    match action.as_str() {
        "sync" => e.sync_now(Some(&id)),
        "pause" => e.set_paused(&id, true),
        "resume" => e.set_paused(&id, false),
        "approve" => e.approve_deletes(&id),
        "restore" => e.restore_deleted(&id),
        "repair" => e.repair(&id),
        "retry_skipped" => e.retry_skipped(&id),
        "dismiss_conflicts" => e.dismiss_conflicts(&id),
        other => return Err(format!("Unknown action: {other}")),
    }
    Ok(())
}

#[tauri::command]
async fn sync_all(app: Ctx<'_>) -> Result<(), String> {
    app.engine.sync_now(None);
    Ok(())
}

#[tauri::command]
async fn set_paused_all(app: Ctx<'_>, paused: bool) -> Result<(), String> {
    app.engine.set_paused_all(paused);
    Ok(())
}

#[tauri::command]
async fn upload_add(app: Ctx<'_>, paths: Vec<String>, dest: String) -> Result<usize, String> {
    app.set_upload_dest(&dest);
    Ok(app.uploads.add(paths, &dest))
}

#[tauri::command]
async fn upload_action(app: Ctx<'_>, action: String, id: Option<u64>) -> Result<(), String> {
    match (action.as_str(), id) {
        ("cancel_all", _) => app.uploads.cancel_all(),
        ("clear", _) => app.uploads.clear_finished(),
        ("retry", Some(id)) => app.uploads.retry(id),
        _ => return Err("Unknown upload action.".into()),
    }
    Ok(())
}

#[tauri::command]
fn set_upload_dest(app: Ctx<'_>, dest: String) {
    app.set_upload_dest(&dest);
}

#[tauri::command]
async fn drive_action(app: Ctx<'_>, action: String) -> Result<(), String> {
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

#[tauri::command]
fn open_target(app: Ctx<'_>, handle: AppHandle, kind: String, value: Option<String>) -> Result<(), String> {
    let opener = handle.opener();
    let s = app.core.settings();
    let result = match kind.as_str() {
        "koofr_password_page" => opener.open_url(app::KOOFR_PASSWORD_URL, None::<&str>),
        "winfsp_page" => opener.open_url(app::WINFSP_URL, None::<&str>),
        "log" => {
            let path = app.core.log_path();
            if !path.exists() {
                app.core.log("Log opened");
            }
            opener.open_path(path.to_string_lossy(), None::<&str>)
        }
        "drive" => opener.open_path(format!("{}:\\", s.drive_letter), None::<&str>),
        "pair_folder" => {
            let id = value.unwrap_or_default();
            match s.pairs.iter().find(|p| p.id == id) {
                Some(p) => opener.open_path(p.local.clone(), None::<&str>),
                None => return Err("That folder is no longer synced.".into()),
            }
        }
        other => return Err(format!("Unknown target: {other}")),
    };
    result.map_err(|e| e.to_string())
}

#[tauri::command]
fn diagnostics(app: Ctx<'_>) -> String {
    app.diagnostics()
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
    let upload = MenuItem::with_id(handle, "upload", "Upload to Koofr", true, None::<&str>)?;
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
            let app = handle.state::<Arc<App>>().inner().clone();
            match event.id.as_ref() {
                "open" => show_main(handle, None),
                "upload" => {
                    let view = if app.core.settings().setup_complete { Some("upload") } else { None };
                    show_main(handle, view);
                }
                "sync" => app.engine.sync_now(None),
                "pause" => {
                    let paused = app.engine.sync_view().paused_all;
                    app.engine.set_paused_all(!paused);
                }
                "quit" => {
                    app.engine.stop_all();
                    app.uploads.cancel_all();
                    app.drive.stop();
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
async fn state_pump(handle: AppHandle, app: Arc<App>, dirty: Arc<AtomicBool>) {
    let mut last_tip = String::new();
    let mut last_paused = None;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if !dirty.swap(false, Ordering::SeqCst) {
            continue;
        }
        let _ = handle.emit("state", app.snapshot());
        let tip = app.status_line();
        if tip != last_tip {
            if let Some(tray) = handle.tray_by_id("main") {
                let _ = tray.set_tooltip(Some(tip.as_str()));
            }
            last_tip = tip;
        }
        let paused = app.engine.sync_view().paused_all;
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

/// Development builds can point the app at a plain folder instead of Koofr,
/// so the whole flow can be tried without an account.
fn test_remote() -> Option<String> {
    #[cfg(debug_assertions)]
    if let Ok(dir) = std::env::var("SYNCHOPPER_TEST_REMOTE") {
        return Some(dir);
    }
    None
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|handle, argv, _cwd| {
            // A second launch lands here, inside the copy that is already
            // running. Hand the work to the async runtime and return at once
            // so the second copy can exit.
            let app = handle.state::<Arc<App>>().inner().clone();
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                if !app.handle_args(&argv) {
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
        .setup(|tauri_app| {
            let handle = tauri_app.handle().clone();
            let dirty = Arc::new(AtomicBool::new(true));
            let host = Arc::new(TauriHost { handle: handle.clone(), dirty: dirty.clone() });
            let test = test_remote();
            let app = App::new(
                data_dir(&handle),
                test.clone().unwrap_or_else(|| "koofr:".to_string()),
                test.is_some(),
                host,
            );
            handle.manage(app.clone());
            build_tray(&handle)?;

            let args: Vec<String> = std::env::args().collect();
            let minimized = args.iter().any(|a| a == "--minimized");
            let setup_done = app.core.settings().setup_complete;
            if !(minimized && setup_done) {
                show_main(&handle, None);
            }

            tauri::async_runtime::spawn(state_pump(handle.clone(), app.clone(), dirty));
            let starter = app.clone();
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
                let app = window.state::<Arc<App>>().inner().clone();
                let s = app.core.settings();
                if !s.tray_hint_shown && s.notify_level != "none" {
                    app.core.update(|s| s.tray_hint_shown = true);
                    app.core.host.notify(
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
            connect,
            disconnect,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running SyncHopper");
}
