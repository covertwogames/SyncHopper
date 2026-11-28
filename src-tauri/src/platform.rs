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

/// The wording of the two right-click items. With one account they name its
/// service ("Send to Koofr"); with more, SyncHopper asks which account.
pub struct MenuLabels {
    pub send: String,
    pub add_folder: String,
}

/// Adds or removes the classic right-click entries: a "SyncHopper" submenu
/// with a send item and, for folders, a sync item. On Windows 11 these sit
/// under "Show more options"; they are used when the Windows 11 entry can't
/// be.
pub async fn set_legacy_menu(enabled: bool, add_folder: bool, exe: &Path, labels: &MenuLabels) -> Result<(), String> {
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
            vec!["add", &send_key, "/ve", "/d", &labels.send, "/f"],
            vec!["add", &send_cmd, "/ve", "/d", &send, "/f"],
        ];
        if folders && add_folder {
            steps.push(vec!["add", &add_key, "/ve", "/d", &labels.add_folder, "/f"]);
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

/// Tells the Windows 11 menu entry which items to show, how they are worded
/// and where SyncHopper is. It reads these every time the menu opens.
pub async fn set_menu_flags(upload: bool, add_folder: bool, exe: &Path, labels: &MenuLabels) -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }
    let exe = exe.to_string_lossy().to_string();
    let on = |b: bool| if b { "1" } else { "0" };
    for args in [
        vec!["add", MENU_SETTINGS_KEY, "/v", "Upload", "/t", "REG_DWORD", "/d", on(upload), "/f"],
        vec!["add", MENU_SETTINGS_KEY, "/v", "AddFolder", "/t", "REG_DWORD", "/d", on(add_folder), "/f"],
        vec!["add", MENU_SETTINGS_KEY, "/v", "Exe", "/t", "REG_SZ", "/d", &exe, "/f"],
        vec!["add", MENU_SETTINGS_KEY, "/v", "SendTitle", "/t", "REG_SZ", "/d", &labels.send, "/f"],
        vec!["add", MENU_SETTINGS_KEY, "/v", "AddFolderTitle", "/t", "REG_SZ", "/d", &labels.add_folder, "/f"],
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

/// The IDs of SyncHopper's entries in File Explorer's sidebar, one per
/// account that has an entry. They never change, so the uninstaller
/// (src-tauri/windows/hooks.nsh, which lists the same IDs) can find and
/// remove them. The first is the one version 0.4 and earlier used.
pub const SIDEBAR_CLSIDS: [&str; 8] = [
    "{09F3086A-BAFB-4D17-ABA8-5765ABD60793}",
    "{5C4BD8AC-E81E-437A-B19D-B58E36B7DA07}",
    "{2E1A3065-D893-4417-AB0F-357C1FE30567}",
    "{5A133181-FBF5-4C02-8532-2CA1D26C6549}",
    "{75225F19-ECA4-4813-8872-2D739733D1FB}",
    "{2962C468-7746-4B68-958D-79F6029180FB}",
    "{AA411EE0-2BE3-45B9-ADF6-98DC292C27A5}",
    "{8A030C30-8A43-46AC-A6BE-914CE6819652}",
];

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

/// Adds an entry called `name` to File Explorer's sidebar that opens
/// `target`, or removes it when `target` is None. `slot` says which of
/// SyncHopper's entries it is. Uses the registry steps Microsoft documents
/// for cloud storage providers, for the current user only.
pub async fn set_sidebar(slot: usize, name: &str, target: Option<&Path>, icon: &str) -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }
    let Some(clsid) = SIDEBAR_CLSIDS.get(slot).copied() else {
        return Err("SyncHopper has no File Explorer entry left for this account.".into());
    };
    let clsid_key = format!(r"HKCU\Software\Classes\CLSID\{clsid}");
    let namespace_key = format!(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\{clsid}");
    let hide_key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";
    match target {
        Some(folder) => {
            let folder = folder.to_string_lossy().to_string();
            let icon_key = format!(r"{clsid_key}\DefaultIcon");
            let server_key = format!(r"{clsid_key}\InProcServer32");
            let instance_key = format!(r"{clsid_key}\Instance");
            let bag_key = format!(r"{clsid_key}\Instance\InitPropertyBag");
            let folder_key = format!(r"{clsid_key}\ShellFolder");
            let steps: Vec<Vec<&str>> = vec![
                vec!["add", &clsid_key, "/ve", "/t", "REG_SZ", "/d", name, "/f"],
                vec!["add", &icon_key, "/ve", "/t", "REG_EXPAND_SZ", "/d", icon, "/f"],
                vec!["add", &clsid_key, "/v", "System.IsPinnedToNameSpaceTree", "/t", "REG_DWORD", "/d", "0x1", "/f"],
                vec!["add", &clsid_key, "/v", "SortOrderIndex", "/t", "REG_DWORD", "/d", "0x42", "/f"],
                vec!["add", &server_key, "/ve", "/t", "REG_EXPAND_SZ", "/d", r"%SystemRoot%\system32\shell32.dll", "/f"],
                vec!["add", &instance_key, "/v", "CLSID", "/t", "REG_SZ", "/d", "{0E5AAE11-A475-4c5b-AB00-C66DE400274E}", "/f"],
                vec!["add", &bag_key, "/v", "Attributes", "/t", "REG_DWORD", "/d", "0x11", "/f"],
                vec!["add", &bag_key, "/v", "TargetFolderPath", "/t", "REG_EXPAND_SZ", "/d", &folder, "/f"],
                vec!["add", &folder_key, "/v", "FolderValueFlags", "/t", "REG_DWORD", "/d", "0x28", "/f"],
                vec!["add", &folder_key, "/v", "Attributes", "/t", "REG_DWORD", "/d", "0xF080004D", "/f"],
                vec!["add", &namespace_key, "/ve", "/t", "REG_SZ", "/d", name, "/f"],
                vec!["add", hide_key, "/v", clsid, "/t", "REG_DWORD", "/d", "0x1", "/f"],
            ];
            for args in steps {
                reg(&args).await.map_err(|e| format!("Windows would not add the File Explorer entry: {e}"))?;
            }
        }
        None => {
            // Missing keys are fine, so results are not checked.
            let _ = reg(&["delete", &namespace_key, "/f"]).await;
            let _ = reg(&["delete", hide_key, "/v", clsid, "/f"]).await;
            let _ = reg(&["delete", &clsid_key, "/f"]).await;
        }
    }
    refresh_explorer().await;
    Ok(())
}

