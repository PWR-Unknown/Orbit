; Orbit installer. Installs for the current user only: NO administrator rights / UAC prompt.
; Build with Inno Setup 6 (free):  iscc /DAppVersion=0.2.0 installer\orbit.iss   (build_windows.bat does this and
; reads the version from Cargo.toml, so the installer and the exe always agree). Run `build_windows.bat release` (or `cargo build --release`) first.
; Player data (%APPDATA%\Orbit) is deliberately left in place on uninstall.
#define AppName "Orbit"
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{B6B1F2E4-5C0D-4E0B-9C57-0A81F4A90001}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Orbit
; The key part: never ask for admin, install under %LOCALAPPDATA%\Programs
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
UninstallDisplayIcon={app}\orbit.exe
SetupIconFile=..\assets\orbit.ico
OutputDir=..\dist
OutputBaseFilename=Orbit-Setup
Compression=lzma2
SolidCompression=yes
; "x64compatible" only exists in Inno Setup 6.3+; older 6.x stops with "invalid value" on it.
#if Ver >= 0x06030000
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#else
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
#endif
; WebView2 (what draws Orbit's window) needs Windows 10 or newer.
MinVersion=10.0
WizardStyle=modern
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "..\target\release\orbit.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\orbit-updater.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\updater.json"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\orbit.exe"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\orbit.exe"; Tasks: desktopicon
Name: "{autoprograms}\Update {#AppName}"; Filename: "{app}\orbit-updater.exe"

[UninstallDelete]
Type: files; Name: "{app}\orbit.exe.old"
Type: files; Name: "{app}\orbit-updater.exe.old"
Type: files; Name: "{app}\.orbit-version"

[Run]
Filename: "{app}\orbit.exe"; Description: "Launch Orbit"; Flags: nowait postinstall skipifsilent

[Code]
const
  WebView2Key = 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';

function HasWebView2: Boolean;
var
  v: String;
begin
  Result := False;
  if RegQueryStringValue(HKLM32, WebView2Key, 'pv', v) then
    Result := (v <> '') and (v <> '0.0.0.0');
  if (not Result) and RegQueryStringValue(HKCU, WebView2Key, 'pv', v) then
    Result := (v <> '') and (v <> '0.0.0.0');
end;

{ Windows 10/11 ship the WebView2 Runtime almost everywhere. If it is missing, Orbit's window can't open,
  so say so now instead of leaving a launcher that silently does nothing. }
function InitializeSetup: Boolean;
var
  code: Integer;
begin
  Result := True;
  if (not WizardSilent) and (not HasWebView2) then
    if MsgBox('Orbit needs the Microsoft Edge WebView2 Runtime, which was not found on this PC.' + #13#10 + #13#10 +
              'Open the download page now? (Install the "Evergreen" runtime, then run this setup again if Orbit does not start.)',
              mbConfirmation, MB_YESNO) = IDYES then
      ShellExec('open', 'https://developer.microsoft.com/microsoft-edge/webview2/', '', '', SW_SHOWNORMAL, ewNoWait, code);
end;

