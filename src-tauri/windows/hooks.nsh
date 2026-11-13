; SyncHopper installer hooks.
;
; When SyncHopper is uninstalled (but not when it is being updated), remove
; the things it added for the current user outside its own folder: the Koofr
; entry in File Explorer's sidebar, the right-click entries (the classic ones
; and the Windows 11 one, which is a small registered package) and the
; settings the Windows 11 entry reads. Windows' own uninstall step already
; removes the start-with-Windows entry.
;
; The Windows 11 entry runs inside a Windows helper process (dllhost) that
; keeps synchopper_shell.dll open for a while after a right-click. That
; process is closed first so the file can be replaced or removed.
;
; The ID below must match SIDEBAR_CLSID in src-tauri/src/platform.rs, and the
; package name must match MENU_PACKAGE there.

!define SYNCHOPPER_SIDEBAR_CLSID "{09F3086A-BAFB-4D17-ABA8-5765ABD60793}"
!define SYNCHOPPER_MENU_PACKAGE "CoverTwoGames.SyncHopper"
!define SYNCHOPPER_PS "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command"

!macro SYNCHOPPER_RELEASE_MENU_DLL
  nsExec::Exec `${SYNCHOPPER_PS} "Get-Process dllhost -ErrorAction SilentlyContinue | Where-Object { $$_.Modules.FileName -contains '$INSTDIR\synchopper_shell.dll' } | Stop-Process -Force -ErrorAction SilentlyContinue"`
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro SYNCHOPPER_RELEASE_MENU_DLL
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode <> 1
    nsExec::Exec `${SYNCHOPPER_PS} "Get-AppxPackage -Name '${SYNCHOPPER_MENU_PACKAGE}' | Remove-AppxPackage"`
    Pop $0
  ${EndIf}
  !insertmacro SYNCHOPPER_RELEASE_MENU_DLL
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\${SYNCHOPPER_SIDEBAR_CLSID}"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel" "${SYNCHOPPER_SIDEBAR_CLSID}"
    DeleteRegKey HKCU "Software\Classes\CLSID\${SYNCHOPPER_SIDEBAR_CLSID}"
    DeleteRegKey HKCU "Software\Classes\*\shell\SyncHopper"
    DeleteRegKey HKCU "Software\Classes\Directory\shell\SyncHopper"
    DeleteRegKey HKCU "Software\SyncHopper"
    System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
  ${EndIf}
!macroend
