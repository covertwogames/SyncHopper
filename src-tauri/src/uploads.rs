//! One-way uploads: files and folders sent straight to Koofr without being
//! kept in sync.

use crate::core::{Core, Notice};
use crate::rclone::{friendly_error, is_blocked_message};
use crate::util::{clean_remote, file_name, Cancel};
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Serialize)]
pub struct UploadView {
    pub id: u64,
    pub name: String,
    pub path: String,
    pub dest: String,
    /// "waiting", "uploading", "done", "failed" or "cancelled".
    pub state: String,
    pub percent: f64,
    pub speed: f64,
    pub error: Option<String>,
}

struct Item {
    view: UploadView,
    is_dir: bool,
    cancel: Cancel,
}

#[derive(Default)]
struct Inner {
    items: Vec<Item>,
    next_id: u64,
    active_weight: u32,
    finished: u32,
    failed: u32,
}

pub struct Uploads {
    core: Arc<Core>,
    inner: Mutex<Inner>,
}

impl Uploads {
    pub fn new(core: Arc<Core>) -> Arc<Uploads> {
        Arc::new(Uploads { core, inner: Mutex::new(Inner::default()) })
    }

    pub fn views(&self) -> Vec<UploadView> {
        self.inner.lock().unwrap().items.iter().map(|i| i.view.clone()).collect()
    }

    pub fn active(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.items.iter().filter(|i| i.view.state == "waiting" || i.view.state == "uploading").count()
    }

    /// Queues files and folders for upload into `dest`. Returns how many
    /// were added.
    pub fn add(self: &Arc<Self>, paths: Vec<String>, dest: &str) -> usize {
        let dest = clean_remote(dest);
        let mut added = 0;
        {
            let mut inner = self.inner.lock().unwrap();
            for path in paths {
                let p = Path::new(&path);
                if !p.exists() {
                    continue;
                }
                let busy = inner.items.iter().any(|i| {
                    i.view.path == path && i.view.dest == dest && (i.view.state == "waiting" || i.view.state == "uploading")
                });
                if busy {
                    continue;
                }
                inner.next_id += 1;
                let id = inner.next_id;
                inner.items.push(Item {
                    view: UploadView {
                        id,
                        name: file_name(&path),
                        path: path.clone(),
                        dest: dest.clone(),
                        state: "waiting".into(),
                        percent: 0.0,
                        speed: 0.0,
                        error: None,
                    },
                    is_dir: p.is_dir(),
                    cancel: Cancel::default(),
                });
                added += 1;
            }
        }
        self.pump();
        self.core.host.state_changed();
        added
    }

    /// Starts waiting uploads while there is room. A folder takes all the
    /// room because it already uploads several files at once.
    fn pump(self: &Arc<Self>) {
        let limit = self.core.settings().transfers.max(1);
        loop {
            let next = {
                let mut inner = self.inner.lock().unwrap();
                let active = inner.active_weight;
                let found = inner.items.iter_mut().find(|i| i.view.state == "waiting").and_then(|item| {
                    let weight = if item.is_dir { limit } else { 1 };
                    if active == 0 || active + weight <= limit {
                        item.view.state = "uploading".into();
                        Some((item.view.id, weight))
                    } else {
                        None
                    }
                });
                if let Some((_, weight)) = found {
                    inner.active_weight += weight;
                }
                found
            };
            let Some((id, weight)) = next else { break };
            let me = self.clone();
            self.core.spawn(async move { me.run_item(id, weight).await });
        }
    }

    fn set<R>(&self, id: u64, f: impl FnOnce(&mut Item) -> R) -> Option<R> {
        let mut inner = self.inner.lock().unwrap();
        inner.items.iter_mut().find(|i| i.view.id == id).map(f)
    }

