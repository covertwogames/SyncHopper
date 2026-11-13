//! Everything that talks to the rclone program: finding it, downloading it,
//! running it and reading its log output.

use crate::platform;
use crate::util::{strip_ansi, Cancel};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Oldest rclone that has the two-way sync options this app relies on.
pub const MIN_VERSION: (u32, u32) = (1, 66);
pub const DOWNLOAD_BASE: &str = "https://downloads.rclone.org";

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
        let mut cmd = self.command(&args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| "Koofr took too long to answer.".to_string())?
            .map_err(|e| format!("Could not start the sync engine: {e}"))?;
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
        let mut cmd = self.command(&["obscure".to_string(), "-".to_string()]);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("Could not start the sync engine: {e}"))?;
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
        let mut cmd = self.command(args);
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                res.exit = -1;
                res.spawn_error = Some(format!("Could not start the sync engine: {e}"));
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

async fn version_of(exe: &Path) -> Option<(String, (u32, u32))> {
    let mut cmd = Command::new(exe);
    cmd.arg("version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    platform::hide_window(&mut cmd);
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output()).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

pub struct Located {
    pub exe: PathBuf,
    pub version: String,
    /// True when this is the copy the app downloaded and keeps up to date.
    pub managed: bool,
}

/// Finds a usable rclone: the app's own copy first, then one on the PATH.
pub async fn locate(data: &Path) -> Option<Located> {
    let own = managed_path(data);
    if own.is_file() {
        if let Some((version, num)) = version_of(&own).await {
            if num >= MIN_VERSION {
                return Some(Located { exe: own, version, managed: true });
            }
        }
    }
    let on_path = PathBuf::from(if cfg!(windows) { "rclone.exe" } else { "rclone" });
    if let Some((version, num)) = version_of(&on_path).await {
        if num >= MIN_VERSION {
            return Some(Located { exe: on_path, version, managed: false });
        }
    }
    None
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
    let actual = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();
    if actual != expected {
        return Err("The downloaded sync engine did not match its published checksum, so it was thrown away. Try again.".to_string());
    }

    let dest = managed_path(data);
    let dest2 = dest.clone();
    tokio::task::spawn_blocking(move || extract(&bytes, &dest2))
        .await
        .map_err(|e| e.to_string())??;
    Ok(dest)
}

fn extract(zip_bytes: &[u8], dest: &Path) -> Result<(), String> {
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
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = dest.with_extension("new");
        std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
        }
        if dest.exists() {
            let _ = std::fs::remove_file(dest);
        }
        return std::fs::rename(&tmp, dest)
            .map_err(|e| format!("Could not put the sync engine in place: {e}"));
    }
    Err("The download did not contain the sync engine program.".to_string())
}

/// Turns raw rclone error text into something a person can act on.
pub fn friendly_error(raw: &str) -> String {
    let l = raw.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| l.contains(w));
    // An HTTP status code standing on its own, not part of a longer number.
    let code = |c: &str| l.split(|ch: char| !ch.is_ascii_digit()).any(|tok| tok == c);
    if code("401") || has(&["unauthorized", "invalid credentials", "authentication failed", "couldn't login"]) {
        "Koofr did not accept the saved email and app password. Open Settings and run setup again to reconnect.".into()
    } else if has(&["being used by another process", "sharing violation"]) {
        "A file is open in another program. It will sync once that program closes it.".into()
    } else if has(&["source file is being updated", "size changed"]) {
        "A file was still being written while it synced. It will be tried again.".into()
    } else if has(&["too long"]) {
        "A file name is too long for Windows.".into()
    } else if has(&["not enough space", "no space left", "disk full"]) {
        "This PC is out of disk space.".into()
    } else if code("507") || has(&["quota", "insufficient storage", "storage is full"]) {
        "Your Koofr storage is full.".into()
    } else if has(&["access is denied", "permission denied"]) {
        "Windows would not let the app read or write a file.".into()
    } else if has(&["no such host", "dial tcp", "connection refused", "i/o timeout", "network is unreachable", "tls handshake", "connection reset", "unexpected eof", "context deadline exceeded", "took too long", "bad gateway", "service unavailable", "gateway timeout"])
        || code("502")
        || code("503")
        || code("504")
     {
        "Could not reach Koofr. It will be tried again automatically.".into()
    } else if has(&["directory not found"]) {
        "A folder was not found.".into()
    } else {
        let t = raw.trim();
        if t.chars().count() > 220 {
            format!("{}...", t.chars().take(220).collect::<String>())
        } else {
            t.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
