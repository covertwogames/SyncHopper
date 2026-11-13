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

/// The classic right-click entries: one for files, one for folders.
const MENU_KEYS: [&str; 2] = [
    r"HKCU\Software\Classes\*\shell\SyncHopper",
    r"HKCU\Software\Classes\Directory\shell\SyncHopper",
];

/// Where SyncHopper tells its Windows 11 menu entry (shell/src/lib.rs) which
/// items to show and where SyncHopper is.
const MENU_SETTINGS_KEY: &str = r"HKCU\Software\SyncHopper\Menu";

/// The name of the package that gives SyncHopper's Windows 11 menu entry its
/// identity (src-tauri/windows/menu/AppxManifest.xml).
pub const MENU_PACKAGE: &str = "CoverTwoGames.SyncHopper";

/// Adds or removes the classic right-click entries: a "SyncHopper" submenu
/// with "Send to Koofr" and, for folders, "Sync folder to Koofr". On Windows
/// 11 these sit under "Show more options"; they are used when the Windows 11
/// entry can't be.
pub async fn set_legacy_menu(enabled: bool, add_folder: bool, exe: &Path) -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }
    // Start clean, so entries from older versions or items that should no
    // longer be there go away. A missing key is fine.
    for key in MENU_KEYS {
        let _ = run("reg", &["delete", key, "/f"], Duration::from_secs(15)).await;
    }
    if !enabled {
        return Ok(());
    }
    let exe = exe.to_string_lossy().to_string();
    let icon = format!("\"{exe}\"");
    let send = format!("\"{exe}\" --upload \"%1\"");
    let add = format!("\"{exe}\" --add-folder \"%1\"");
    for (key, folders) in [(MENU_KEYS[0], false), (MENU_KEYS[1], true)] {
        let send_key = format!(r"{key}\shell\1send");
        let send_cmd = format!(r"{send_key}\command");
        let add_key = format!(r"{key}\shell\2add");
        let add_cmd = format!(r"{add_key}\command");
        let mut steps: Vec<Vec<&str>> = vec![
            vec!["add", key, "/v", "MUIVerb", "/d", "SyncHopper", "/f"],
            vec!["add", key, "/v", "SubCommands", "/d", "", "/f"],
            vec!["add", key, "/v", "Icon", "/d", &icon, "/f"],
            vec!["add", &send_key, "/ve", "/d", "Send to Koofr", "/f"],
            vec!["add", &send_cmd, "/ve", "/d", &send, "/f"],
        ];
        if folders && add_folder {
            steps.push(vec!["add", &add_key, "/ve", "/d", "Sync folder to Koofr", "/f"]);
            steps.push(vec!["add", &add_cmd, "/ve", "/d", &add, "/f"]);
        }
        for args in steps {
            let (ok, _, err) = run("reg", &args, Duration::from_secs(15)).await?;
            if !ok {
                return Err(format!("Windows would not add the right-click entry: {}", err.trim()));
            }
        }
    }
    Ok(())
}

/// Tells the Windows 11 menu entry which items to show and where SyncHopper
/// is. It reads these every time the menu opens.
pub async fn set_menu_flags(upload: bool, add_folder: bool, exe: &Path) -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }
    let exe = exe.to_string_lossy().to_string();
    let on = |b: bool| if b { "1" } else { "0" };
    for args in [
        vec!["add", MENU_SETTINGS_KEY, "/v", "Upload", "/t", "REG_DWORD", "/d", on(upload), "/f"],
        vec!["add", MENU_SETTINGS_KEY, "/v", "AddFolder", "/t", "REG_DWORD", "/d", on(add_folder), "/f"],
        vec!["add", MENU_SETTINGS_KEY, "/v", "Exe", "/t", "REG_SZ", "/d", &exe, "/f"],
    ] {
        reg(&args).await?;
    }
    Ok(())
}

