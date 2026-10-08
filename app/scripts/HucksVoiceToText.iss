; The Windows installer for Huck's Voice to Text - built by release.ps1, in Huck's Snip 'n' Clip's
; shape: per-user, no administrator prompt, a Start menu entry, an optional desktop shortcut and an
; entry in Installed Apps.
;
; Uninstalling removes everything, after one question (his decision, 2026-10-06, as on the Mac):
; the program, and the settings, learned fixes, recovery drafts and speech models it kept in
; %LOCALAPPDATA%\Huck's Voice to Text. An update never runs the uninstaller, so it keeps them.
; The H's Settings > Uninstall... starts this same uninstaller (desktop\src\uninstall.rs).

#ifndef AppVersion
  #error AppVersion must be supplied by release.ps1
#endif
#ifndef SourceExe
  #error SourceExe must be supplied by release.ps1
#endif
#ifndef SourceModel
  #error SourceModel must be supplied by release.ps1
#endif
#ifndef SourceDetector
  #error SourceDetector must be supplied by release.ps1
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
; Dressed like the floating box (app/desktop/ui/style.css): always dark, its near-black panel,
; no divider lines, the white H. The art is drawn by make_install_art.py from docs/icon/mark.svg.
WizardStyle=modern dark hidebevels
WizardBackColor=#171717
WizardBackColorDynamicDark=#171717
WizardImageFile=..\desktop\installer\wizard-100.png,..\desktop\installer\wizard-125.png,..\desktop\installer\wizard-150.png,..\desktop\installer\wizard-175.png,..\desktop\installer\wizard-200.png,..\desktop\installer\wizard-225.png,..\desktop\installer\wizard-250.png
WizardSmallImageFile=..\desktop\installer\wizard-small-100.png,..\desktop\installer\wizard-small-125.png,..\desktop\installer\wizard-small-150.png,..\desktop\installer\wizard-small-175.png,..\desktop\installer\wizard-small-200.png,..\desktop\installer\wizard-small-225.png,..\desktop\installer\wizard-small-250.png
; The welcome page carries the tall panel - the first thing he sees is the box's look.
DisableWelcomePage=no
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

[Messages]
; The one question, in the words the H's Uninstall… uses in the box.
ConfirmUninstall=Uninstall %1?%n%nThis removes the program, your settings and learned fixes, your recovery drafts and the speech models you downloaded. It cannot be undone.
; The program's only surface is its H, so say where Quit is.
UninstallAppRunningError=%1 is still running.%n%nChoose Quit from the H in the notification area, then click OK to continue, or Cancel to leave it installed.

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "{#AppExeName}"; Flags: ignoreversion
; The speech model, so a download works straight away. A model in the app-data folder wins.
Source: "{#SourceModel}"; DestDir: "{app}\models"; Flags: ignoreversion
; The voice detector beside it (hvtt_core::models::VOICE_DETECTOR): it finds where speech ends, so
; the stop press waits about half a second instead of always the full 1.2.
Source: "{#SourceDetector}"; DestDir: "{app}\models"; Flags: ignoreversion
; Vulkan's loader, for a PC whose graphics driver brought none: the program prefers the driver's
; own copy and falls back to this one, which finds no card and leaves recognition to the processor.
Source: "{#SourceVulkan}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceVulkanLicense}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceLicense}"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[InstallDelete]
; The loose Start menu entry a developer install (app\scripts\install.ps1) leaves behind.
Type: files; Name: "{userprograms}\{#AppName}.lnk"

[Registry]
; Start with Windows is switched on from the H, never by the installer. This only makes sure an
; uninstall does not leave the startup entry pointing at a program that is gone.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "HucksVoiceToText"; Flags: uninsdeletevalue dontcreatekey

[Run]
; An update from the app itself runs /VERYSILENT - no installer window at all - and comes back
; running with --updated, so the box can say it is done.
Filename: "{app}\{#AppExeName}"; Description: "Start {#AppName}"; Flags: nowait postinstall; Check: not WizardSilent
Filename: "{app}\{#AppExeName}"; Parameters: "--updated"; Flags: nowait postinstall; Check: WizardSilent and WasInstalled
Filename: "{app}\{#AppExeName}"; Flags: nowait postinstall; Check: WizardSilent and not WasInstalled

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

