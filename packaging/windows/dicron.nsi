Unicode true

!cd "../.."

!define APP_NAME "Dicron"
!define APP_EXE "dicron.exe"
!ifndef APP_VERSION
!define APP_VERSION "0.0.0-dev"
!endif
!define APP_PUBLISHER "0John-Hong0"
!define APP_REG_NAME "Dicron"
!define APP_PROG_ID "Dicron.File"

Name "${APP_NAME}"
OutFile "dist/DicronSetup-${APP_VERSION}.exe"
InstallDir "$LOCALAPPDATA\Programs\Dicron"

RequestExecutionLevel user
SetCompressor /SOLID lzma

Icon "assets/icon.ico"
UninstallIcon "assets/icon.ico"

Section "Install"
    SetOutPath "$INSTDIR"

    File /r "dist\windows\app\*"

    CreateDirectory "$SMPROGRAMS\Dicron"
    CreateShortcut "$SMPROGRAMS\Dicron\Dicron.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_EXE}" 0

    WriteUninstaller "$INSTDIR\Uninstall.exe"

    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_REG_NAME}" "DisplayName" "${APP_NAME}"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_REG_NAME}" "DisplayVersion" "${APP_VERSION}"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_REG_NAME}" "Publisher" "${APP_PUBLISHER}"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_REG_NAME}" "DisplayIcon" "$INSTDIR\${APP_EXE}"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_REG_NAME}" "UninstallString" "$\"$INSTDIR\Uninstall.exe$\""

    WriteRegStr HKCU "Software\Classes\Applications\${APP_EXE}" "FriendlyAppName" "${APP_NAME}"
    WriteRegStr HKCU "Software\Classes\Applications\${APP_EXE}\shell\open\command" "" "$\"$INSTDIR\${APP_EXE}$\" $\"%1$\""
    WriteRegStr HKCU "Software\Classes\Applications\${APP_EXE}\SupportedTypes" ".dcm" ""
    WriteRegStr HKCU "Software\Classes\Applications\${APP_EXE}\SupportedTypes" ".dicom" ""

    WriteRegStr HKCU "Software\Classes\${APP_PROG_ID}" "" "DICOM File"
    WriteRegStr HKCU "Software\Classes\${APP_PROG_ID}\DefaultIcon" "" "$INSTDIR\${APP_EXE},0"
    WriteRegStr HKCU "Software\Classes\${APP_PROG_ID}\shell\open\command" "" "$\"$INSTDIR\${APP_EXE}$\" $\"%1$\""

    WriteRegStr HKCU "Software\Classes\.dcm\OpenWithProgids" "${APP_PROG_ID}" ""
    WriteRegStr HKCU "Software\Classes\.dicom\OpenWithProgids" "${APP_PROG_ID}" ""

    WriteRegStr HKCU "Software\Classes\*\shell\OpenWithDicron" "MUIVerb" "Open with Dicron"
    WriteRegStr HKCU "Software\Classes\*\shell\OpenWithDicron" "Icon" "$INSTDIR\${APP_EXE}"
    WriteRegStr HKCU "Software\Classes\*\shell\OpenWithDicron\command" "" "$\"$INSTDIR\${APP_EXE}$\" $\"%1$\""

    WriteRegStr HKCU "Software\Classes\Directory\shell\OpenWithDicron" "MUIVerb" "Open folder with Dicron"
    WriteRegStr HKCU "Software\Classes\Directory\shell\OpenWithDicron" "Icon" "$INSTDIR\${APP_EXE}"
    WriteRegStr HKCU "Software\Classes\Directory\shell\OpenWithDicron\command" "" "$\"$INSTDIR\${APP_EXE}$\" $\"%1$\""

    System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
SectionEnd

Section "Uninstall"
    Delete "$SMPROGRAMS\Dicron\Dicron.lnk"
    RMDir "$SMPROGRAMS\Dicron"

    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APP_REG_NAME}"
    DeleteRegKey HKCU "Software\Classes\Applications\${APP_EXE}"
    DeleteRegKey HKCU "Software\Classes\${APP_PROG_ID}"

    DeleteRegValue HKCU "Software\Classes\.dcm\OpenWithProgids" "${APP_PROG_ID}"
    DeleteRegValue HKCU "Software\Classes\.dicom\OpenWithProgids" "${APP_PROG_ID}"

    DeleteRegKey HKCU "Software\Classes\*\shell\OpenWithDicron"
    DeleteRegKey HKCU "Software\Classes\Directory\shell\OpenWithDicron"

    Delete "$INSTDIR\${APP_EXE}"
    Delete "$INSTDIR\*.dll"
    RMDir /r "$INSTDIR\licenses"
    Delete "$INSTDIR\Uninstall.exe"
    RMDir "$INSTDIR"

    System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
SectionEnd