//! The optional Koofr drive: the whole account shown as a drive letter.
//! Files there stay online and are fetched when opened.

use crate::core::Core;
use crate::platform;
use crate::rclone::{friendly_error, is_blocked_message};
use crate::util::Cancel;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Serialize, Default)]
pub struct DriveView {
    pub enabled: bool,
    pub supported: bool,
    /// "off", "needs_driver", "installing", "starting", "connected" or "error".
    pub state: String,
    pub letter: String,
    pub message: String,
    pub free_letters: Vec<String>,
}

#[derive(Default)]
struct Inner {
    state: String,
    message: String,
    cancel: Option<Cancel>,
    generation: u64,
    letter: String,
}

pub struct Drive {
    core: Arc<Core>,
    inner: Mutex<Inner>,
}

impl Drive {
    pub fn new(core: Arc<Core>) -> Arc<Drive> {
        Arc::new(Drive { core, inner: Mutex::new(Inner { state: "off".into(), ..Default::default() }) })
    }

    pub fn view(&self) -> DriveView {
        let s = self.core.settings();
        let inner = self.inner.lock().unwrap();
        let mut letters = platform::free_letters();
        if !letters.contains(&s.drive_letter) {
            letters.push(s.drive_letter.clone());
            letters.sort();
        }
        DriveView {
            enabled: s.feature_drive,
            supported: cfg!(windows),
            state: inner.state.clone(),
            letter: s.drive_letter.clone(),
            message: inner.message.clone(),
            free_letters: letters,
        }
    }

    fn set(&self, state: &str, message: &str) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.state = state.into();
            inner.message = message.into();
        }
        self.core.host.state_changed();
    }

    /// Disconnects the drive if it is connected.
    pub fn stop(&self) {
        let cancel = {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            inner.state = "off".into();
            inner.message.clear();
            inner.cancel.take()
        };
        if let Some(c) = cancel {
            c.cancel();
        }
        self.core.host.state_changed();
    }

    pub async fn restart(self: &Arc<Self>) {
        self.stop();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        self.apply().await;
    }

    /// Brings the drive in line with the settings: connect it, or
    /// disconnect it, or report what is in the way.
    pub async fn apply(self: &Arc<Self>) {
        let s = self.core.settings();
        if !s.feature_drive || !s.setup_complete {
            return self.stop();
        }
        if !cfg!(windows) {
            return self.set("error", "The Koofr drive is only available on Windows.");
        }
        {
            let inner = self.inner.lock().unwrap();
            if inner.cancel.is_some() && inner.letter == s.drive_letter {
                return;
            }
            if inner.state == "installing" {
                return;
            }
        }
        self.stop();
        let rc = match self.core.rclone() {
            Ok(rc) => rc,
            Err(e) => return self.set("error", &e),
        };
        if !platform::winfsp_installed().await {
            return self.set("needs_driver", "");
        }
        let letter = s.drive_letter.clone();
        if platform::letter_in_use(&letter) {
            return self.set("error", &format!("Drive letter {letter}: is already in use. Pick another letter in Settings."));
        }
        let cancel = Cancel::default();
        let generation = {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            inner.cancel = Some(cancel.clone());
            inner.letter = letter.clone();
            inner.state = "starting".into();
            inner.message.clear();
            inner.generation
        };
        self.core.host.state_changed();

        let mut args: Vec<String> = vec![
            "mount".into(),
            self.core.remote(""),
            format!("{letter}:"),
            "--vfs-cache-mode".into(),
            "full".into(),
            "--vfs-cache-max-size".into(),
            "10G".into(),
            "--vfs-cache-max-age".into(),
            "24h".into(),
            "--dir-cache-time".into(),
            "1m".into(),
            "--volname".into(),
            "Koofr".into(),
            "--use-json-log".into(),
            "--log-level".into(),
            "NOTICE".into(),
            "--color".into(),
            "NEVER".into(),
        ];
        if !s.use_http2 {
            args.push("--disable-http2".into());
        }
        self.core.log(&format!("[drive] Connecting {letter}:"));

        // Report "connected" once Windows can see the drive letter.
        let me = self.clone();
        let watch_letter = letter.clone();
        self.core.spawn(async move {
            for _ in 0..120 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let current = {
                    let inner = me.inner.lock().unwrap();
                    inner.generation == generation && inner.state == "starting"
                };
                if !current {
                    return;
                }
                if platform::letter_in_use(&watch_letter) {
                    me.set("connected", "");
                    return;
                }
            }
        });

        let me = self.clone();
        self.core.spawn(async move {
            let core = me.core.clone();
            let res = rc
                .run_logged(&args, &cancel, |line| {
                    if line.is_error() {
                        core.log(&format!("[drive] {} {}", line.level, line.msg));
                    }
                })
                .await;
            let mut inner = me.inner.lock().unwrap();
            if inner.generation != generation {
                return;
            }
            inner.cancel = None;
            if res.cancelled {
                inner.state = "off".into();
            } else {
                let raw = res
                    .spawn_error
                    .clone()
                    .or_else(|| res.errors.last().map(|l| l.msg.clone()))
                    .unwrap_or_else(|| format!("The drive stopped (code {}).", res.exit));
                inner.state = "error".into();
                inner.message = friendly_error(&raw);
                me.core.log(&format!("[drive] Stopped: {raw}"));
            }
            drop(inner);
            if res.spawn_error.as_deref().is_some_and(is_blocked_message) {
                me.core.engine_blocked(res.spawn_error.as_deref().unwrap_or_default());
            }
            me.core.host.state_changed();
        });
    }

    /// Installs the driver the drive needs, then connects.
    pub async fn install_driver(self: &Arc<Self>) {
        {
            let inner = self.inner.lock().unwrap();
            if inner.state == "installing" {
                return;
            }
        }
        self.set("installing", "");
        self.core.log("[drive] Installing the WinFsp driver with winget");
        match platform::install_winfsp().await {
            Ok(()) => {
                self.set("off", "");
                self.apply().await;
            }
            Err(e) => {
                self.core.log(&format!("[drive] Driver install failed: {e}"));
                self.set("needs_driver", &e);
            }
        }
    }
}
