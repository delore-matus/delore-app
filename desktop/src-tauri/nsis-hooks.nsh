!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegKey HKCU "Software\delore\DeLore"
  DeleteRegKey /ifempty HKCU "Software\delore"
!macroend
