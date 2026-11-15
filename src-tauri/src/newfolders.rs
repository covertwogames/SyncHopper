//! Watches the folder that holds the synced folders (the "Koofr folder" on
//! this PC) for things put straight into it. New folders become synced
//! folders, a synced folder that was renamed is followed (and renamed in
//! Koofr too), and loose files, which can't sync there, are pointed out.

use crate::config::Pair;
use crate::core::{Core, Notice};
use crate::engine::Engine;
use crate::platform;
use crate::rclone::is_network_error;
use crate::util::{file_name, glob_match, koofr_refuses_chars, new_id, norm_local};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the Koofr folder has to stay unchanged before new things in it
/// are acted on. Leaves time to finish naming a new folder or copying one in.
pub const SETTLE: Duration = Duration::from_secs(30);
/// The wait when a synced folder disappeared, which usually means it was
/// renamed.
const SETTLE_RENAME: Duration = Duration::from_secs(5);
/// Looks anyway every so often, in case a change was missed.
const RESCAN: Duration = Duration::from_secs(5 * 60);

/// Something in the Koofr folder the user should know about, shown as a
/// banner in the main window until it is dealt with or dismissed.
#[derive(Clone, Debug, Serialize)]
pub struct FolderNotice {
    pub id: String,
    /// "clash": Koofr already has a folder with that name.
    /// "not_synced": automatic syncing of new folders is off.
    /// "failed": the folder could not be added.
    /// "files": files were put straight into the Koofr folder.
    /// "rename_kept": a renamed folder still syncs under its old Koofr name.
    pub kind: String,
    pub name: String,
    pub local: String,
    pub message: String,
}

pub struct NewFolders {
    core: Arc<Core>,
    engine: Arc<Engine>,
    notices: Mutex<Vec<FolderNotice>>,
    last_event: Mutex<Option<Instant>>,
    last_scan: Mutex<Option<Instant>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    busy: tokio::sync::Mutex<()>,
}

/// Names compare without regard to case on Windows, like File Explorer.
fn key(name: &str) -> String {
    if cfg!(windows) {
        name.to_lowercase()
    } else {
        name.to_string()
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// True when `outer` is `inner` or one of the folders it is in.
fn holds(outer: &str, inner: &str) -> bool {
    let sep = std::path::MAIN_SEPARATOR;
    inner == outer || (inner.starts_with(outer) && inner[outer.len()..].starts_with(sep))
}

/// The folders and files directly in `base` that people see in File
/// Explorer, as (name, is a folder). Ignored names are left out.
fn visible_entries(base: &Path, ignores: &[String]) -> std::io::Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(base)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(meta) = entry.metadata() else { continue };
        if platform::hidden_entry(&name, &meta) || name.ends_with(".partial") {
            continue;
        }
        if ignores.iter().any(|g| glob_match(g, &name)) {
            continue;
        }
        out.push((name, meta.is_dir()));
    }
    out.sort_by_key(|(n, _)| n.to_lowercase());
    Ok(out)
}

fn names_list(names: &[String]) -> String {
    let shown: Vec<String> = names.iter().take(3).map(|n| format!("\"{n}\"")).collect();
    let more = names.len().saturating_sub(3);
    let mut text = match shown.len() {
        1 => shown[0].clone(),
        2 => format!("{} and {}", shown[0], shown[1]),
        _ => format!("{}, {} and {}", shown[0], shown[1], shown[2]),
    };
    if more > 0 {
        text = format!("{text} and {more} more");
    }
    text
}

impl NewFolders {
    pub fn new(core: Arc<Core>, engine: Arc<Engine>) -> Arc<NewFolders> {
        Arc::new(NewFolders {
            core,
            engine,
            notices: Mutex::new(Vec::new()),
            last_event: Mutex::new(None),
            last_scan: Mutex::new(None),
            watcher: Mutex::new(None),
            busy: tokio::sync::Mutex::new(()),
        })
    }

