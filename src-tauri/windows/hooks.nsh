; SyncHopper installer hooks.
;
; When SyncHopper is uninstalled (but not when it is being updated), remove
; the things it added for the current user outside its own folder: its
; entries in File Explorer's sidebar, the right-click entries (the classic ones
; and the Windows 11 one, which is a small registered package) and the
; settings the Windows 11 entry reads. Windows' own uninstall step already
; removes the start-with-Windows entry.
;
; The Windows 11 entry runs inside a Windows helper process (dllhost) that
; keeps synchopper_shell.dll open for a while after a right-click. That
; process is closed first so the file can be replaced or removed.
;
; The IDs removed below must match SIDEBAR_CLSIDS in src-tauri/src/platform.rs
; (one per account that can have a sidebar entry), and the package name must
; match MENU_PACKAGE there.

!define SYNCHOPPER_MENU_PACKAGE "CoverTwoGames.SyncHopper"
!define SYNCHOPPER_PS "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command"

!macro SYNCHOPPER_REMOVE_SIDEBAR CLSID
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\${CLSID}"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel" "${CLSID}"
  DeleteRegKey HKCU "Software\Classes\CLSID\${CLSID}"
!macroend

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
    !insertmacro SYNCHOPPER_REMOVE_SIDEBAR "{09F3086A-BAFB-4D17-ABA8-5765ABD60793}"
    !insertmacro SYNCHOPPER_REMOVE_SIDEBAR "{5C4BD8AC-E81E-437A-B19D-B58E36B7DA07}"
    !insertmacro SYNCHOPPER_REMOVE_SIDEBAR "{2E1A3065-D893-4417-AB0F-357C1FE30567}"
    !insertmacro SYNCHOPPER_REMOVE_SIDEBAR "{5A133181-FBF5-4C02-8532-2CA1D26C6549}"
    !insertmacro SYNCHOPPER_REMOVE_SIDEBAR "{75225F19-ECA4-4813-8872-2D739733D1FB}"
    !insertmacro SYNCHOPPER_REMOVE_SIDEBAR "{2962C468-7746-4B68-958D-79F6029180FB}"
    !insertmacro SYNCHOPPER_REMOVE_SIDEBAR "{AA411EE0-2BE3-45B9-ADF6-98DC292C27A5}"
    !insertmacro SYNCHOPPER_REMOVE_SIDEBAR "{8A030C30-8A43-46AC-A6BE-914CE6819652}"
    DeleteRegKey HKCU "Software\Classes\*\shell\SyncHopper"
    DeleteRegKey HKCU "Software\Classes\Directory\shell\SyncHopper"
    DeleteRegKey HKCU "Software\SyncHopper"
    System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
  ${EndIf}
!macroend
