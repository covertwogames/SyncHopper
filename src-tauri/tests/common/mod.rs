#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use synchopper_lib::app::App;
use synchopper_lib::core::Host;
use synchopper_lib::engine::{PairView, Phase};

#[derive(Default)]
pub struct TestHost {
    pub notes: Mutex<Vec<(String, String)>>,
    pub shown: Mutex<Vec<Option<String>>>,
}

impl Host for TestHost {
    fn state_changed(&self) {}
    fn notify(&self, title: &str, body: &str) {
        self.notes.lock().unwrap().push((title.to_string(), body.to_string()));
    }
    fn set_autostart(&self, _on: bool) -> Result<(), String> {
        Ok(())
    }
    fn exe_path(&self) -> Option<PathBuf> {
        None
    }
    fn show_window(&self, view: Option<&str>) {
        self.shown.lock().unwrap().push(view.map(|v| v.to_string()));
    }
}

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub app: Arc<App>,
    pub host: Arc<TestHost>,
}

impl Rig {
    /// A fresh app whose "Koofr" is a plain folder, already connected.
    pub fn new() -> Rig {
        Rig::with_engine("rclone")
    }

    pub fn with_engine(exe: &str) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("koofr")).unwrap();
        std::fs::create_dir_all(dir.path().join("pc")).unwrap();
        let host = Arc::new(TestHost::default());
        let app = App::new(
            dir.path().join("data"),
            dir.path().join("koofr").to_string_lossy().to_string(),
            true,
            host.clone(),
        );
        let exe = PathBuf::from(exe);
        app.core.set_engine(|e| {
            e.state = "ready".into();
            e.version = "test".into();
            e.exe = Some(exe);
        });
        app.core.update(|s| {
            s.setup_complete = true;
            s.account_email = "tester@example.com".into();
            s.notify_level = "all".into();
        });
        Rig { dir, app, host }
    }

    pub fn pc(&self, rel: &str) -> PathBuf {
        self.dir.path().join("pc").join(rel)
    }

    pub fn koofr(&self, rel: &str) -> PathBuf {
        self.dir.path().join("koofr").join(rel)
    }

    /// Adds a synced folder and runs its first sync.
    pub async fn pair(&self, name: &str) -> String {
        let local = self.pc(name);
        let pair = self.app.engine.add_pair(&local.to_string_lossy(), name).expect("pair should be accepted");
        self.app.engine.drain().await;
        pair.id
    }

    pub async fn sync(&self, id: &str) {
        self.app.engine.sync_now(Some(id));
        self.app.engine.drain().await;
    }

    pub fn view(&self, id: &str) -> PairView {
        self.app.engine.pair_views().into_iter().find(|p| p.id == id).expect("pair exists")
    }

    /// Everything known about a folder, for failure messages.
    pub fn dump(&self, id: &str) -> String {
        let v = self.view(id);
        let log = std::fs::read_to_string(self.app.core.log_path()).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(25).collect();
        format!(
            "phase={:?} error={:?} details={:?} approval={:?} skipped={:?}\nlog tail:\n{}",
            v.phase, v.error, v.details, v.approval, v.skipped,
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        )
    }

    pub fn phase(&self, id: &str) -> Phase {
        self.view(id).phase
    }
}

pub fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|_| "<missing>".to_string())
}

pub fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// rclone compares modified times, so edits made back to back need a gap.
pub async fn tick() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

pub async fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("timed out waiting for: {what}");
}
