//! Everything that talks to the rclone program: finding it, downloading it,
//! running it and reading its log output.

use crate::platform;
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

#[derive(Clone, Debug)]
pub struct Rclone {
    pub exe: PathBuf,
    pub config: PathBuf,
    pub cache: PathBuf,
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
            .map_err(|_| "Koofr took too long to answer.".to_string())?
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
                            } else if parsed.level == "error" || parsed.level == "raw" {
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

/// True when the text describes a dropped or failed connection.
pub fn is_network_error(raw: &str) -> bool {
    let l = raw.to_lowercase();
    NETWORK_WORDS.iter().any(|w| l.contains(w))
}

/// A plain-language version of an error from the sync engine. Text it does
/// not recognise is passed through, shortened if very long.
pub fn friendly_error(raw: &str) -> String {
    friendly_known(raw).unwrap_or_else(|| {
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
pub fn friendly_known(raw: &str) -> Option<String> {
    let l = raw.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    // An HTTP status code standing on its own, not part of a longer number.
    let code = |c: &str| l.split(|ch: char| !ch.is_ascii_digit()).any(|tok| tok == c);
    let text = if is_blocked_message(raw) {
        BLOCKED_TEXT
    } else if code("401") || has(&["unauthorized", "invalid credentials", "authentication failed", "couldn't login"]) {
        "Koofr did not accept the saved email and app password. Open Settings and run setup again to reconnect."
    } else if has(&["being used by another process", "sharing violation"]) {
        "A file is open in another program. It will sync once that program closes it."
    } else if has(&["source file is being updated", "size changed"]) {
        "A file was still being written while it synced. It will be tried again."
    } else if has(&["too long"]) {
        "A file name is too long for Windows."
    } else if has(&["not enough space", "no space left", "disk full"]) {
        "This PC is out of disk space."
    } else if code("507") || has(&["quota", "insufficient storage", "storage is full"]) {
        "Your Koofr storage is full."
    } else if has(&["access is denied", "permission denied"]) {
        "Windows would not let the app read or write a file."
    } else if is_network_error(raw) || has(&["bad gateway", "service unavailable", "gateway timeout"]) || code("502") || code("503") || code("504") {
        "Could not reach Koofr. It will be tried again automatically."
    } else if has(&["directory not found"]) {
        "A folder was not found."
    } else {
        return None;
    };
    Some(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/engine");

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{FIX}/{name}")).unwrap()
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
        assert_eq!(friendly_error(&blocked), BLOCKED_TEXT);
        assert_eq!(friendly_known(&blocked).as_deref(), Some(BLOCKED_TEXT));
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
    }

    #[test]
    fn friendly() {
        assert!(friendly_error("open C:\\x.pst: The process cannot access the file because it is being used by another process.").contains("open in another program"));
        assert!(friendly_error("dial tcp: lookup app.koofr.net: no such host").contains("Could not reach Koofr"));
        assert_eq!(friendly_error("something odd"), "something odd");
        assert!(friendly_error("Failed to create file system: unexpected status code 401").contains("did not accept"));
        assert_eq!(friendly_error("Failed to copy IMG_14012.jpg: odd"), "Failed to copy IMG_14012.jpg: odd");
        assert!(friendly_error("HTTP error 503 returned").contains("Could not reach Koofr"));
        // What Windows says when a connection opened before sleep is dead.
        let woke = r#"error reading destination directory: Get "https://app.koofr.net/api/v2/mounts/x/files/list?path=%2FApps": write tcp 192.168.1.76:51559->176.9.109.126:443: wsasend: An existing connection was forcibly closed by the remote host."#;
        assert!(friendly_error(woke).contains("Could not reach Koofr"));
        assert!(is_network_error("read tcp 10.0.0.2:1->1.2.3.4:443: wsarecv: An established connection was aborted by the software in your host machine."));
        assert!(is_network_error("dial tcp 1.2.3.4:443: connectex: A connection attempt failed because the connected party did not properly respond after a period of time"));
        assert!(friendly_known("something odd").is_none());
    }
}