/// The Windows build number, such as 26100. Windows 11 starts at 22000.
pub async fn windows_build() -> Option<u32> {
    if !cfg!(windows) {
        return None;
    }
    let (ok, out, _) = run(
        "reg",
        &["query", r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion", "/v", "CurrentBuildNumber"],
        Duration::from_secs(15),
    )
    .await
    .ok()?;
    if !ok {
        return None;
    }
    out.lines().find(|l| l.contains("CurrentBuildNumber"))?.split_whitespace().last()?.parse().ok()
}

fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

async fn powershell_checked(script: &str, timeout: Duration) -> Result<String, String> {
    let (ok, out, err) = run("powershell", &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script], timeout).await?;
    if ok {
        Ok(out.trim().to_string())
    } else {
        let msg = err.lines().map(str::trim).filter(|l| !l.is_empty()).take(3).collect::<Vec<_>>().join(" ");
        Err(if msg.is_empty() { "PowerShell reported a problem.".into() } else { msg })
    }
}

/// The version of the menu package that is registered, if any.
pub async fn menu_package_version() -> Option<String> {
    let script = format!("(Get-AppxPackage -Name {} | Select-Object -First 1).Version", ps_quote(MENU_PACKAGE));
    powershell_checked(&script, Duration::from_secs(30)).await.ok().filter(|v| !v.is_empty())
}

pub async fn unregister_menu_package() {
    let script = format!("Get-AppxPackage -Name {} | Remove-AppxPackage", ps_quote(MENU_PACKAGE));
    let _ = powershell_checked(&script, Duration::from_secs(60)).await;
}

/// Makes sure the Windows 11 menu entry is registered for this copy of
/// SyncHopper. Returns false when it can't be used here: older Windows, or
/// a copy built without it (the package only comes with signed releases).
pub async fn ensure_menu_package(exe: &Path, version: &str) -> Result<bool, String> {
    if !cfg!(windows) {
        return Ok(false);
    }
    let Some(dir) = exe.parent() else { return Ok(false) };
    let msix = dir.join("SyncHopper.msix");
    let dll = dir.join("synchopper_shell.dll");
    let have = menu_package_version().await;
    let usable = windows_build().await.unwrap_or(0) >= 22000 && msix.is_file() && dll.is_file();
    if !usable {
        if have.is_some() {
            unregister_menu_package().await;
        }
        return Ok(false);
    }
    let want = format!("{version}.0");
    if have.as_deref() == Some(want.as_str()) {
        return Ok(true);
    }
    if have.is_some() {
        unregister_menu_package().await;
    }
    let script = format!(
        "$ErrorActionPreference = 'Stop'; Add-AppxPackage -Path {} -ExternalLocation {}",
        ps_quote(&msix.to_string_lossy()),
        ps_quote(&dir.to_string_lossy())
    );
    powershell_checked(&script, Duration::from_secs(120)).await.map(|_| true)
}

/// The ID of SyncHopper's entry in File Explorer's sidebar. It never changes,
/// so the uninstaller (src-tauri/windows/hooks.nsh) can find and remove it.
pub const SIDEBAR_CLSID: &str = "{09F3086A-BAFB-4D17-ABA8-5765ABD60793}";

async fn reg(args: &[&str]) -> Result<(), String> {
    let (ok, _, err) = run("reg", args, Duration::from_secs(15)).await?;
    if ok {
        Ok(())
    } else {
        Err(err.trim().to_string())
    }
}

/// Asks File Explorer to pick up changes to its sidebar.
async fn refresh_explorer() {
    let _ = powershell(
        "Add-Type -Namespace SyncHopper -Name Shell -MemberDefinition '[DllImport(\"shell32.dll\")] public static extern void SHChangeNotify(int e, uint f, System.IntPtr a, System.IntPtr b);'; [SyncHopper.Shell]::SHChangeNotify(0x08000000, 0, [System.IntPtr]::Zero, [System.IntPtr]::Zero)",
    )
    .await;
}

