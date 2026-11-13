//! SyncHopper's entry in File Explorer's right-click menu on Windows 11.
//!
//! Windows 11 only puts an app in its main right-click menu (not just under
//! "Show more options") when the app provides an IExplorerCommand object and
//! has a package identity. This library is that object. It runs in a separate
//! Windows helper process, so a problem here can't take File Explorer down.
//!
//! The menu shows a "SyncHopper" entry with "Send to Koofr" and, for a single
//! folder, "Sync folder to Koofr". Picking one starts SyncHopper with
//! `--upload <path>...` or `--add-folder <path>`. SyncHopper writes the
//! settings this reads to HKEY_CURRENT_USER\Software\SyncHopper\Menu:
//! `Upload` and `AddFolder` (1 to show the item, 0 to hide it) and `Exe` (the
//! path of SyncHopper's program file).

#![cfg(windows)]
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOTIMPL, E_POINTER, HMODULE, S_FALSE, S_OK};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM, SFGAO_FOLDER, SFGAO_STREAM};
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IEnumExplorerCommand_Impl, IExplorerCommand, IExplorerCommand_Impl, IShellItemArray, SHStrDupW, ECF_DEFAULT,
    ECF_HASSUBCOMMANDS, ECS_ENABLED, ECS_HIDDEN, SIGDN_FILESYSPATH,
};
use windows_core::{implement, Interface, Ref, BOOL, GUID, HRESULT, HSTRING, IUnknown, PCWSTR, PWSTR};

/// The class Windows asks for. Must match `com:Class Id` and
/// `desktop5:Verb Clsid` in src-tauri/windows/menu/AppxManifest.xml.
pub const CLSID_MENU: GUID = GUID::from_u128(0x797df41a_f6c3_4e5d_989c_42c4fbcd090a);

const SETTINGS_KEY: &str = r"Software\SyncHopper\Menu";
/// Windows limits a command line to about 32,000 characters, so a large
/// selection is handed over in several launches.
const MAX_COMMAND: usize = 30_000;

// ----- settings and paths ------------------------------------------------

fn flag(name: &str) -> bool {
    windows_registry::CURRENT_USER
        .open(SETTINGS_KEY)
        .and_then(|k| k.get_u32(name))
        .map(|v| v != 0)
        .unwrap_or(false)
}

/// SyncHopper's program file: the path SyncHopper saved, or the program
/// next to this library.
fn exe_path() -> Option<PathBuf> {
    if let Ok(saved) = windows_registry::CURRENT_USER.open(SETTINGS_KEY).and_then(|k| k.get_string("Exe")) {
        let p = PathBuf::from(saved);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut module = HMODULE::default();
    let anchor = exe_path as *const () as *const u16;
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(anchor),
            &mut module,
        )
        .ok()?;
    }
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    let dll = PathBuf::from(String::from_utf16_lossy(&buf[..len]));
    let exe = dll.parent()?.join("synchopper.exe");
    exe.is_file().then_some(exe)
}

/// A string Windows will free itself.
fn shell_string(text: &str) -> windows_core::Result<PWSTR> {
    let h = HSTRING::from(text);
    unsafe { SHStrDupW(&h) }
}

/// What was right-clicked: the file system paths, and whether each one is a
/// folder (zip files count as files).
fn selection(items: Option<&IShellItemArray>) -> Vec<(String, bool)> {
    let Some(items) = items else { return Vec::new() };
    let count = unsafe { items.GetCount() }.unwrap_or(0);
    let mut out = Vec::new();
    for i in 0..count {
        let Ok(item) = (unsafe { items.GetItemAt(i) }) else { continue };
        let Ok(name) = (unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }) else { continue };
        let path = unsafe { name.to_string() }.unwrap_or_default();
        unsafe { CoTaskMemFree(Some(name.0 as *const c_void)) };
        if path.is_empty() {
            continue;
        }
        let mask = SFGAO_FOLDER | SFGAO_FILESYSTEM | SFGAO_STREAM;
        let attrs = unsafe { item.GetAttributes(mask) }.unwrap_or_default();
        let folder = (attrs & SFGAO_FOLDER).0 != 0 && (attrs & SFGAO_FILESYSTEM).0 != 0 && (attrs & SFGAO_STREAM).0 == 0;
        out.push((path, folder));
    }
    out
}

fn launch(args: &[String]) {
    let Some(exe) = exe_path() else { return };
    let _ = std::process::Command::new(exe).args(args).spawn();
}

// ----- the commands ------------------------------------------------------

/// The "SyncHopper" entry. Its items open in a submenu.
#[implement(IExplorerCommand)]
struct Root;

impl IExplorerCommand_Impl for Root_Impl {
    fn GetTitle(&self, _items: Ref<IShellItemArray>) -> windows_core::Result<PWSTR> {
        shell_string("SyncHopper")
    }

    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> windows_core::Result<PWSTR> {
        match exe_path() {
            Some(exe) => shell_string(&format!("{},0", exe.display())),
            None => Err(E_NOTIMPL.into()),
        }
    }

    fn GetToolTip(&self, _items: Ref<IShellItemArray>) -> windows_core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> windows_core::Result<GUID> {
        Ok(CLSID_MENU)
    }