    async fn run_item(self: Arc<Self>, id: u64, weight: u32) {
        let s = self.core.settings();
        let Some((path, dest, is_dir, cancel)) =
            self.set(id, |i| (i.view.path.clone(), i.view.dest.clone(), i.is_dir, i.cancel.clone()))
        else {
            return;
        };
        let target = if is_dir {
            let name = file_name(&path);
            self.core.remote(&if dest.is_empty() { name } else { format!("{dest}/{name}") })
        } else {
            self.core.remote(&dest)
        };
        let mut args: Vec<String> = vec![
            "copy".into(),
            path.clone(),
            target,
            "--transfers".into(),
            if is_dir { s.transfers.to_string() } else { "1".into() },
            "--retries".into(),
            "3".into(),
            "--retries-sleep".into(),
            "5s".into(),
            "--use-json-log".into(),
            "--log-level".into(),
            "INFO".into(),
            "--stats".into(),
            "1s".into(),
            "--stats-log-level".into(),
            "NOTICE".into(),
            "--color".into(),
            "NEVER".into(),
        ];
        if is_dir {
            args.push("--create-empty-src-dirs".into());
        }
        if !s.use_http2 {
            args.push("--disable-http2".into());
        }
        if let Some(limit) = s.bwlimit() {
            args.push("--bwlimit".into());
            args.push(limit);
        }
        self.core.log(&format!("[upload] {path} starting"));
        let result = match self.core.rclone() {
            Ok(rc) => {
                let me = self.clone();
                Ok(
                    rc.run_logged(&args, &cancel, |line| {
                        if let Some(st) = &line.stats {
                            me.set(id, |i| {
                                if st.total_bytes > 0 {
                                    i.view.percent = (st.bytes as f64 / st.total_bytes as f64 * 100.0).min(100.0);
                                }
                                i.view.speed = st.speed;
                            });
                            me.core.host.state_changed();
                        } else if line.is_error() {
                            me.core.log(&format!("[upload] {} {} {}", line.level, line.object, line.msg));
                        }
                    })
                    .await,
                )
            }
            Err(e) => Err(e),
        };
        let (state, error) = match &result {
            Err(e) => ("failed", Some(e.clone())),
            Ok(r) if r.cancelled => ("cancelled", None),
            Ok(r) if r.exit == 0 => ("done", None),
            Ok(r) => {
                let raw = r
                    .spawn_error
                    .clone()
                    .or_else(|| r.errors.iter().find(|l| !l.msg.starts_with("Failed to copy with")).map(|l| l.msg.clone()))
                    .or_else(|| r.errors.first().map(|l| l.msg.clone()))
                    .unwrap_or_else(|| format!("The upload stopped unexpectedly (code {}).", r.exit));
                if is_blocked_message(&raw) {
                    self.core.engine_blocked(&raw);
                }
                ("failed", Some(friendly_error(&raw)))
            }
        };
        self.core.log(&format!("[upload] {path} {state} {}", error.clone().unwrap_or_default()));
        let summary = {
            let mut inner = self.inner.lock().unwrap();
            inner.active_weight = inner.active_weight.saturating_sub(weight);
            if let Some(i) = inner.items.iter_mut().find(|i| i.view.id == id) {
                i.view.state = state.into();
                i.view.error = error;
                i.view.speed = 0.0;
                if state == "done" {
                    i.view.percent = 100.0;
                }
            }
            match state {
                "done" => inner.finished += 1,
                "failed" => inner.failed += 1,
                _ => {}
            }
            let still_going = inner.items.iter().any(|i| i.view.state == "waiting" || i.view.state == "uploading");
            if still_going {
                None
            } else {
                let counts = (inner.finished, inner.failed);
                inner.finished = 0;
                inner.failed = 0;
                Some(counts)
            }
        };
        self.core.host.state_changed();
        self.pump();
        if let Some((done, failed)) = summary {
            let plural = |n: u32| if n == 1 { "upload" } else { "uploads" };
            if failed > 0 {
                self.core.notify(
                    Notice::Problem,
                    &format!("{failed} {} to Koofr failed", plural(failed)),
                    "Open SyncHopper to see what went wrong and try again.",
                );
            } else if done > 0 {
                self.core.notify(Notice::Info, "Uploads finished", &format!("{done} {} to Koofr finished.", plural(done)));
            }
        }
    }

    /// Stops every running and waiting upload.
    pub fn cancel_all(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            for i in inner.items.iter_mut() {
                match i.view.state.as_str() {
                    "waiting" => i.view.state = "cancelled".into(),
                    "uploading" => i.cancel.cancel(),
                    _ => {}
                }
            }
        }
        self.core.host.state_changed();
    }

    pub fn retry(self: &Arc<Self>, id: u64) {
        self.set(id, |i| {
            if i.view.state == "failed" || i.view.state == "cancelled" {
                i.view.state = "waiting".into();
                i.view.error = None;
                i.view.percent = 0.0;
                i.cancel = Cancel::default();
            }
        });
        self.pump();
        self.core.host.state_changed();
    }

    /// Removes finished, failed and cancelled rows from the list.
    pub fn clear_finished(&self) {
        self.inner.lock().unwrap().items.retain(|i| i.view.state == "waiting" || i.view.state == "uploading");
        self.core.host.state_changed();
    }
}
