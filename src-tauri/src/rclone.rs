//! Everything that talks to the rclone program: finding it, downloading it,
//! running it and reading its log output.

use crate::platform;
use crate::providers::Provider;
use crate::util::{strip_ansi, Cancel};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Oldest rclone that has the two-way sync options this app relies on.
pub const MIN_VERSION: (u32, u32) = (1, 66);
pub const DOWNLOAD_BASE: &str = "https://downloads.rclone.org";

/// Where release builds get the sync engine: copies of rclone signed with
/// Cover Two's code signing certificate, so Windows (Smart App Control) lets
/// SyncHopper run them. The signing job in .github/workflows/engine.yml keeps
/// it current. The list of files (engine.json) is itself signed with the
/// same key as SyncHopper's own updates, and each file is checked against it.
pub const ENGINE_CHANNEL_URL: &str = "https://github.com/covertwogames/SyncHopper/releases/download/engine/engine.json";

/// Starts every message about the system refusing to run the engine, so any
/// part of the app can recognise it.
pub const BLOCKED_PREFIX: &str = "The sync engine was blocked from running";

/// What people see when the system won't run the engine.
pub const BLOCKED_TEXT: &str = "This PC blocked SyncHopper's sync engine, so nothing can sync right now. Usually that's Smart App Control or antivirus software. SyncHopper tries again by itself.";

/// What SyncHopper is told when the engine program can't be started.
pub fn spawn_failure(exe: &Path, e: &std::io::Error) -> String {
    if blocked_by_system(exe, e) {
        format!("{BLOCKED_PREFIX} ({}): {e}", exe.display())
    } else {
        format!("Could not start the sync engine: {e}")
    }
}

/// True when the error text says the system refused to run the engine.
pub fn is_blocked_message(msg: &str) -> bool {
    msg.contains(BLOCKED_PREFIX) || msg.contains(BLOCKED_TEXT)
}

/// True when the system refused to run the program, as opposed to it being
/// missing. On Windows that is Smart App Control and other application
/// control policies (error 4551 and its neighbours), group policy (1260),
/// antivirus (225, 226) or access to the program being refused even though
/// it is there (5). On other systems it is permission to run the file being
/// refused even though it is there.
pub fn blocked_by_system(exe: &Path, e: &std::io::Error) -> bool {
    blocked_code(e.raw_os_error(), &e.to_string(), cfg!(windows), exe.is_file())
}

fn blocked_code(code: Option<i32>, text: &str, windows: bool, file_exists: bool) -> bool {
    let text = text.to_lowercase();
    if text.contains("application control policy") || text.contains("blocked by group policy") {
        return true;
    }
    match code {
        Some(c) if windows => (4550..=4599).contains(&c) || c == 1260 || c == 225 || c == 226 || (c == 5 && file_exists),
        // EACCES: the file is there but may not be run.
        Some(13) => file_exists,
        _ => false,
    }
}

/// Where release builds get the engine and how they check it.
#[derive(Clone, Debug)]
pub struct EngineChannel {
    pub manifest_url: String,
    /// SyncHopper's update key, as in tauri.conf.json (plugins > updater).
    pub pubkey: String,
}

/// engine.json: the current signed engine for each kind of PC.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EngineManifest {
    pub version: String,
    /// Keyed like "windows-amd64".
    pub files: HashMap<String, EngineFile>,
    /// rclone's license, which goes along with every copy.
    #[serde(default)]
    pub license: Option<EngineFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EngineFile {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

/// Records where the installed engine came from, next to it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EngineSource {
    /// "signed" for a copy from the signed channel, "rclone.org" otherwise.
    pub source: String,
    pub version: String,
    pub sha256: String,
}

/// The key for this kind of PC in engine.json, like "windows-amd64".
pub fn platform_key() -> String {
    let (os, arch) = platform_tag();
    format!("{os}-{arch}")
}

/// Checks a signature made with `tauri signer sign` against SyncHopper's
/// update key. Both are base64 text, as Tauri writes them.
pub fn verify_signature(data: &[u8], signature_b64: &str, pubkey_b64: &str) -> Result<(), String> {
    use base64::Engine as _;
    let decode = |b64: &str, what: &str| -> Result<String, String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|_| format!("The {what} could not be read."))?;
        String::from_utf8(bytes).map_err(|_| format!("The {what} could not be read."))
    };
    let key = minisign_verify::PublicKey::decode(&decode(pubkey_b64, "update key")?).map_err(|e| format!("The update key could not be read: {e}"))?;
    let sig = minisign_verify::Signature::decode(&decode(signature_b64, "signature")?).map_err(|e| format!("The signature could not be read: {e}"))?;
    // Same rule as Tauri's own update check, so whatever `tauri signer sign`
    // writes is accepted here too.
    key.verify(data, &sig, true).map_err(|_| "The signature does not match.".to_string())
}

fn channel_url(channel: &EngineChannel, name: &str) -> String {
    let base = channel.manifest_url.rsplit_once('/').map(|(b, _)| b).unwrap_or(&channel.manifest_url);
    format!("{base}/{name}")
}

/// Fetches engine.json and checks its signature.
pub async fn channel_manifest(channel: &EngineChannel) -> Result<EngineManifest, String> {
    let client = http_client()?;
    let fail = |what: &str, e: String| format!("Could not download the sync engine ({what}): {e}");
    let get = |url: String| {
        let client = client.clone();
        async move {
            let resp = tokio::time::timeout(Duration::from_secs(60), client.get(url).send())
                .await
                .map_err(|_| "no answer".to_string())?
                .and_then(|r| r.error_for_status())
                .map_err(|e| e.to_string())?;
            resp.bytes().await.map_err(|e| e.to_string())
        }
    };
    let body = get(channel.manifest_url.clone()).await.map_err(|e| fail("list", e))?;
    let sig = get(format!("{}.sig", channel.manifest_url)).await.map_err(|e| fail("signature", e))?;
    verify_signature(&body, &String::from_utf8_lossy(&sig), &channel.pubkey)
        .map_err(|e| format!("The list of signed sync engines failed its signature check, so it was ignored. {e}"))?;
    let manifest: EngineManifest = serde_json::from_slice(&body).map_err(|e| fail("list", e.to_string()))?;
    let safe = |f: &EngineFile| !f.name.is_empty() && !f.name.contains(['/', '\\']) && !f.name.contains("..");
    if !manifest.files.values().all(safe) || !manifest.license.iter().all(safe) {
        return Err("The list of signed sync engines has a file name SyncHopper won't use.".into());
    }
    Ok(manifest)
}

