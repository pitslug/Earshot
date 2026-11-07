; Hooks for tauri-bundler's installer.nsi, named by the overlay from xtask's `bundle_config`
; (`bundle.windows.nsis.installerHooks`). Pinned line by line by `build_bundle_hook_*` in
; xtask/src/bundle/tests.rs.
;
; The app puts its own exe on the current user's Windows Error Reporting exclusion list at every
; start (wer_win.rs, T9.1t). An uninstall takes that one value away (T9.11) and leaves the rest of
; the list and its key alone. An update keeps it: un.onInit has read /UPDATE before Section
; Uninstall runs.
;
; After the uninstall rather than before it: the uninstall can stop for an app that is still
; running, and that app must stay on the list. `nsis_uninstall` in xtask/src/bundle.rs relies on
; this being the uninstaller's last registry step.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\Windows Error Reporting\ExcludedApplications" "${MAINBINARYNAME}.exe"
    ; A value that cannot be removed only sets the error flag: it never fails the uninstall.
    ClearErrors
  ${EndIf}
!macroend
