; The Windows installer for Huck's Voice to Text - built by release.ps1, in Huck's Snip 'n' Clip's
; shape: per-user, no administrator prompt, a Start menu entry, an optional desktop shortcut, an
; entry in Installed Apps, and settings, models and recovery drafts left alone on uninstall (they
; live in %LOCALAPPDATA%\Huck's Voice to Text, not here).

#ifndef AppVersion
  #error AppVersion must be supplied by release.ps1
#endif
#ifndef SourceExe
  #error SourceExe must be supplied by release.ps1
#endif
#ifndef SourceModel
  #error SourceModel must be supplied by release.ps1
#endif
#ifndef SourceLicense
  #error SourceLicense must be supplied by release.ps1
#endif
#ifndef OutputDir
  #error OutputDir must be supplied by release.ps1
#endif
#ifndef SetupIcon
  #error SetupIcon must be supplied by release.ps1
#endif

#define AppName "Huck's Voice to Text"
#define AppExeName "HucksVoiceToText.exe"

[Setup]
AppId={{AE21D989-11BC-4DBD-9B5C-9F05997CD30B}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Huck's Project Playground
AppPublisherURL=https://github.com/Huckletsplay/hucks-voice-to-text
AppSupportURL=https://github.com/Huckletsplay/hucks-voice-to-text/issues
AppUpdatesURL=https://github.com/Huckletsplay/hucks-voice-to-text/releases
DefaultDirName={localappdata}\Programs\HucksVoiceToText
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir={#OutputDir}
OutputBaseFilename=HucksVoiceToText-{#AppVersion}-windows-x64-setup
SetupIconFile={#SetupIcon}
UninstallDisplayIcon={app}\{#AppExeName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; The program holds this while it runs (win_surface::claim_single_instance).
AppMutex=Local\HucksVoiceToText.SingleInstance
MinVersion=10.0
VersionInfoVersion={#AppVersion}.0
VersionInfoCompany=Huck's Project Playground
VersionInfoDescription={#AppName} Installer
VersionInfoProductName={#AppName}

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "{#AppExeName}"; Flags: ignoreversion
; The speech model, so a download works straight away. A model in the app-data folder wins.
Source: "{#SourceModel}"; DestDir: "{app}\models"; Flags: ignoreversion
Source: "{#SourceLicense}"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[InstallDelete]
; The loose Start menu entry a developer install (app\scripts\install.ps1) leaves behind.
Type: files; Name: "{userprograms}\{#AppName}.lnk"

[Run]
; No skipifsilent: an update from the app itself runs /SILENT and should come back running.
Filename: "{app}\{#AppExeName}"; Description: "Start {#AppName}"; Flags: nowait postinstall

[Code]
{ The floating box is drawn by Microsoft Edge WebView2. Windows 11 always has it and Windows 10
  almost always does; if it is missing, say so now rather than let the program fail silently. }
function WebView2Installed(): Boolean;
var
  Version: String;
begin
  Result :=
    (RegQueryStringValue(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', Version) and (Version <> '') and (Version <> '0.0.0.0')) or
    (RegQueryStringValue(HKCU, 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}', 'pv', Version) and (Version <> '') and (Version <> '0.0.0.0'));
end;

function InitializeSetup(): Boolean;
begin
  Result := True;
  if (not WizardSilent()) and (not WebView2Installed()) then
    MsgBox('Huck''s Voice to Text needs the Microsoft Edge WebView2 Runtime, which this PC does not seem to have.' + #13#10#13#10 +
           'Setup will continue. If the floating box does not appear, install the WebView2 Runtime from Microsoft and start the program again.',
           mbInformation, MB_OK);
end;