/// Downloads one file listed in engine.json and checks its size and
/// checksum. Nothing is returned unless both match.
pub async fn channel_file(channel: &EngineChannel, file: &EngineFile, progress: impl Fn(u8)) -> Result<Vec<u8>, String> {
    let client = http_client()?;
    let fail = |e: String| format!("Could not download the signed sync engine: {e}");
    let mut resp = client
        .get(channel_url(channel, &file.name))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| fail(e.to_string()))?;
    let total = file.size.max(1);
    let mut bytes: Vec<u8> = Vec::with_capacity(file.size as usize);
    let mut hasher = Sha256::new();
    let mut last = 255u8;
    while let Some(chunk) = resp.chunk().await.map_err(|e| fail(e.to_string()))? {
        hasher.update(&chunk);
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > file.size {
            return Err("The downloaded sync engine was bigger than expected, so it was thrown away.".into());
        }
        let pct = ((bytes.len() as u64 * 100) / total).min(100) as u8;
        if pct != last {
            last = pct;
            progress(pct);
        }
    }
    let actual = hex(&hasher.finalize());
    if bytes.len() as u64 != file.size || actual != file.sha256.to_lowercase() {
        return Err("The downloaded sync engine did not match the signed list, so it was thrown away.".into());
    }
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a file on disk, or None if it can't be read.
pub fn file_sha256(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex(&hasher.finalize()))
}

fn source_path(data: &Path) -> PathBuf {
    data.join("bin").join("engine-source.json")
}

/// Where the installed engine came from, if SyncHopper recorded it and the
/// program is still the same file.
pub fn installed_source(data: &Path) -> Option<EngineSource> {
    let text = std::fs::read_to_string(source_path(data)).ok()?;
    let src: EngineSource = serde_json::from_str(&text).ok()?;
    (file_sha256(&managed_path(data))? == src.sha256).then_some(src)
}

/// Renames, trying a few times: antivirus often holds a freshly written
/// program open for a moment while it scans it.
fn rename_patiently(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(e) if tries < 5 && e.kind() != std::io::ErrorKind::NotFound => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(400));
            }
            r => return r,
        }
    }
}

/// Puts a new engine program in place and records where it came from. Works
/// while the old one is running: on Windows a running program can be
/// renamed but not overwritten or deleted, so it is moved aside first and
/// removed at the next start.
pub fn install_engine(data: &Path, bytes: &[u8], source: &str, version: &str) -> Result<PathBuf, String> {
    let dest = managed_path(data);
    let dir = dest.parent().ok_or("No folder for the sync engine.")?.to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    let tmp = dest.with_extension("new");
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    // A unique name, because an earlier replaced copy may still be running
    // (the Koofr drive, for one) and can't be removed yet.
    let old = dir.join(format!("rclone.old-{stamp}"));
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, bytes).map_err(|e| format!("Could not save the sync engine: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    let had_old = dest.exists();
    if had_old {
        if let Err(e) = rename_patiently(&dest, &old) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("Could not move the old sync engine aside: {e}"));
        }
    }
    if let Err(e) = rename_patiently(&tmp, &dest) {
        if had_old {
            let _ = std::fs::rename(&old, &dest);
        }
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Could not put the sync engine in place: {e}"));
    }
    if had_old {
        let _ = std::fs::remove_file(&old);
    }
    let src = EngineSource { source: source.into(), version: version.into(), sha256: hex(&Sha256::digest(bytes)) };
    let _ = std::fs::write(source_path(data), serde_json::to_string_pretty(&src).unwrap_or_default());
    Ok(dest)
}

/// Saves rclone's license next to it.
pub fn install_license(data: &Path, bytes: &[u8]) -> Result<(), String> {
    let path = data.join("bin").join("rclone-COPYING.txt");
    std::fs::write(&path, bytes).map_err(|e| format!("Could not save rclone's license: {e}"))
}