    /// Starts watching and the background check. Call once.
    pub fn start(self: &Arc<Self>) {
        self.rewatch();
        let me = self.clone();
        self.core.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if me.due() {
                    me.scan().await;
                }
            }
        });
    }

    fn due(&self) -> bool {
        let scanned = *self.last_scan.lock().unwrap();
        let Some(scanned) = scanned else { return true };
        if scanned.elapsed() >= RESCAN {
            return true;
        }
        let event = *self.last_event.lock().unwrap();
        let Some(event) = event else { return false };
        let wait = if self.synced_folder_missing() { SETTLE_RENAME } else { SETTLE };
        event.elapsed() >= wait
    }

    /// The Koofr folder, when watching it makes sense.
    fn base(&self) -> Option<PathBuf> {
        let s = self.core.settings();
        let base = s.base_folder.trim();
        if !s.setup_complete || !s.feature_sync || base.is_empty() {
            return None;
        }
        Some(PathBuf::from(base))
    }

    fn synced_folder_missing(&self) -> bool {
        let Some(base) = self.base() else { return false };
        let nb = norm_local(&base.to_string_lossy());
        self.core.settings().pairs.iter().any(|p| {
            let parent = Path::new(&p.local).parent().map(|x| norm_local(&x.to_string_lossy()));
            parent.as_deref() == Some(nb.as_str()) && !Path::new(&p.local).exists()
        })
    }

    /// (Re)starts watching the Koofr folder. Only what is directly in it is
    /// watched; the synced folders inside are watched by the sync engine.
    pub fn rewatch(self: &Arc<Self>) {
        let mut slot = self.watcher.lock().unwrap();
        *slot = None;
        let Some(base) = self.base() else { return };
        if !base.is_dir() {
            return;
        }
        let me = Arc::downgrade(self);
        let handler = move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            if let Some(me) = me.upgrade() {
                *me.last_event.lock().unwrap() = Some(Instant::now());
            }
        };
        match notify::recommended_watcher(handler) {
            Ok(mut w) => {
                if let Err(e) = w.watch(&base, RecursiveMode::NonRecursive) {
                    self.core.log(&format!("Could not watch {}: {e}", base.display()));
                    return;
                }
                *slot = Some(w);
            }
            Err(e) => self.core.log(&format!("Could not watch the Koofr folder: {e}")),
        }
    }

    /// Notices that still apply. Ones about folders that are now synced or
    /// gone are dropped.
    pub fn notices(&self) -> Vec<FolderNotice> {
        let pairs: HashSet<String> = self.core.settings().pairs.iter().map(|p| norm_local(&p.local)).collect();
        let mut list = self.notices.lock().unwrap();
        list.retain(|n| match n.kind.as_str() {
            "rename_kept" => true,
            // Gone once none of the loose files are left.
            "files" => n.name.split('\n').any(|f| !f.is_empty() && Path::new(&n.local).join(f).exists()),
            _ => Path::new(&n.local).exists() && !pairs.contains(&norm_local(&n.local)),
        });
        list.clone()
    }

    pub fn dismiss(&self, id: &str) {
        self.notices.lock().unwrap().retain(|n| n.id != id);
        self.core.host.state_changed();
    }

    fn add_notice(&self, kind: &str, name: &str, local: &str, message: String) {
        let mut list = self.notices.lock().unwrap();
        list.retain(|n| !(n.kind == kind && same_name(&n.local, local)));
        list.push(FolderNotice { id: new_id("n"), kind: kind.into(), name: name.into(), local: local.into(), message });
        drop(list);
        self.core.host.state_changed();
    }

    /// Looks at what is directly in the Koofr folder and deals with anything
    /// new. Safe to call at any time.
    pub async fn scan(&self) {
        let _one_at_a_time = self.busy.lock().await;
        *self.last_event.lock().unwrap() = None;
        *self.last_scan.lock().unwrap() = Some(Instant::now());
        let Some(base) = self.base() else { return };
        if !base.is_dir() {
            return;
        }
        let s = self.core.settings();
        let entries = match visible_entries(&base, &s.ignore_patterns) {
            Ok(e) => e,
            Err(e) => {
                self.core.log(&format!("Could not look in {}: {e}", base.display()));
                return;
            }
        };
        let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
        let nb = norm_local(&base.to_string_lossy());

        // What is there the first time is never treated as new.
        if !s.base_known_ready {
            self.core.update(|s| {
                s.base_known = names.clone();
                s.base_known_ready = true;
            });
            return;
        }
        // A synced folder that is the Koofr folder, or holds it, already
        // syncs everything put into it.
        if s.pairs.iter().any(|p| holds(&norm_local(&p.local), &nb)) {
            self.core.update(|s| s.base_known = names.clone());
            return;
        }

        let known: HashSet<String> = s.base_known.iter().map(|n| key(n)).collect();
        let synced: HashSet<String> = s.pairs.iter().map(|p| norm_local(&p.local)).collect();
        let mut new_dirs: Vec<String> = Vec::new();
        let mut new_files: Vec<String> = Vec::new();
        for (name, is_dir) in &entries {
            if known.contains(&key(name)) {
                continue;
            }
            if *is_dir {
                if !synced.contains(&norm_local(&base.join(name).to_string_lossy())) {
                    new_dirs.push(name.clone());
                }
            } else {
                new_files.push(name.clone());
            }
        }
        // Names that have to be looked at again next time, for example
        // because Koofr could not be reached.
        let mut later: HashSet<String> = HashSet::new();
        let mut koofr_dirs: Option<Result<Vec<String>, String>> = None;

        // Synced folders that were renamed (or moved in here from somewhere
        // else on the same drive): the old one is gone and a "new" folder is
        // the same folder under another name.
        let gone: Vec<Pair> = s.pairs.iter().filter(|p| !p.local_id.is_empty() && !Path::new(&p.local).exists()).cloned().collect();
        for pair in gone {
            let Some(pos) = new_dirs.iter().position(|n| platform::folder_id(&base.join(n)).as_deref() == Some(pair.local_id.as_str())) else {
                continue;
            };
            let name = new_dirs.remove(pos);
            let new_local = base.join(&name).to_string_lossy().to_string();
            let old_name = file_name(&pair.local);
            let was_here = Path::new(&pair.local).parent().map(|x| norm_local(&x.to_string_lossy())).as_deref() == Some(nb.as_str());
            // Rename the Koofr folder too when this was a rename inside the
            // Koofr folder and the Koofr folder had the same name as the
            // folder on this PC, at the top of Koofr.
            let follow_in_koofr = was_here
                && name != old_name
                && !pair.remote.is_empty()
                && !pair.remote.contains('/')
                && same_name(&pair.remote, &old_name);
            if !follow_in_koofr {
                let _ = self.engine.rename_pair(&pair.id, &new_local, None).await;
                continue;
            }
            if koofr_refuses_chars(&name) {
                let _ = self.engine.rename_pair(&pair.id, &new_local, None).await;
                let msg = format!(
                    "Koofr doesn't allow emoji and some other symbols in folder names, so this folder keeps syncing to \"{}\" in Koofr.",
                    pair.remote
                );
                self.add_notice("rename_kept", &name, &new_local, msg.clone());
                self.core.notify(Notice::Problem, &format!("\"{name}\" was not renamed in Koofr"), &msg);
                continue;
            }
            if koofr_dirs.is_none() {
                koofr_dirs = Some(self.koofr_dirs().await);
            }
            match koofr_dirs.as_ref().unwrap() {
                Err(_) => {
                    later.insert(key(&name));
                }
                Ok(dirs) => {
                    let taken = dirs.iter().any(|d| same_name(d, &name)) && !same_name(&name, &pair.remote);
                    if taken {
                        let _ = self.engine.rename_pair(&pair.id, &new_local, None).await;
                        let msg = format!("Koofr already has a folder called \"{name}\", so this folder keeps syncing to \"{}\" in Koofr.", pair.remote);
                        self.add_notice("rename_kept", &name, &new_local, msg.clone());
                        self.core.notify(Notice::Problem, &format!("\"{name}\" was not renamed in Koofr"), &msg);
                    } else {
                        match self.engine.rename_pair(&pair.id, &new_local, Some(&name)).await {
                            Ok(()) => {
                                self.core.notify(Notice::Info, &format!("Renamed \"{old_name}\" to \"{name}\""), "The folder was renamed in Koofr too.");
                            }
                            Err(e) => {
                                let msg = format!(
                                    "Koofr would not rename \"{}\" to \"{name}\", so this folder keeps syncing to \"{}\" in Koofr. {}",
                                    pair.remote,
                                    pair.remote,
                                    crate::rclone::friendly_error(&e)
                                );
                                self.add_notice("rename_kept", &name, &new_local, msg.clone());
                                self.core.notify(Notice::Problem, &format!("\"{name}\" was not renamed in Koofr"), &msg);
                            }
                        }
                    }
                }
            }
        }

        // Brand new folders.
        for name in new_dirs {
            let local = base.join(&name).to_string_lossy().to_string();
            if !s.auto_add_folders {
                let msg = format!("\"{name}\" is in your Koofr folder but isn't synced. Sync it to keep a copy in Koofr.");
                self.add_notice("not_synced", &name, &local, msg.clone());
                self.core.notify_folder(Notice::Problem, &format!("\"{name}\" isn't synced yet"), &msg, &local);
                continue;
            }
            if koofr_dirs.is_none() {
                koofr_dirs = Some(self.koofr_dirs().await);
            }
            let dirs = match koofr_dirs.as_ref().unwrap() {
                Ok(d) => d,
                Err(_) => {
                    later.insert(key(&name));
                    continue;
                }
            };
            let in_koofr = dirs.iter().any(|d| same_name(d, &name));
            let synced_elsewhere = s.pairs.iter().find(|p| same_name(&p.remote, &name) || p.remote.to_lowercase().starts_with(&format!("{}/", name.to_lowercase())));
            if in_koofr || synced_elsewhere.is_some() {
                let msg = match synced_elsewhere {
                    Some(p) => format!("Koofr's \"{name}\" folder is already synced to {}. Pick a different Koofr folder for this one.", p.local),
                    None => format!("Koofr already has a folder called \"{name}\". Syncing would combine the two, so SyncHopper is checking with you first."),
                };
                self.add_notice("clash", &name, &local, msg.clone());
                self.core.notify_folder(Notice::Problem, &format!("\"{name}\" isn't synced yet"), &msg, &local);
                continue;
            }
            match self.engine.add_pair(&local, &name) {
                Ok(_) => {
                    self.core.log(&format!("Started syncing {local}, a new folder in the Koofr folder"));
                    self.core.notify(Notice::Info, &format!("Now syncing \"{name}\""), "It was added to Koofr. Anything you put in it syncs from now on.");
                }
                Err(e) => {
                    let msg = format!("\"{name}\" could not be synced. {e}");
                    self.add_notice("failed", &name, &local, msg.clone());
                    self.core.notify_folder(Notice::Problem, &format!("\"{name}\" isn't synced"), &msg, &local);
                }
            }
        }

        // Loose files can't sync: only the folders in here do.
        if !new_files.is_empty() {
            let mut all = new_files.clone();
            if let Some(old) = self.notices.lock().unwrap().iter().find(|n| n.kind == "files") {
                all.extend(old.name.split('\n').map(|s| s.to_string()).filter(|n| !n.is_empty() && !new_files.contains(n)));
            }
            all.retain(|n| base.join(n).exists());
            let (title, msg) = if all.len() == 1 {
                (
                    "A file in your Koofr folder isn't synced".to_string(),
                    format!("{} is sitting loose in your Koofr folder. Only the folders in it sync, so move it into one of them.", names_list(&all)),
                )
            } else {
                (
                    "Files in your Koofr folder aren't synced".to_string(),
                    format!("{} are sitting loose in your Koofr folder. Only the folders in it sync, so move them into one of them.", names_list(&all)),
                )
            };
            let mut list = self.notices.lock().unwrap();
            list.retain(|n| n.kind != "files");
            list.push(FolderNotice { id: new_id("n"), kind: "files".into(), name: all.join("\n"), local: base.to_string_lossy().to_string(), message: msg.clone() });
            drop(list);
            self.core.host.state_changed();
            self.core.notify(Notice::Problem, &title, &msg);
        }

        let now_known: Vec<String> = names.into_iter().filter(|n| !later.contains(&key(n))).collect();
        self.core.update(|s| s.base_known = now_known);
    }

    /// Names of the folders at the top of Koofr.
    async fn koofr_dirs(&self) -> Result<Vec<String>, String> {
        let rc = self.core.rclone()?;
        rc.list_dirs(&self.core.remote("")).await.map_err(|e| {
            if is_network_error(&e) {
                self.core.log(&format!("Could not check Koofr for new folders, will try again: {e}"));
            } else {
                self.core.log(&format!("Could not list the folders in Koofr: {e}"));
            }
            e
        })
    }
}