    fn GetState(&self, items: Ref<IShellItemArray>, _ok_to_be_slow: BOOL) -> windows_core::Result<u32> {
        let shown = (flag("Upload") || flag("AddFolder")) && !selection(items.as_ref()).is_empty();
        Ok(if shown { ECS_ENABLED.0 as u32 } else { ECS_HIDDEN.0 as u32 })
    }

    fn Invoke(&self, _items: Ref<IShellItemArray>, _ctx: Ref<IBindCtx>) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetFlags(&self) -> windows_core::Result<u32> {
        Ok(ECF_HASSUBCOMMANDS.0 as u32)
    }

    fn EnumSubCommands(&self) -> windows_core::Result<IEnumExplorerCommand> {
        let items: Vec<IExplorerCommand> = vec![Item { kind: Kind::Send }.into(), Item { kind: Kind::SyncFolder }.into()];
        Ok(Commands { items, next: AtomicUsize::new(0) }.into())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Send,
    SyncFolder,
}

#[implement(IExplorerCommand)]
struct Item {
    kind: Kind,
}

impl IExplorerCommand_Impl for Item_Impl {
    fn GetTitle(&self, _items: Ref<IShellItemArray>) -> windows_core::Result<PWSTR> {
        shell_string(match self.kind {
            Kind::Send => "Send to Koofr",
            Kind::SyncFolder => "Sync folder to Koofr",
        })
    }

    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> windows_core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetToolTip(&self, _items: Ref<IShellItemArray>) -> windows_core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> windows_core::Result<GUID> {
        Err(E_NOTIMPL.into())
    }

    fn GetState(&self, items: Ref<IShellItemArray>, _ok_to_be_slow: BOOL) -> windows_core::Result<u32> {
        let picked = selection(items.as_ref());
        let shown = match self.kind {
            Kind::Send => flag("Upload") && !picked.is_empty(),
            Kind::SyncFolder => flag("AddFolder") && picked.len() == 1 && picked[0].1,
        };
        Ok(if shown { ECS_ENABLED.0 as u32 } else { ECS_HIDDEN.0 as u32 })
    }

    fn Invoke(&self, items: Ref<IShellItemArray>, _ctx: Ref<IBindCtx>) -> windows_core::Result<()> {
        let picked = selection(items.as_ref());
        match self.kind {
            Kind::Send => {
                let mut args: Vec<String> = Vec::new();
                let mut length = 0;
                for (path, _) in picked {
                    if length + path.len() + 12 > MAX_COMMAND && !args.is_empty() {
                        launch(&args);
                        args.clear();
                        length = 0;
                    }
                    length += path.len() + 12;
                    args.push("--upload".into());
                    args.push(path);
                }
                if !args.is_empty() {
                    launch(&args);
                }
            }
            Kind::SyncFolder => {
                if let Some((path, true)) = picked.into_iter().next() {
                    launch(&["--add-folder".to_string(), path]);
                }
            }
        }
        Ok(())
    }

    fn GetFlags(&self) -> windows_core::Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> windows_core::Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

/// Hands Windows the items of the submenu.
#[implement(IEnumExplorerCommand)]
struct Commands {
    items: Vec<IExplorerCommand>,
    next: AtomicUsize,
}

impl IEnumExplorerCommand_Impl for Commands_Impl {
    fn Next(&self, wanted: u32, out: *mut Option<IExplorerCommand>, fetched: *mut u32) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        let mut given = 0u32;
        while given < wanted {
            let i = self.next.load(Ordering::SeqCst);
            let Some(cmd) = self.items.get(i) else { break };
            unsafe { out.add(given as usize).write(Some(cmd.clone())) };
            self.next.store(i + 1, Ordering::SeqCst);
            given += 1;
        }
        if !fetched.is_null() {
            unsafe { fetched.write(given) };
        }
        if given == wanted {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, count: u32) -> windows_core::Result<()> {
        let i = self.next.load(Ordering::SeqCst) + count as usize;
        self.next.store(i.min(self.items.len()), Ordering::SeqCst);
        Ok(())
    }

    fn Reset(&self) -> windows_core::Result<()> {
        self.next.store(0, Ordering::SeqCst);
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IEnumExplorerCommand> {
        Ok(Commands { items: self.items.clone(), next: AtomicUsize::new(self.next.load(Ordering::SeqCst)) }.into())
    }
}

// ----- what Windows calls to load this library ---------------------------

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, iid: *const GUID, object: *mut *mut c_void) -> windows_core::Result<()> {
        if object.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *object = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let cmd: IExplorerCommand = Root.into();
        unsafe { cmd.query(iid, object).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> windows_core::Result<()> {
        Ok(())
    }
}

/// Called by Windows to get the object that creates the menu.
///
/// # Safety
///
/// Windows passes valid pointers to a class ID, an interface ID and a slot
/// for the result.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() || clsid.is_null() {
        return E_POINTER;
    }
    unsafe { *out = std::ptr::null_mut() };
    if unsafe { *clsid } != CLSID_MENU {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory.into();
    unsafe { factory.query(iid, out) }
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    // Staying loaded is harmless and avoids unloading while Windows still
    // holds one of the objects.
    S_FALSE
}
