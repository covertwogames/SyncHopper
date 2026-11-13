; SyncHopper installer hooks.
;
; When SyncHopper is uninstalled (but not when it is being updated), remove
; the things it added for the current user outside its own folder: the Koofr
; entry in File Explorer's sidebar and the "Send to Koofr" right-click entry.
; Windows' own uninstall step already removes the start-with-Windows entry.
; The ID below must match SIDEBAR_CLSID in src-tauri/src/platform.rs.

!define SYNCHOPPER_SIDEBAR_CLSID "{09F3086A-BAFB-4D17-ABA8-5765ABD60793}"

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\${SYNCHOPPER_SIDEBAR_CLSID}"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel" "${SYNCHOPPER_SIDEBAR_CLSID}"
    DeleteRegKey HKCU "Software\Classes\CLSID\${SYNCHOPPER_SIDEBAR_CLSID}"
    DeleteRegKey HKCU "Software\Classes\*\shell\SyncHopper"
    DeleteRegKey HKCU "Software\Classes\Directory\shell\SyncHopper"
    System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
  ${EndIf}
!macroend
