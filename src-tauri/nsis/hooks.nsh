; Custom NSIS hooks for the installer.
; Referenced by tauri.conf.json > bundle.windows.nsis.installerHooks
;
; Two things here are worth copying into your own app, and both were learned
; the hard way:
;
;   1. The double kill. `taskkill /T` walks the process TREE. A sidecar or
;      worker spawned with CREATE_NEW_PROCESS_GROUP is NOT in that tree, so it
;      survives, keeps its file handles open, and the uninstaller fails with
;      "file in use". The WMIC pass catches the escapees by command line.
;
;   2. The $UpdateMode guards. Tauri's NSIS installer runs the UNinstaller as
;      part of an update. Without `${AndIf} $UpdateMode <> 1` every auto-update
;      would silently delete the user's data on the way through. This is the
;      single most destructive bug this file prevents.

!macro NSIS_HOOK_PREUNINSTALL
  ; Kill background processes to prevent "file in use" errors during uninstall.

  ; Kill main app and sidecar (/F=force, /T=tree kill)
  nsExec::ExecToStack 'taskkill /F /IM "${MAINBINARYNAME}.exe" /T'
  Pop $0
  Pop $0
  nsExec::ExecToStack 'taskkill /F /IM "py-sidecar.exe" /T'
  Pop $0
  Pop $0

  ; Kill detached Python workers that escaped the process tree. In dev mode the
  ; sidecar runs as `python -m sidecar.loader`, so the command line — not the
  ; image name — is what identifies it. Replace the needle if you rename the
  ; module. `pythonw.exe` is the windowless variant and needs its own pass.
  nsExec::ExecToStack 'wmic process where "name='"'"'pythonw.exe'"'"' and commandline like '"'"'%sidecar.loader%'"'"'" call terminate'
  Pop $0
  Pop $0
  nsExec::ExecToStack 'wmic process where "name='"'"'python.exe'"'"' and commandline like '"'"'%sidecar.loader%'"'"'" call terminate'
  Pop $0
  Pop $0

  ; Wait for processes to terminate and release file handles
  Sleep 1000

  ; Retry for stubborn processes that may be mid-I/O when the first pass ran
  nsExec::ExecToStack 'taskkill /F /IM "py-sidecar.exe"'
  Pop $0
  Pop $0
  nsExec::ExecToStack 'wmic process where "name='"'"'pythonw.exe'"'"' and commandline like '"'"'%sidecar.loader%'"'"'" call terminate'
  Pop $0
  Pop $0

  Sleep 500
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Delete internal app data if the checkbox was selected — but NEVER during an
  ; update. $UpdateMode = 1 means Tauri is running this uninstaller as a step of
  ; installing a newer version; wiping the data directory there would destroy
  ; the user's database on every auto-update.
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    SetShellVarContext current
    ; Must match paths::get_data_dir() in src-tauri/src/paths.rs
    RmDir /r "$APPDATA\TauriPythonSidecar"

    ; Clean up Windows Credential Manager entries, if your app stores secrets
    ; there. This template stores none, so the sweep is a no-op — it is kept
    ; because getting it right is non-obvious: Python's `keyring` (>= 24)
    ; writes LegacyGeneric targets named "username@service", so a single
    ; `cmdkey /delete` on the service name misses every entry. Enumerating and
    ; matching covers both static keys and dynamically named ones.
    ; Replace the needle with your own credential service name.
    nsExec::ExecToStack 'powershell -NoProfile -Command "cmdkey /list | Select-String ''tauri-python-sidecar'' | ForEach-Object { if ($_ -match ''Target:\s*\S+:target=(.+)'') { cmdkey /delete:$($Matches[1]) } }"'
    Pop $0
    Pop $0
  ${EndIf}

  ; Prompt before deleting a user-chosen data folder (again, never on update).
  ; The path is read from the registry because the user may have relocated it;
  ; the hardcoded fallback is only for installs that never wrote the key.
  ${If} $UpdateMode <> 1
    ReadRegStr $R0 HKCU "Software\TauriPythonSidecar" "DataFolder"
    ${If} $R0 == ""
      StrCpy $R0 "$DOCUMENTS\tauri-python-sidecar"
    ${EndIf}
    ${If} ${FileExists} "$R0\*"
      MessageBox MB_YESNO|MB_ICONQUESTION \
        "Do you also want to delete your user data?$\n$\n\
Location: $R0$\n$\n\
WARNING: This action is PERMANENT and cannot be undone.$\n$\n\
Click 'No' to keep your data for a future reinstall." \
        IDYES userData_delete IDNO userData_done

      userData_delete:
        RmDir /r "$R0"
        DeleteRegKey HKCU "Software\TauriPythonSidecar"

      userData_done:
    ${EndIf}
  ${EndIf}
!macroend