{ Read before anything is installed: was a copy here already? Only then is a silent install an
  update, and only then does the program start with --updated to say so. }
var
  Existing: Boolean;

function WasInstalled(): Boolean;
begin
  Result := Existing;
end;

function InitializeSetup(): Boolean;
begin
  Existing := RegKeyExists(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{AE21D989-11BC-4DBD-9B5C-9F05997CD30B}_is1');
  Result := True;
  if (not WizardSilent()) and (not WebView2Installed()) then
    MsgBox('Huck''s Voice to Text needs the Microsoft Edge WebView2 Runtime, which this PC does not seem to have.' + #13#10#13#10 +
           'Setup will continue. If the floating box does not appear, install the WebView2 Runtime from Microsoft and start the program again.',
           mbInformation, MB_OK);
end;

{ ---------------------------------------------------------------------------- Uninstall }

function GetFileAttributes(lpFileName: String): DWORD;
  external 'GetFileAttributesW@kernel32.dll stdcall';

{ Started from the H (Settings > Uninstall...), silently, the program is on its way out as this
  begins: wait for it to be gone - it holds the one-copy marker until its process ends - rather
  than give up because it is "still running". Asked for by hand, the message above says what to do. }
function InitializeUninstall(): Boolean;
var
  Tries: Integer;
begin
  Tries := 0;
  while UninstallSilent() and CheckForMutexes('Local\HucksVoiceToText.SingleInstance') and (Tries < 80) do
  begin
    Sleep(250);
    Tries := Tries + 1;
  end;
  Result := True;
end;

{ One of the program's own folders, with all in it. A link standing where the folder would be is
  removed, never followed. The window's helper processes can hold their files for a moment after
  the program has gone, so it is tried for a few seconds. }
procedure RemoveKept(Dir: String);
var
  Tries: Integer;
  Attributes: DWORD;
begin
  Tries := 0;
  while DirExists(Dir) and (Tries < 20) do
  begin
    Attributes := GetFileAttributes(Dir);
    if (Attributes <> $FFFFFFFF) and ((Attributes and $400) <> 0) then
      RemoveDir(Dir)
    else
      DelTree(Dir, True, True, True);
    if DirExists(Dir) then
      Sleep(250);
    Tries := Tries + 1;
  end;
  if DirExists(Dir) then
    Log('Could not remove ' + Dir);
end;

{ Everything the program kept outside its own folder - fixed names, every one its own; nothing is
  searched for. Kept in step with what the program writes (hvtt_core::paths, update.rs). Huck's
  Clipboard is in the running program's memory on Windows, so it has gone already. }
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Temp, ExePath: String;
  Tries: Integer;
begin
  if CurUninstallStep <> usPostUninstall then
    exit;
  { Settings, learned fixes, recovery drafts and downloaded speech models. }
  RemoveKept(ExpandConstant('{localappdata}\Huck''s Voice to Text'));
  { What Microsoft Edge WebView2 keeps for the floating box. }
  RemoveKept(ExpandConstant('{localappdata}\com.huck.voice-to-text'));
  { A downloaded update. }
  Temp := GetEnv('TMP');
  if Temp = '' then
    Temp := GetEnv('TEMP');
  if Temp <> '' then
    RemoveKept(AddBackslash(Temp) + 'HucksVoiceToText-Update');
  { The program's own folder, if it is still there: the window's helper processes stand in it for a
    moment after the program has gone, which keeps Windows from removing it. Only ever when empty. }
  Tries := 0;
  while DirExists(ExpandConstant('{app}')) and (Tries < 20) do
  begin
    if not RemoveDir(ExpandConstant('{app}')) then
      Sleep(250);
    Tries := Tries + 1;
  end;
  { Windows' own record that this program used the microphone (Settings > Privacy > Microphone). }
  ExePath := ExpandConstant('{app}\{#AppExeName}');
  StringChangeEx(ExePath, '\', '#', True);
  RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged\' + ExePath);
end;