/// Removes replaced engines left behind because they were still running,
/// and any half-finished download.
pub fn tidy_engine_folder(data: &Path) {
    let dir = data.join("bin");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("rclone.old") || name == "rclone.new" {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[derive(Clone)]
pub struct Rclone {
    pub exe: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
    /// Added to every command, for example the request limit.
    pub extra: Vec<String>,
    /// What the saved sign-ins are locked with, when they are.
    pub pass: Option<String>,
}

impl std::fmt::Debug for Rclone {
    /// Leaves the key out, so it can never end up in a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rclone").field("exe", &self.exe).field("config", &self.config).field("locked", &self.pass.is_some()).finish()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct About {
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub free: Option<u64>,
}

/// One line of rclone's JSON log.
#[derive(Clone, Debug, Default)]
pub struct LogLine {
    pub level: String,
    pub msg: String,
    pub object: String,
    pub object_type: String,
    pub stats: Option<Stats>,
}

impl LogLine {
    /// An error, including text that wasn't rclone's JSON and the levels
    /// above "error" ("critical" and up) that rclone uses when it can't
    /// even start, for example when Koofr can't be reached at all.
    pub fn is_error(&self) -> bool {
        matches!(self.level.as_str(), "error" | "raw" | "critical" | "alert" | "emergency")
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub bytes: u64,
    pub total_bytes: u64,
    pub speed: f64,
    pub eta: Option<u64>,
    pub transfers: u64,
    pub total_transfers: u64,
    pub deletes: u64,
    pub errors: u64,
    pub current: Vec<String>,
}

#[derive(Debug, Default)]
pub struct RunResult {
    pub exit: i32,
    pub cancelled: bool,
    pub errors: Vec<LogLine>,
    pub notices: Vec<LogLine>,
    pub stats: Option<Stats>,
    /// Set when the program could not be started at all.
    pub spawn_error: Option<String>,
}

pub fn parse_line(line: &str) -> Option<LogLine> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let stats = v.get("stats").map(|s| {
        let num = |k: &str| s.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        Stats {
            bytes: num("bytes") as u64,
            total_bytes: num("totalBytes") as u64,
            speed: num("speed"),
            eta: s.get("eta").and_then(|x| x.as_f64()).map(|x| x as u64),
            transfers: num("transfers") as u64,
            total_transfers: num("totalTransfers") as u64,
            deletes: num("deletes") as u64,
            errors: num("errors") as u64,
            current: s
                .get("transferring")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(|n| n.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
        }
    });
    Some(LogLine {
        level: text("level"),
        msg: strip_ansi(&text("msg")),
        object: text("object"),
        object_type: text("objectType"),
        stats,
    })
}

impl Rclone {
    /// Starts the engine. If the program is missing just then (it is being
    /// swapped for a newer copy, which takes a moment), tries once more.
    pub async fn spawn(&self, args: &[String], setup: impl Fn(&mut Command)) -> std::io::Result<tokio::process::Child> {
        let mut cmd = self.command(args);
        setup(&mut cmd);
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let mut cmd = self.command(args);
                setup(&mut cmd);
                cmd.spawn()
            }
            r => r,
        }
    }

    /// A command with the app's own config and cache folders, and no console
    /// window on Windows.
    pub fn command(&self, args: &[String]) -> Command {
        let mut c = Command::new(&self.exe);
        c.args(args);
        c.arg("--config").arg(&self.config);
        c.arg("--cache-dir").arg(&self.cache);
        c.args(&self.extra);
        // The saved sign-ins may be locked. The key goes in through the
        // environment, never the command line, and the engine must not stop
        // to ask for it when it is missing.
        c.env("RCLONE_ASK_PASSWORD", "false");
        match &self.pass {
            Some(pass) => c.env("RCLONE_CONFIG_PASS", pass),
            None => c.env_remove("RCLONE_CONFIG_PASS"),
        };
        c.stdin(Stdio::null());
        c.kill_on_drop(true);
        platform::hide_window(&mut c);
        c
    }

    /// Runs a short command and returns what it printed.
    pub async fn capture(&self, args: &[&str], timeout: Duration) -> Result<String, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let child = self
            .spawn(&args, |c| {
                c.stdout(Stdio::piped()).stderr(Stdio::piped());
            })
            .await
            .map_err(|e| spawn_failure(&self.exe, &e))?;
        let out = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| "The service took too long to answer.".to_string())?
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            let err = strip_ansi(&String::from_utf8_lossy(&out.stderr));
            let last = err
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.contains("Config file") && !l.contains("using defaults"))
                .collect::<Vec<_>>()
                .join(" ");
            Err(if last.is_empty() { format!("The sync engine stopped with code {:?}.", out.status.code()) } else { last })
        }
    }

    /// Scrambles a password the way rclone expects in its config file. The
    /// password goes in through stdin so it never shows in a process list.
    pub async fn obscure(&self, password: &str) -> Result<String, String> {
        let mut child = self
            .spawn(&["obscure".to_string(), "-".to_string()], |c| {
                c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            })
            .await
            .map_err(|e| spawn_failure(&self.exe, &e))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(password.as_bytes()).await.map_err(|e| e.to_string())?;
            stdin.shutdown().await.ok();
        }
        let out = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
            .await
            .map_err(|_| "The sync engine did not answer.".to_string())?
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !text.is_empty() {
            Ok(text)
        } else {
            Err("The sync engine could not store the password.".to_string())
        }
    }

    pub async fn about(&self, target: &str) -> Result<About, String> {
        let out = self.capture(&["about", target, "--json"], Duration::from_secs(60)).await?;
        let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
        let n = |k: &str| v.get(k).and_then(|x| x.as_f64()).map(|x| x as u64);
        Ok(About { total: n("total"), used: n("used"), free: n("free") })
    }

    /// Names of the folders directly inside `target`.
    pub async fn list_dirs(&self, target: &str) -> Result<Vec<String>, String> {
        let out = self
            .capture(&["lsjson", target, "--dirs-only", "--no-modtime", "--no-mimetype"], Duration::from_secs(120))
            .await?;
        let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
        let mut names: Vec<String> = v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.get("Name").and_then(|n| n.as_str()).map(|n| n.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names)
    }

    /// Number of files and total bytes under `target`.
    pub async fn size(&self, target: &str) -> Result<(u64, u64), String> {
        let out = self.capture(&["size", target, "--json"], Duration::from_secs(900)).await?;
        let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| e.to_string())?;
        let n = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) as u64;
        Ok((n("count"), n("bytes")))
    }

    pub async fn mkdir(&self, target: &str) -> Result<(), String> {
        self.capture(&["mkdir", target], Duration::from_secs(120)).await.map(|_| ())
    }

    /// Has the engine sign an account in on the service's own page. The
    /// engine opens the browser, waits on this PC for the answer, and
    /// writes the result into its settings file as account `name`.
    /// `on_link` is told the page's address, for when the browser doesn't
    /// open by itself.
    pub async fn browser_sign_in(&self, name: &str, kind: &str, cancel: &Cancel, mut on_link: impl FnMut(&str)) -> Result<(), String> {
        let args = ["config".to_string(), "create".to_string(), name.to_string(), kind.to_string()];
        let mut child = self
            .spawn(&args, |c| {
                c.stdout(Stdio::null()).stderr(Stdio::piped());
            })
            .await
            .map_err(|e| spawn_failure(&self.exe, &e))?;
        let stderr = child.stderr.take().expect("stderr was piped");
        let mut lines = BufReader::new(stderr).lines();
        let mut last = String::new();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(text)) => {
                        let text = strip_ansi(text.trim());
                        if let Some(i) = text.find("link: ") {
                            on_link(text[i + 6..].trim());
                        } else if !text.is_empty() && !text.contains("Config file") {
                            last = text;
                        }
                    }
                    _ => break,
                },
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    return Err(SIGN_IN_CANCELLED.to_string());
                }
            }
        }
        match child.wait().await {
            Ok(status) if status.success() => Ok(()),
            _ if last.is_empty() => Err("The sync engine stopped before the sign-in finished.".to_string()),
            _ => Err(last),
        }
    }

    // ----- saved sign-ins -----------------------------------------------
    //
    // The engine keeps every account's sign-in in its own settings file,
    // one `[name]` section per account. SyncHopper has the engine lock that
    // file with a key (see `app::Hub::secure_signins`), so the file is
    // always written through the two functions below and never by hand.

    /// The saved sign-ins as plain text. Empty when there are none yet.
    pub async fn signins(&self) -> Result<String, String> {
        if !self.config.is_file() {
            return Ok(String::new());
        }
        if config_locked(&self.config) {
            self.capture(&["config", "show"], Duration::from_secs(30)).await
        } else {
            std::fs::read_to_string(&self.config).map_err(|e| format!("Could not read the saved sign-ins: {e}"))
        }
    }

    /// Replaces the saved sign-ins with `text`, locked with this engine's
    /// key when it has one. The new file is checked before it takes the old
    /// one's place, so a failure leaves the old file as it was.
    pub async fn save_signins(&self, text: &str) -> Result<(), String> {
        if section_names(text).is_empty() {
            return match std::fs::remove_file(&self.config) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Could not remove the saved sign-ins: {e}")),
                _ => Ok(()),
            };
        }
        let pending = self.config.with_extension("conf.new");
        std::fs::write(&pending, text).map_err(|e| format!("Could not save the sign-in: {e}"))?;
        if let Err(e) = self.lock_and_check(&pending, text).await {
            let _ = std::fs::remove_file(&pending);
            return Err(e);
        }
        let _ = std::fs::remove_file(&self.config);
        std::fs::rename(&pending, &self.config).map_err(|e| format!("Could not save the sign-in: {e}"))
    }

    /// Has the engine lock the file at `pending` with this engine's key,
    /// then makes sure it opens with that key and holds the same accounts.
    async fn lock_and_check(&self, pending: &Path, text: &str) -> Result<(), String> {
        let Some(pass) = &self.pass else { return Ok(()) };
        let failed = || "The sync engine could not lock the saved sign-in.".to_string();
        let plain = Rclone { config: pending.to_path_buf(), pass: None, extra: Vec::new(), ..self.clone() };
        let args = ["config".to_string(), "encryption".to_string(), "set".to_string()];
        let mut child = plain
            .spawn(&args, |c| {
                c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            })
            .await
            .map_err(|e| spawn_failure(&self.exe, &e))?;
        if let Some(mut stdin) = child.stdin.take() {
            // The engine asks for the key twice. It goes in through stdin so
            // it never shows in a process list.
            stdin.write_all(format!("{pass}\n{pass}\n").as_bytes()).await.map_err(|e| e.to_string())?;
            stdin.shutdown().await.ok();
        }
        let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .map_err(|_| "The sync engine did not answer.".to_string())?
            .map_err(|e| e.to_string())?;
        if !out.status.success() || !config_locked(pending) {
            return Err(failed());
        }
        let locked = Rclone { config: pending.to_path_buf(), extra: Vec::new(), ..self.clone() };
        let back = locked.capture(&["config", "show"], Duration::from_secs(30)).await.map_err(|_| failed())?;
        if section_names(&back) == section_names(text) {
            Ok(())
        } else {
            Err(failed())
        }
    }

    /// Runs a long job, feeding every log line to `on_line`. Stops the job
    /// when `cancel` fires.
    pub async fn run_logged(
        &self,
        args: &[String],
        cancel: &Cancel,
        mut on_line: impl FnMut(&LogLine),
    ) -> RunResult {
        let mut res = RunResult::default();
        let spawned = self
            .spawn(args, |c| {
                c.stdout(Stdio::null()).stderr(Stdio::piped());
            })
            .await;
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                res.exit = -1;
                res.spawn_error = Some(spawn_failure(&self.exe, &e));
                return res;
            }
        };
        let stderr = child.stderr.take().expect("stderr was piped");
        let mut lines = BufReader::new(stderr).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    match line {
                        Ok(Some(text)) => {
                            let parsed = parse_line(&text).unwrap_or_else(|| LogLine {
                                level: "raw".into(),
                                msg: strip_ansi(text.trim()),
                                ..Default::default()
                            });
                            if let Some(s) = &parsed.stats {
                                res.stats = Some(s.clone());
                            } else if parsed.is_error() {
                                if res.errors.len() < 400 { res.errors.push(parsed.clone()); }
                            } else if parsed.level == "notice" || parsed.level == "warning" {
                                if res.notices.len() < 400 { res.notices.push(parsed.clone()); }
                            }
                            on_line(&parsed);
                        }
                        _ => break,
                    }
                }
                _ = cancel.cancelled() => {
                    res.cancelled = true;
                    let _ = child.kill().await;
                    break;
                }
            }
        }
        res.exit = match child.wait().await {
            Ok(status) => status.code().unwrap_or(-1),
            Err(_) => -1,
        };
        res
    }
}