/// Adds a "Koofr" entry to File Explorer's sidebar that opens `target`, or
/// removes it when `target` is None. Uses the registry steps Microsoft
/// documents for cloud storage providers, for the current user only.
pub async fn set_sidebar(target: Option<&Path>, exe: &Path) -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }
    let clsid_key = format!(r"HKCU\Software\Classes\CLSID\{SIDEBAR_CLSID}");
    let namespace_key = format!(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{SIDEBAR_CLSID}");
    let hide_key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";
    match target {
        Some(folder) => {
            let folder = folder.to_string_lossy().to_string();
            let icon = format!("{},0", exe.to_string_lossy());
            let icon_key = format!(r"{clsid_key}\DefaultIcon");
            let server_key = format!(r"{clsid_key}\InProcServer32");
            let instance_key = format!(r"{clsid_key}\Instance");
            let bag_key = format!(r"{clsid_key}\Instance\InitPropertyBag");
            let folder_key = format!(r"{clsid_key}\ShellFolder");
            let steps: Vec<Vec<&str>> = vec![
                vec!["add", &clsid_key, "/ve", "/t", "REG_SZ", "/d", "Koofr", "/f"],
                vec!["add", &icon_key, "/ve", "/t", "REG_EXPAND_SZ", "/d", &icon, "/f"],
                vec!["add", &clsid_key, "/v", "System.IsPinnedToNameSpaceTree", "/t", "REG_DWORD", "/d", "0x1", "/f"],
                vec!["add", &clsid_key, "/v", "SortOrderIndex", "/t", "REG_DWORD", "/d", "0x42", "/f"],
                vec!["add", &server_key, "/ve", "/t", "REG_EXPAND_SZ", "/d", r"%SystemRoot%\system32\shell32.dll", "/f"],
                vec!["add", &instance_key, "/v", "CLSID", "/t", "REG_SZ", "/d", "{0E5AAE11-A475-4c5b-AB00-C66DE400274E}", "/f"],
                vec!["add", &bag_key, "/v", "Attributes", "/t", "REG_DWORD", "/d", "0x11", "/f"],
                vec!["add", &bag_key, "/v", "TargetFolderPath", "/t", "REG_EXPAND_SZ", "/d", &folder, "/f"],
                vec!["add", &folder_key, "/v", "FolderValueFlags", "/t", "REG_DWORD", "/d", "0x28", "/f"],
                vec!["add", &folder_key, "/v", "Attributes", "/t", "REG_DWORD", "/d", "0xF080004D", "/f"],
                vec!["add", &namespace_key, "/ve", "/t", "REG_SZ", "/d", "Koofr", "/f"],
                vec!["add", hide_key, "/v", SIDEBAR_CLSID, "/t", "REG_DWORD", "/d", "0x1", "/f"],
            ];
            for args in steps {
                reg(&args).await.map_err(|e| format!("Windows would not add the File Explorer entry: {e}"))?;
            }
        }
        None => {
            // Missing keys are fine, so results are not checked.
            let _ = reg(&["delete", &namespace_key, "/f"]).await;
            let _ = reg(&["delete", hide_key, "/v", SIDEBAR_CLSID, "/f"]).await;
            let _ = reg(&["delete", &clsid_key, "/f"]).await;
        }
    }
    refresh_explorer().await;
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

/// A value that stays the same for a folder when it is renamed or moved on
/// the same drive, so a renamed synced folder can be recognised.
pub fn folder_id(path: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        let handle = winapi_util::Handle::from_path_any(path).ok()?;
        let info = winapi_util::file::information(&handle).ok()?;
        Some(format!("{:x}-{:x}", info.volume_serial_number(), info.file_index()))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(format!("{:x}-{:x}", meta.dev(), meta.ino()))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        None
    }
}

/// True for entries people don't see in File Explorer: hidden or system
/// files on Windows, dot files elsewhere.
pub fn hidden_entry(name: &str, meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        const SYSTEM: u32 = 0x4;
        if meta.file_attributes() & (HIDDEN | SYSTEM) != 0 {
            return true;
        }
    }
    #[cfg(not(windows))]
    let _ = meta;
    name.starts_with('.')
}
