//! The parts that depend on the operating system. Windows work is done by
//! calling the tools that ship with Windows (reg, powershell, winget,
//! tasklist, taskkill) so there is no Windows-only code to maintain here
//! beyond hiding console windows.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Stops a console window from flashing up when a helper program runs.
pub fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

async fn run(program: &str, args: &[&str], timeout: Duration) -> Result<(bool, String, String), String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    hide_window(&mut cmd);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| format!("{program} took too long."))?
        .map_err(|e| format!("Could not run {program}: {e}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

async fn powershell(script: &str) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let (ok, out, _) = run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script],
        Duration::from_secs(20),
    )
    .await
    .ok()?;
    ok.then(|| out.trim().to_string())
}

/// Some(true) when the PC is running on battery. None when that cannot be told.
pub async fn on_battery() -> Option<bool> {
    let out = powershell(
        "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SystemInformation]::PowerStatus.PowerLineStatus",
    )
    .await?;
    match out.as_str() {
        "Offline" => Some(true),
        "Online" => Some(false),
        _ => None,
    }
}

/// Some(true) when Windows marks the current connection as metered.
pub async fn metered() -> Option<bool> {
    let out = powershell(
        "[void][Windows.Networking.Connectivity.NetworkInformation,Windows,ContentType=WindowsRuntime]; $p=[Windows.Networking.Connectivity.NetworkInformation]::GetInternetConnectionProfile(); if($p){$p.GetConnectionCost().NetworkCostType}",
    )
    .await?;
    match out.as_str() {
        "Fixed" | "Variable" => Some(true),
        "Unrestricted" => Some(false),
        _ => None,
    }
}

const MENU_KEYS: [&str; 2] = [
    r"HKCU\Software\Classes\*\shell\SyncHopper",
    r"HKCU\Software\Classes\Directory\shell\SyncHopper",
];

/// Adds or removes the "Send to Koofr" entry in File Explorer's right-click
/// menu for the current user.
pub async fn set_context_menu(enabled: bool, exe: &Path) -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }
    let exe = exe.to_string_lossy().to_string();
    for key in MENU_KEYS {
        if enabled {
            let command_key = format!(r"{key}\command");
            let icon = format!("\"{exe}\"");
            let command = format!("\"{exe}\" --upload \"%1\"");
            let steps: [Vec<&str>; 3] = [
                vec!["add", key, "/ve", "/d", "Send to Koofr", "/f"],
                vec!["add", key, "/v", "Icon", "/d", &icon, "/f"],
                vec!["add", &command_key, "/ve", "/d", &command, "/f"],
            ];
            for args in steps {
                let (ok, _, err) = run("reg", &args, Duration::from_secs(15)).await?;
                if !ok {
                    return Err(format!("Windows would not add the right-click entry: {}", err.trim()));
                }
            }
        } else {
            // A missing key is fine, so the result is not checked.
            let _ = run("reg", &["delete", key, "/f"], Duration::from_secs(15)).await;
        }
    }
    Ok(())
}

/// True when the WinFsp driver that the Koofr drive needs is installed.
pub async fn winfsp_installed() -> bool {
    if !cfg!(windows) {
        return false;
    }
    for key in [r"HKLM\SOFTWARE\WOW6432Node\WinFsp", r"HKLM\SOFTWARE\WinFsp"] {
        if let Ok((true, out, _)) = run("reg", &["query", key, "/v", "InstallDir"], Duration::from_secs(15)).await {
            if out.contains("InstallDir") {
                return true;
            }
        }
    }
    false
}

/// Installs WinFsp with winget. Windows shows its own permission prompt.
pub async fn install_winfsp() -> Result<(), String> {
    if !cfg!(windows) {
        return Err("The Koofr drive is only available on Windows.".into());
    }
    let result = run(
        "winget",
        &[
            "install", "--id", "WinFsp.WinFsp", "-e", "--silent",
            "--accept-source-agreements", "--accept-package-agreements",
        ],
        Duration::from_secs(600),
    )
    .await;
    match result {
        Ok(_) if winfsp_installed().await => Ok(()),
        Ok((_, out, err)) => {
            let text = format!("{} {}", out.trim(), err.trim());
            let tail: String = text.chars().rev().take(200).collect::<String>().chars().rev().collect();
            Err(format!("The driver install did not finish. {}", tail.trim()))
        }
        Err(_) => Err("Windows could not run its installer tool (winget).".into()),
    }
}

/// Drive letters that are not in use, as single upper case letters.
pub fn free_letters() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    ('D'..='Z')
        .filter(|c| !Path::new(&format!("{c}:\\")).exists())
        .map(|c| c.to_string())
        .collect()
}

pub fn letter_in_use(letter: &str) -> bool {
    cfg!(windows) && Path::new(&format!("{letter}:\\")).exists()
}

/// Stops sync engine processes left behind by an earlier run that ended
/// badly. Only rclone processes that were started with this app's own config
/// file are touched, so any rclone the user runs themselves is left alone.
pub async fn kill_orphans(config_path: &Path) {
    let marker = config_path.to_string_lossy().to_string();
    if marker.trim().is_empty() {
        return;
    }
    if cfg!(windows) {
        let script = format!(
            "$m='{}'; Get-CimInstance Win32_Process -Filter \"Name='rclone.exe'\" | Where-Object {{ $_.CommandLine -and $_.CommandLine.Contains($m) }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}",
            marker.replace('\'', "''")
        );
        let _ = powershell(&script).await;
    } else {
        let me = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else { return };
        for e in entries.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
            if pid == me {
                continue;
            }
            let Ok(raw) = std::fs::read(e.path().join("cmdline")) else { continue };
            let args: Vec<String> = raw.split(|b| *b == 0).map(|a| String::from_utf8_lossy(a).to_string()).collect();
            let is_rclone = args.first().map(|a| a.ends_with("rclone")).unwrap_or(false);
            if is_rclone && args.iter().any(|a| a == &marker) {
                let _ = run("kill", &["-9", &pid.to_string()], Duration::from_secs(15)).await;
            }
        }
    }
}

/// The folder suggested for synced folders: a "Koofr" folder in the user's
/// home folder.
pub fn default_base_folder() -> String {
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
    if home.is_empty() {
        return String::new();
    }
    PathBuf::from(home).join("Koofr").to_string_lossy().to_string()
}