/// True when the WinFsp driver that the network drive needs is installed.
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
        return Err("The network drive is only available on Windows.".into());
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

/// The folder suggested for an account's synced folders. The first account
/// gets the folder version 0.4 and earlier suggested, "Koofr" in the user's
/// home folder. Every other account gets its own folder inside a
/// "SyncHopper" folder there.
pub fn default_base_folder(first: bool, account_name: &str) -> String {
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
    if home.is_empty() {
        return String::new();
    }
    if first {
        return PathBuf::from(home).join("Koofr").to_string_lossy().to_string();
    }
    let name: String = account_name.trim().chars().map(|c| if r#"<>:"/\|?*"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let name = name.trim_matches([' ', '.']).to_string();
    PathBuf::from(home)
        .join("SyncHopper")
        .join(if name.is_empty() { "Account".to_string() } else { name })
        .to_string_lossy()
        .to_string()
}

/// Locks a small piece of data so only this Windows user on this PC can
/// read it back. Used for the key to the saved sign-ins. None when Windows
/// can't do it.
///
/// Other systems have no equivalent wired in yet, so there the data is
/// returned as it is. The Mac and Linux versions need the keychain here.
pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi::run(data, true)
    }
    #[cfg(not(windows))]
    {
        Some(data.to_vec())
    }
}

/// Reads back what `protect` locked. None when it can't be read here, for
/// example when the file came from another PC or another Windows user.
pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        dpapi::run(data, false)
    }
    #[cfg(not(windows))]
    {
        Some(data.to_vec())
    }
}

/// Windows' own data protection, which ties data to the signed-in user.
#[cfg(windows)]
mod dpapi {
    use std::ffi::c_void;

    #[repr(C)]
    struct Blob {
        len: u32,
        data: *mut u8,
    }

    /// Never show a prompt.
    const UI_FORBIDDEN: u32 = 0x1;

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const Blob,
            description: *const u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *const c_void,
            flags: u32,
            data_out: *mut Blob,
        ) -> i32;
        fn CryptUnprotectData(
            data_in: *const Blob,
            description: *mut *mut u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *const c_void,
            flags: u32,
            data_out: *mut Blob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    pub fn run(data: &[u8], protect: bool) -> Option<Vec<u8>> {
        let input = Blob { len: u32::try_from(data.len()).ok()?, data: data.as_ptr() as *mut u8 };
        let mut output = Blob { len: 0, data: std::ptr::null_mut() };
        let ok = unsafe {
            if protect {
                CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), UI_FORBIDDEN, &mut output)
            } else {
                CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null(), UI_FORBIDDEN, &mut output)
            }
        };
        if ok == 0 || output.data.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(output.data, output.len as usize) }.to_vec();
        unsafe {
            LocalFree(output.data as *mut c_void);
        }
        Some(bytes)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_account_gets_a_folder_of_its_own() {
        let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let home = PathBuf::from(home);
        assert_eq!(default_base_folder(true, "Koofr"), home.join("Koofr").to_string_lossy());
        assert_eq!(default_base_folder(false, "pCloud"), home.join("SyncHopper").join("pCloud").to_string_lossy());
        // Characters Windows refuses in a folder name are swapped out.
        assert_eq!(default_base_folder(false, "Work: a/b?"), home.join("SyncHopper").join("Work_ a_b_").to_string_lossy());
        assert_eq!(default_base_folder(false, " .. "), home.join("SyncHopper").join("Account").to_string_lossy());
    }

    #[test]
    fn sidebar_entries_have_ids_of_their_own() {
        let mut ids: Vec<&str> = SIDEBAR_CLSIDS.to_vec();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SIDEBAR_CLSIDS.len());
        assert_eq!(SIDEBAR_CLSIDS[0], "{09F3086A-BAFB-4D17-ABA8-5765ABD60793}", "the first is the one earlier versions used");
        // The uninstaller removes every one of them.
        let hooks = include_str!("../windows/hooks.nsh");
        for id in SIDEBAR_CLSIDS {
            assert!(hooks.contains(&format!("SYNCHOPPER_REMOVE_SIDEBAR \"{id}\"")), "{id} is missing from hooks.nsh");
        }
    }

    #[test]
    fn what_is_locked_reads_back() {
        let locked = protect(b"a key").expect("this system can lock");
        assert_eq!(unprotect(&locked).as_deref(), Some(&b"a key"[..]));
    }
}