/// What `Rclone::browser_sign_in` returns when the wait was cancelled.
pub const SIGN_IN_CANCELLED: &str = "cancelled";

/// The line that marks a settings file the engine has locked.
const LOCKED_MARK: &str = "RCLONE_ENCRYPT_V0:";

/// True when the engine's settings file at `path` is locked with a key.
pub fn config_locked(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 256];
    let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    String::from_utf8_lossy(&head[..n]).lines().any(|l| l.trim() == LOCKED_MARK)
}

/// Sign-in text split into its sections: each account's name and the lines
/// under it.
fn sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') && t.len() > 2 {
            out.push((t[1..t.len() - 1].to_string(), Vec::new()));
        } else if !t.is_empty() {
            if let Some((_, lines)) = out.last_mut() {
                lines.push(t.to_string());
            }
        }
    }
    out
}

/// The names of the accounts in sign-in text, in order.
pub fn section_names(text: &str) -> Vec<String> {
    sections(text).into_iter().map(|(name, _)| name).collect()
}

/// One account's lines in sign-in text, without its `[name]` line.
pub fn section(text: &str, name: &str) -> Option<String> {
    sections(text).into_iter().find(|(n, _)| n == name).map(|(_, lines)| lines.join("\n"))
}

/// Sign-in text with one account's lines replaced or added, or with the
/// account taken out when `body` is None.
pub fn with_section(text: &str, name: &str, body: Option<&str>) -> String {
    let mut all = sections(text);
    all.retain(|(n, _)| n != name);
    if let Some(body) = body {
        all.push((name.to_string(), body.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()));
    }
    let mut out = String::new();
    for (n, lines) in all {
        out.push_str(&format!("[{n}]\n"));
        for l in lines {
            out.push_str(&l);
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Reads "rclone v1.76.0" style text into ("1.76.0", (1, 76)).
pub fn parse_version(text: &str) -> Option<(String, (u32, u32))> {
    let first = text.lines().next()?.trim();
    let v = first.split_whitespace().find(|w| w.starts_with('v'))?.trim_start_matches('v');
    let clean: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut parts = clean.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((clean, (major, minor)))
}

/// "1.76.2" as (1, 76, 2), for comparing versions.
pub fn version_triple(v: &str) -> (u32, u32, u32) {
    let mut parts = v.trim_start_matches('v').split('.').map(|p| {
        p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>().unwrap_or(0)
    });
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

pub fn managed_path(data: &Path) -> PathBuf {
    data.join("bin").join(if cfg!(windows) { "rclone.exe" } else { "rclone" })
}

/// What happened when the engine was asked for its version.
enum Probe {
    Works(String, (u32, u32)),
    Blocked(String),
    Broken,
}

async fn probe(exe: &Path) -> Probe {
    let mut cmd = Command::new(exe);
    cmd.arg("version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    platform::hide_window(&mut cmd);
    let out = match tokio::time::timeout(Duration::from_secs(20), cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) if blocked_by_system(exe, &e) => return Probe::Blocked(spawn_failure(exe, &e)),
        _ => return Probe::Broken,
    };
    if !out.status.success() {
        return Probe::Broken;
    }
    match parse_version(&String::from_utf8_lossy(&out.stdout)) {
        Some((v, n)) => Probe::Works(v, n),
        None => Probe::Broken,
    }
}

pub struct Located {
    pub exe: PathBuf,
    pub version: String,
}

/// The state of the app's own copy of rclone.
pub enum Found {
    Ready(Located),
    /// It is there but the system won't run it.
    Blocked(String),
    /// Missing, too old or not working.
    Missing,
}

/// Looks at the app's own copy of rclone.
pub async fn find(data: &Path) -> Found {
    let own = managed_path(data);
    if !own.is_file() {
        return Found::Missing;
    }
    match probe(&own).await {
        Probe::Works(version, num) if num >= MIN_VERSION => Found::Ready(Located { exe: own, version }),
        Probe::Blocked(msg) => Found::Blocked(msg),
        _ => Found::Missing,
    }
}

/// Finds the app's own copy of rclone. A copy installed some other way is
/// never used, so every install runs the version SyncHopper downloaded and
/// keeps up to date.
pub async fn locate(data: &Path) -> Option<Located> {
    match find(data).await {
        Found::Ready(loc) => Some(loc),
        _ => None,
    }
}

fn platform_tag() -> (&'static str, &'static str) {
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "386"
    } else {
        "amd64"
    };
    (os, arch)
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("SyncHopper")
        .connect_timeout(Duration::from_secs(20))
        // A download that stops receiving anything for this long is given
        // up and tried again later, instead of hanging forever.
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())
}

/// The newest released rclone version, like "1.76.0".
pub async fn latest_version(base: &str) -> Result<String, String> {
    let client = http_client()?;
    let text = tokio::time::timeout(Duration::from_secs(30), async {
        client
            .get(format!("{base}/version.txt"))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .text()
            .await
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "The download site did not answer.".to_string())??;
    parse_version(&text).map(|(v, _)| v).ok_or_else(|| "Could not read the newest sync engine version.".to_string())
}

/// Downloads the official rclone build for this computer, checks it against
/// the published SHA-256 list and puts it in the app's own folder.
pub async fn download(data: &Path, base: &str, progress: impl Fn(u8)) -> Result<PathBuf, String> {
    let version = latest_version(base).await?;
    let (os, arch) = platform_tag();
    let name = format!("rclone-v{version}-{os}-{arch}.zip");
    let client = http_client()?;
    let fail = |what: &str, e: String| format!("Could not download the sync engine ({what}): {e}");

    let sums = client
        .get(format!("{base}/v{version}/SHA256SUMS"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| fail("checksum list", e.to_string()))?
        .text()
        .await
        .map_err(|e| fail("checksum list", e.to_string()))?;
    let expected = sums
        .lines()
        .find_map(|l| {
            let mut parts = l.split_whitespace();
            let hash = parts.next()?;
            let file = parts.next()?.trim_start_matches('*');
            (file == name).then(|| hash.to_lowercase())
        })
        .ok_or_else(|| format!("The download site has no checksum for {name}."))?;

    let mut resp = client
        .get(format!("{base}/v{version}/{name}"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| fail("program", e.to_string()))?;
    let total = resp.content_length().unwrap_or(0);
    let mut bytes: Vec<u8> = Vec::with_capacity(total as usize);
    let mut hasher = Sha256::new();
    let mut last = 255u8;
    while let Some(chunk) = resp.chunk().await.map_err(|e| fail("program", e.to_string()))? {
        hasher.update(&chunk);
        bytes.extend_from_slice(&chunk);
        if total > 0 {
            let pct = ((bytes.len() as u64 * 100) / total).min(100) as u8;
            if pct != last {
                last = pct;
                progress(pct);
            }
        }
    }
    let actual = hex(&hasher.finalize());
    if actual != expected {
        return Err("The downloaded sync engine did not match its published checksum, so it was thrown away. Try again.".to_string());
    }

    let program = tokio::task::spawn_blocking(move || extract(&bytes)).await.map_err(|e| e.to_string())??;
    let data = data.to_path_buf();
    tokio::task::spawn_blocking(move || install_engine(&data, &program, "rclone.org", &version))
        .await
        .map_err(|e| e.to_string())?
}

/// The rclone program inside a release zip.
fn extract(zip_bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let wanted = if cfg!(windows) { "rclone.exe" } else { "rclone" };
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).map_err(|e| e.to_string())?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().replace('\\', "/");
        if name.rsplit('/').next() != Some(wanted) || entry.is_dir() {
            continue;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(|e| e.to_string())?;
        return Ok(data);
    }
    Err("The download did not contain the sync engine program.".to_string())
}

/// Turns raw rclone error text into something a person can act on.
/// Wordings that mean the connection to Koofr failed or dropped, rather
/// than anything being wrong with a file. Windows words several of these
/// differently from Linux and macOS, for example after the PC wakes from
/// sleep and a connection opened before it slept turns out to be dead.
pub const NETWORK_WORDS: &[&str] = &[
    "no such host",
    "dial tcp",
    "connection refused",
    "i/o timeout",
    "network is unreachable",
    "tls handshake",
    "connection reset",
    "unexpected eof",
    "context deadline exceeded",
    "broken pipe",
    "took too long",
    "could not reach koofr",
    "temporary failure in name resolution",
    "use of closed network connection",
    "server closed idle connection",
    "client connection lost",
    "client connection force closed",
    // Windows socket errors, as Go reports them.
    "wsasend:",
    "wsarecv:",
    "connectex:",
    "getaddrinfow",
    "forcibly closed by the remote host",
    "aborted by the software in your host machine",
    "no connection could be made",
    "did not properly respond after a period of time",
    "unreachable network",
    "unreachable host",
    "network name is no longer available",
    "semaphore timeout period has expired",
];

/// What the engine's name for every account after the first starts with.
/// `error_words` relies on it to tell an account's path from other text.
pub const REMOTE_PREFIX: &str = "shacct";

/// True when the text describes a dropped or failed connection.
pub fn is_network_error(raw: &str) -> bool {
    let l = error_words(raw);
    NETWORK_WORDS.iter().any(|w| l.contains(w))
}

/// An error in lower case with its file paths, folder paths and web
/// addresses taken out, so that what an error is gets decided by its words
/// and never by a name in it: a file called "Sales quota 401.xlsx" that is
/// open in another program is not "storage full" or "wrong password".
///
/// A path starts at a drive (`C:\`), a network or long path (`\\`), a
/// slash after a space or quote, or an account's name in the engine
/// (`koofr:`, `shacct1a2b:`), and runs up to a quote, the
/// end of a line, or a colon that ends it (Windows names can't hold a
/// colon). A web address runs up to a quote or a space.
pub fn error_words(raw: &str) -> String {
    /// The length of an account's engine name with its colon, when the text
    /// starts with one: "koofr:" for the first account, "shacct" and some
    /// letters and digits for every later one.
    fn remote_prefix(c: &[char]) -> Option<usize> {
        let starts = |s: &str| s.chars().enumerate().all(|(k, ch)| c.get(k).map(|x| x.to_ascii_lowercase()) == Some(ch));
        if starts("koofr:") {
            return Some(6);
        }
        if !starts(REMOTE_PREFIX) {
            return None;
        }
        let id = c[REMOTE_PREFIX.len()..].iter().take_while(|ch| ch.is_ascii_alphanumeric()).count();
        (id > 0 && c.get(REMOTE_PREFIX.len() + id) == Some(&':')).then_some(REMOTE_PREFIX.len() + id + 1)
    }

    let c: Vec<char> = raw.chars().collect();
    let at = |i: usize, s: &str| s.chars().enumerate().all(|(k, ch)| c.get(i + k).map(|x| x.to_ascii_lowercase()) == Some(ch));
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < c.len() {
        let starts = i == 0 || matches!(c[i - 1], ' ' | '"' | '(' | '[' | '=' | '`' | '\t' | '\n');
        let url = starts && (at(i, "http://") || at(i, "https://"));
        let path = starts
            && !url
            && ((c[i] == '\\' && c.get(i + 1) == Some(&'\\'))
                || (c[i].is_ascii_alphabetic() && c.get(i + 1) == Some(&':') && matches!(c.get(i + 2), Some('\\') | Some('/')))
                || (c[i] == '/' && c.get(i + 1).is_some_and(|n| !n.is_whitespace() && *n != '/'))
                || remote_prefix(&c[i..]).is_some_and(|n| c.get(i + n).is_some_and(|x| !x.is_whitespace())));
        if !url && !path {
            out.extend(c[i].to_lowercase());
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < c.len() {
            let ch = c[j];
            if ch == '"' || ch == '\n' || (url && ch.is_whitespace()) {
                break;
            }
            if path && ch == ':' && c.get(j + 1).is_none_or(|n| n.is_whitespace()) {
                break;
            }
            j += 1;
        }
        out.push(' ');
        i = j;
    }
    out
}

/// True when the text (from `error_words`) reports this HTTP status, the
/// way Koofr's errors do ("Invalid response status! Got 401, expected 200")
/// or as "status code 401" or "HTTP error 401". Any other number, such as
/// a header value or part of a name, is not a status.
pub fn reports_status(words: &str, code: &str) -> bool {
    ["status! got ", "status code ", "http error "].iter().any(|marker| {
        words.match_indices(marker).any(|(i, _)| {
            let digits: String = words[i + marker.len()..].chars().take_while(|ch| ch.is_ascii_digit()).collect();
            digits == code
        })
    })
}

/// A plain-language version of an error from the sync engine. Text it does
/// not recognise is passed through, shortened if very long.
pub fn friendly_error(p: &Provider, raw: &str) -> String {
    friendly_known(p, raw).unwrap_or_else(|| {
        let t = raw.trim();
        if t.chars().count() > 220 {
            format!("{}...", t.chars().take(220).collect::<String>())
        } else {
            t.to_string()
        }
    })
}

/// A plain-language version of an error, or None when it is not one SyncHopper
/// recognises.
pub fn friendly_known(p: &Provider, raw: &str) -> Option<String> {
    let l = error_words(raw);
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    let code = |c: &str| reports_status(&l, c);
    let cloud = p.name;
    let text = if is_blocked_message(raw) {
        BLOCKED_TEXT.to_string()
    } else if is_locked_signins(raw) {
        LOCKED_SIGNINS_TEXT.to_string()
    } else if code("429") || has(&["too many requests"]) {
        format!("{cloud} says SyncHopper is sending too many requests. {}", throttle_hint(p))
    } else if is_auth_error(&l) {
        format!("{cloud} did not accept the saved sign-in. Open this account's settings and choose Reconnect.")
    } else if has(&["being used by another process", "sharing violation"]) {
        "A file is open in another program. It will sync once that program closes it.".to_string()
    } else if has(&["source file is being updated", "size changed"]) {
        "A file was still being written while it synced. It will be tried again.".to_string()
    } else if has(&["too long"]) {
        "A file name is too long for Windows.".to_string()
    } else if has(&["not enough space", "no space left", "disk full"]) {
        "This PC is out of disk space.".to_string()
    } else if code("507") || has(&["quota", "insufficient storage", "storage is full"]) {
        format!("Your {cloud} storage is full.")
    } else if has(&["access is denied", "permission denied"]) {
        "Windows would not let the app read or write a file.".to_string()
    } else if is_network_error(raw) || has(&["bad gateway", "service unavailable", "gateway timeout"]) || code("502") || code("503") || code("504") {
        format!("Could not reach {cloud}. It will be tried again automatically.")
    } else if has(&["directory not found"]) {
        "A folder was not found.".to_string()
    } else {
        return None;
    };
    Some(text)
}

/// What to do when the service says SyncHopper is asking too often.
pub fn throttle_hint(p: &Provider) -> String {
    match p.request_limit {
        Some(_) => format!("If this keeps happening, turn on Limit requests to {} in this account's settings.", p.name),
        None => "SyncHopper waits and tries again by itself.".to_string(),
    }
}

/// True when the words (from `error_words`) say the service turned down the
/// saved sign-in.
pub fn is_auth_error(words: &str) -> bool {
    reports_status(words, "401")
        || [
            "unauthorized",
            "invalid credentials",
            "authentication failed",
            "couldn't login",
            // pCloud
            "log in required",
            "log in failed",
            "invalid 'access_token'",
            "invalid access token",
            "invalid_grant",
            // The saved sign-ins don't have this account any more.
            "didn't find section in config file",
            // Filen
            "invalid api key",
            "api key not found",
            "wrong email or password",
            "invalid email or password",
        ]
        .iter()
        .any(|w| words.contains(w))
}

/// What a folder or account shows when the saved sign-ins can't be read on
/// this PC, for example after the app's files were copied from another PC.
pub const LOCKED_SIGNINS_TEXT: &str = "SyncHopper can't read the saved sign-in on this PC. Open this account's settings and choose Reconnect.";

/// True when the engine says it could not unlock the saved sign-ins.
pub fn is_locked_signins(raw: &str) -> bool {
    let l = raw.to_lowercase();
    l.contains("unable to decrypt configuration") || l.contains("couldn't decrypt configuration")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::KOOFR;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/engine");

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{FIX}/{name}")).unwrap()
    }

    #[test]
    fn paths_and_addresses_are_left_out_of_error_words() {
        let w = error_words("Failed to copy: failed to open source object: open C:\\Users\\r\\Sales quota\\Invoice 401.pdf: The process cannot access the file because it is being used by another process.");
        assert!(!w.contains("quota") && !w.contains("401"), "{w}");
        assert!(w.contains("failed to open source object: open"), "{w}");
        assert!(w.contains("the process cannot access the file because it is being used by another process."), "{w}");
        // rclone's long Windows paths and network shares.
        let w = error_words("open \\\\?\\C:\\Unauthorized\\a.jpg: Access is denied.");
        assert_eq!(w.trim(), "open  : access is denied.".trim(), "{w}");
        let w = error_words("open \\\\nas\\share\\disk full\\b.txt: The device is not ready.");
        assert!(!w.contains("disk full") && w.contains("the device is not ready."), "{w}");
        // Quoted paths, several in one message.
        let w = error_words("rename \"C:\\a\\quota.partial\" \"C:\\a\\quota.txt\": Access is denied.");
        assert!(!w.contains("quota") && w.contains("access is denied."), "{w}");
        assert!(!error_words("too many deletes (>50%, 2 of 3) on Path2 \"koofr:Quota/x\". Run with --force").contains("quota"));
        // Web addresses carry names too.
        let w = error_words("Post \"https://app.koofr.net/content/api/v2/mounts/x/files/put?path=%2FQuota&filename=a%20401.txt\": dial tcp: lookup app.koofr.net: no such host");
        assert!(!w.contains("quota") && !w.contains("401") && w.contains("dial tcp: lookup app.koofr.net: no such host"), "{w}");
        let w = error_words("Get https://app.koofr.net/api/v2/mounts: tls: failed to verify certificate: x509: certificate signed by unknown authority");
        assert!(w.contains("tls: failed to verify certificate: x509:"), "{w}");
        // Slashes that aren't paths stay.
        for keep in ["i/o timeout", "an i/o device error", "use -l/--copy-links", "map[content-type:[application/json]]", "expected [200 206]"] {
            assert!(error_words(keep).contains(keep), "{keep}");
        }
    }

    #[test]
    fn only_reported_statuses_count() {
        let koofr = |code: u32, len: u32| error_words(&format!("Invalid response status! Got {code}, expected 200; headers: map[Content-Length:[{len}] Date:[Sat, 14 Nov 2026 10:50:30 GMT]], content: "));
        assert!(reports_status(&koofr(400, 401), "400"));
        assert!(!reports_status(&koofr(400, 401), "401"), "a header value is not a status");
        assert!(!reports_status(&koofr(400, 503), "503"));
        assert!(!reports_status(&koofr(4001, 0), "400"), "only the whole number counts");
        assert!(reports_status(&error_words("unexpected status code 401"), "401"));
        assert!(reports_status(&error_words("HTTP error 503 returned"), "503"));
        assert!(!reports_status(&error_words("Failed to copy IMG_401.jpg: odd"), "401"));
    }

    #[test]
    fn signatures_from_tauri_signer_are_checked() {
        let key = String::from_utf8(fixture("test-key.pub")).unwrap();
        let data = fixture("list-1.76.0.json");
        let sig = String::from_utf8(fixture("list-1.76.0.json.sig")).unwrap();
        verify_signature(&data, &sig, &key).expect("a good signature passes");
        // Surrounding whitespace, as when read from a file, is fine.
        verify_signature(&data, &format!("{sig}\n"), &format!(" {key}\n")).unwrap();

        let mut tampered = data.clone();
        let i = tampered.iter().position(|b| *b == b'6').unwrap();
        tampered[i] = b'7';
        assert!(verify_signature(&tampered, &sig, &key).unwrap_err().contains("does not match"));

        // Another list's signature doesn't fit this list.
        let other = String::from_utf8(fixture("list-1.77.0.json.sig")).unwrap();
        assert!(verify_signature(&data, &other, &key).is_err());

        assert!(verify_signature(&data, "not base64 !!", &key).is_err());
        assert!(verify_signature(&data, &sig, "bm90IGEga2V5").is_err());
        assert!(verify_signature(&data, "", &key).is_err());
    }

    #[test]
    fn blocked_is_told_apart_from_missing() {
        // Windows: Smart App Control and other application control
        // policies, group policy, antivirus, access refused to a file that
        // is there.
        for code in [4551, 4550, 4580, 4599, 1260, 225, 226] {
            assert!(blocked_code(Some(code), "x", true, true), "{code}");
        }
        assert!(blocked_code(Some(5), "Access is denied.", true, true));
        assert!(!blocked_code(Some(5), "Access is denied.", true, false));
        // Missing, or some other trouble: not blocked.
        assert!(!blocked_code(Some(2), "The system cannot find the file specified.", true, false));
        assert!(!blocked_code(Some(3), "x", true, false));
        assert!(!blocked_code(Some(4549), "x", true, true));
        assert!(!blocked_code(Some(4600), "x", true, true));
        assert!(!blocked_code(None, "x", true, true));
        // The wording alone is enough.
        assert!(blocked_code(None, "An Application Control policy has blocked this file.", true, true));
        assert!(blocked_code(None, "This program is blocked by group policy.", true, true));
        // Other systems: permission refused to run a file that is there.
        assert!(blocked_code(Some(13), "Permission denied", false, true));
        assert!(!blocked_code(Some(13), "Permission denied", false, false));
        assert!(!blocked_code(Some(2), "No such file or directory", false, false));
        // Windows codes mean nothing elsewhere.
        assert!(!blocked_code(Some(4551), "x", false, true));
    }

    #[test]
    fn blocked_messages_read_plainly_everywhere() {
        let raw = spawn_failure(Path::new("/nowhere/rclone"), &std::io::Error::from_raw_os_error(2));
        assert!(raw.starts_with("Could not start the sync engine"), "{raw}");
        assert!(!is_blocked_message(&raw));
        let blocked = format!("{BLOCKED_PREFIX} (C:\\x\\rclone.exe): An Application Control policy has blocked this file. (os error 4551)");
        assert!(is_blocked_message(&blocked));
        assert!(is_blocked_message(BLOCKED_TEXT));
        assert_eq!(friendly_error(&KOOFR, &blocked), BLOCKED_TEXT);
        assert_eq!(friendly_known(&KOOFR, &blocked).as_deref(), Some(BLOCKED_TEXT));
        assert!(!BLOCKED_TEXT.contains('\u{2014}'));
    }

    #[test]
    fn engine_swap_keeps_a_record_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        assert!(installed_source(data).is_none());
        let exe = install_engine(data, b"first", "signed", "1.0.0").unwrap();
        assert_eq!(exe, managed_path(data));
        assert_eq!(std::fs::read(&exe).unwrap(), b"first");
        let src = installed_source(data).unwrap();
        assert_eq!((src.source.as_str(), src.version.as_str()), ("signed", "1.0.0"));
        assert_eq!(src.sha256, file_sha256(&exe).unwrap());

        install_engine(data, b"second", "signed", "1.1.0").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"second");
        assert_eq!(installed_source(data).unwrap().version, "1.1.0");
        let left: Vec<String> = std::fs::read_dir(data.join("bin")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert!(left.iter().all(|n| !n.starts_with("rclone.old") && n != "rclone.new"), "{left:?}");

        // A record that no longer matches the program (it was replaced some
        // other way) counts for nothing.
        std::fs::write(&exe, b"swapped behind our back").unwrap();
        assert!(installed_source(data).is_none());

        // Leftovers from a replaced copy that was still running.
        std::fs::write(data.join("bin").join("rclone.old-1"), b"x").unwrap();
        std::fs::write(data.join("bin").join("rclone.new"), b"x").unwrap();
        tidy_engine_folder(data);
        assert!(!data.join("bin").join("rclone.old-1").exists());
        assert!(!data.join("bin").join("rclone.new").exists());
        assert!(exe.exists());
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("rclone v1.76.0\n- os/version: x"), Some(("1.76.0".into(), (1, 76))));
        assert_eq!(parse_version("rclone v1.65.2-beta.1"), Some(("1.65.2".into(), (1, 65))));
        assert!(parse_version("nonsense").is_none());
        assert!((1, 76) >= MIN_VERSION && (1, 65) < MIN_VERSION);
        assert!(version_triple("1.76.1") > version_triple("1.76.0"));
        assert!(version_triple("1.100.0") > version_triple("1.99.9"));
        assert!(version_triple("1.76.0") == version_triple("v1.76.0"));
    }

    #[test]
    fn log_lines() {
        let l = parse_line(r#"{"time":"x","level":"error","msg":"Failed to copy: boom","object":"a/b.txt","objectType":"*local.Object","source":"s"}"#).unwrap();
        assert_eq!(l.level, "error");
        assert_eq!(l.object, "a/b.txt");
        let s = parse_line(r#"{"level":"notice","msg":"x","stats":{"bytes":3141632,"totalBytes":6000000,"speed":3145623.5,"eta":2,"transfers":0,"totalTransfers":1,"errors":0,"deletes":0,"transferring":[{"name":"big.bin","percentage":52}]}}"#).unwrap();
        let st = s.stats.unwrap();
        assert_eq!(st.total_bytes, 6000000);
        assert_eq!(st.eta, Some(2));
        assert_eq!(st.current, vec!["big.bin".to_string()]);
        let n = parse_line(r#"{"level":"notice","msg":"y","stats":{"bytes":0,"eta":null}}"#).unwrap();
        assert_eq!(n.stats.unwrap().eta, None);
        assert!(parse_line("not json").is_none());
        // What rclone 1.76 prints when Koofr can't be reached at all, as when
        // a laptop has just woken up and isn't online yet.
        let c = parse_line(r#"{"time":"2026-11-16T15:10:55-05:00","level":"critical","msg":"Failed to create file system for destination \"koofr:Docs\": Get \"https://app.koofr.net/api/v2/mounts\": dial tcp: lookup app.koofr.net: no such host","source":"cmd/cmd.go:208"}"#).unwrap();
        assert!(c.is_error());
        assert!(is_network_error(&c.msg));
        for (level, error) in [("error", true), ("raw", true), ("alert", true), ("emergency", true), ("warning", false), ("notice", false), ("info", false)] {
            assert_eq!(LogLine { level: level.into(), ..Default::default() }.is_error(), error, "{level}");
        }
    }

    #[test]
    fn friendly() {
        assert!(friendly_error(&KOOFR, "open C:\\x.pst: The process cannot access the file because it is being used by another process.").contains("open in another program"));
        assert!(friendly_error(&KOOFR, "dial tcp: lookup app.koofr.net: no such host").contains("Could not reach Koofr"));
        assert_eq!(friendly_error(&KOOFR, "something odd"), "something odd");
        assert!(friendly_error(&KOOFR, "Failed to create file system: unexpected status code 401").contains("did not accept"));
        assert_eq!(friendly_error(&KOOFR, "Failed to copy IMG_14012.jpg: odd"), "Failed to copy IMG_14012.jpg: odd");
        assert!(friendly_error(&KOOFR, "HTTP error 503 returned").contains("Could not reach Koofr"));
        // What Windows says when a connection opened before sleep is dead.
        let woke = r#"error reading destination directory: Get "https://app.koofr.net/api/v2/mounts/x/files/list?path=%2FApps": write tcp 192.168.1.76:51559->176.9.109.126:443: wsasend: An existing connection was forcibly closed by the remote host."#;
        assert!(friendly_error(&KOOFR, woke).contains("Could not reach Koofr"));
        assert!(is_network_error("read tcp 10.0.0.2:1->1.2.3.4:443: wsarecv: An established connection was aborted by the software in your host machine."));
        assert!(is_network_error("dial tcp 1.2.3.4:443: connectex: A connection attempt failed because the connected party did not properly respond after a period of time"));
        assert!(friendly_known(&KOOFR, "something odd").is_none());
        // A number or word in a name is never taken for the error itself.
        assert_eq!(friendly_known(&KOOFR, "open C:\\Users\\r\\Invoice 401.pdf: something odd"), None);
        assert_eq!(friendly_known(&KOOFR, "open /home/r/Sales quota.xlsx: something odd"), None);
        assert_eq!(friendly_known(&KOOFR, "Failed to copy Invoice 401.pdf: odd"), None);
        let busy = "Invalid response status! Got 429, expected 200; headers: map[], content: ";
        assert!(friendly_error(&KOOFR, busy).contains("Limit requests to Koofr"));
        assert!(friendly_error(&KOOFR, "HTTP 429 Too Many Requests").contains("too many requests"));
    }

    #[test]
    fn every_accounts_paths_are_left_out_of_error_words() {
        // A later account's name in the engine starts with "shacct".
        let w = error_words("too many deletes (>50%, 2 of 3) on Path2 \"shacct18ebf41faaec:Quota 401/x\". Run with --force");
        assert!(!w.contains("quota") && !w.contains("401") && w.contains("too many deletes"), "{w}");
        let w = error_words("Failed to create file system for \"shacct9:\": didn't find section in config file (\"shacct9\")");
        assert!(w.contains("didn't find section in config file"), "{w}");
        assert!(is_auth_error(&w));
        // Other words with a colon are kept.
        let w = error_words("dial tcp: lookup api.pcloud.com: no such host");
        assert!(w.contains("no such host") && w.contains("dial tcp"), "{w}");
        assert!(error_words("shacct: not a name").contains("shacct: not a name"));
    }

    #[test]
    fn sign_in_text_is_edited_one_account_at_a_time() {
        let text = "[koofr]\r\ntype = koofr\r\nuser = a@b.c\r\n\r\n[shacct1]\ntype = pcloud\ntoken = {\"access_token\":\"x[y]\"}\nhostname = eapi.pcloud.com\n";
        assert_eq!(section_names(text), vec!["koofr", "shacct1"]);
        assert_eq!(section(text, "koofr").unwrap(), "type = koofr\nuser = a@b.c");
        assert_eq!(section(text, "shacct1").unwrap(), "type = pcloud\ntoken = {\"access_token\":\"x[y]\"}\nhostname = eapi.pcloud.com");
        assert!(section(text, "nobody").is_none());
        let replaced = with_section(text, "koofr", Some("type = koofr\nuser = new@b.c\n"));
        assert_eq!(section_names(&replaced), vec!["shacct1", "koofr"]);
        assert_eq!(section(&replaced, "koofr").unwrap(), "type = koofr\nuser = new@b.c");
        assert_eq!(section(&replaced, "shacct1"), section(text, "shacct1"), "the other account is untouched");
        let removed = with_section(&replaced, "shacct1", None);
        assert_eq!(section_names(&removed), vec!["koofr"]);
        assert_eq!(section_names(&with_section(&removed, "koofr", None)), Vec::<String>::new());
        assert_eq!(section_names(""), Vec::<String>::new());
    }

    #[test]
    fn sign_in_problems_are_told_apart() {
        for raw in [
            "Invalid response status! Got 401, expected 200",
            "couldn't login: wrong email or password",
            "Failed to create file system for \"shacct1:\": Log in required. (Error 1000)",
            "invalid 'access_token' provided. (Error 2094)",
        ] {
            assert!(is_auth_error(&error_words(raw)), "{raw}");
            assert!(friendly_error(&crate::providers::PCLOUD, raw).starts_with("pCloud did not accept the saved sign-in."), "{raw}");
        }
        let locked = "CRITICAL: Failed to load config file \"C:\\Users\\r\\AppData\\Local\\x\\rclone.conf\": unable to decrypt configuration and not allowed to ask for password - set RCLONE_CONFIG_PASS to your configuration password";
        assert!(is_locked_signins(locked));
        assert_eq!(friendly_error(&KOOFR, locked), LOCKED_SIGNINS_TEXT);
        assert!(!is_locked_signins("dial tcp: no such host"));
        // What to do about too many requests depends on the service.
        let busy = "Invalid response status! Got 429, expected 200";
        assert!(friendly_error(&KOOFR, busy).contains("turn on Limit requests to Koofr in this account's settings"));
        assert_eq!(friendly_error(&crate::providers::FILEN, busy), "Filen says SyncHopper is sending too many requests. SyncHopper waits and tries again by itself.");
        assert_eq!(friendly_error(&crate::providers::PCLOUD, "Got 507, expected 200: quota"), "Your pCloud storage is full.");
    }
}
