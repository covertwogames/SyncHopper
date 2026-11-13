//! State shared by every part of the app: where things live on disk, the
//! saved settings, the sync engine program and the log file.

use crate::config::{self, Pair, Settings};
use crate::rclone::Rclone;
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
    /// "checking", "ready", "missing", "downloading" or "error".
    pub state: String,
    pub version: String,
    pub percent: u8,
    pub message: String,
    pub managed: bool,
    #[serde(skip)]
    pub exe: Option<PathBuf>,
}

pub struct Core {
    pub data: PathBuf,
    /// "koofr:" normally. Tests point this at a plain folder.
    pub remote_root: String,
    pub test_mode: bool,
    pub host: Arc<dyn Host>,
    settings: Mutex<Settings>,
    engine: Mutex<EngineInfo>,
    log_lock: Mutex<()>,
    runtime: std::sync::OnceLock<tokio::runtime::Handle>,
}

impl Core {
    pub fn new(data: PathBuf, remote_root: String, test_mode: bool, host: Arc<dyn Host>) -> Arc<Core> {
        let _ = std::fs::create_dir_all(&data);
        let settings = config::load(&data.join("settings.json"));
        Arc::new(Core {
            data,
            remote_root,
            test_mode,
            host,
            settings: Mutex::new(settings),
            engine: Mutex::new(EngineInfo { state: "checking".into(), ..Default::default() }),
            log_lock: Mutex::new(()),
            runtime: std::sync::OnceLock::new(),
        })
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

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Changes the settings, saves them and refreshes the screens.
    pub fn update<R>(&self, change: impl FnOnce(&mut Settings) -> R) -> R {
        let (result, snapshot) = {
            let mut s = self.settings.lock().unwrap();
            let r = change(&mut s);
            s.sanitize();
            (r, s.clone())
        };
        if let Err(e) = config::save(&self.data.join("settings.json"), &snapshot) {
            self.log(&format!("Could not save settings: {e}"));
        }
        self.host.state_changed();
        result
    }

    pub fn pair(&self, id: &str) -> Option<Pair> {
        self.settings.lock().unwrap().pairs.iter().find(|p| p.id == id).cloned()
    }

    pub fn update_pair(&self, id: &str, change: impl FnOnce(&mut Pair)) {
        self.update(|s| {
            if let Some(p) = s.pairs.iter_mut().find(|p| p.id == id) {
                change(p);
            }
        });
    }

    pub fn engine_info(&self) -> EngineInfo {
        self.engine.lock().unwrap().clone()
    }

    pub fn set_engine(&self, change: impl FnOnce(&mut EngineInfo)) {
        change(&mut self.engine.lock().unwrap());
        self.host.state_changed();
    }

    pub fn config_path(&self) -> PathBuf {
        self.data.join("rclone.conf")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.data.join("cache")
    }

    pub fn rclone(&self) -> Result<Rclone, String> {
        let exe = self
            .engine
            .lock()
            .unwrap()
            .exe
            .clone()
            .ok_or_else(|| "The sync engine is not installed yet.".to_string())?;
        Ok(Rclone { exe, config: self.config_path(), cache: self.cache_dir() })
    }

    /// The full rclone path for a folder inside Koofr.
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
        let level = self.settings.lock().unwrap().notify_level.clone();
        let allowed = match level.as_str() {
            "none" => false,
            "all" => true,
            _ => kind == Notice::Problem,
        };
        if allowed {
            self.host.notify(title, body);
        }
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
