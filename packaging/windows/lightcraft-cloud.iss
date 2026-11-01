; LightCraft Cloud for Windows: the installer (Inno Setup 6.3 or newer).
;
; Per user, no administrator: it installs to %LOCALAPPDATA%\Programs\LightCraft Cloud.
; Build (CI does this, see .github/workflows/release.yml):
;   iscc /DAppVersion=0.2.1 /DStage=<folder with the files> /DOutDir=<folder for the setup> lightcraft-cloud.iss         -> lightcraft-cloud-0.2.1-windows-setup.exe
;   iscc /DAppVersion=0.2.1 /DHome /DStage=...                                                                           -> lightcraft-cloud-0.2.1-windows-home-setup.exe
; <Stage> holds lightcraft-cloud-desktop.exe, librclone.dll, README.txt and the LICENSE-*.txt files. One script builds both editions:
;  * the light one (no /DHome) is the other-PC app, and its file name must never change (every v0.2.0 app looks for exactly it);
;  * the home one (/DHome) has the engine and can be the home PC.
; Both have the same AppId, folder, Start menu entry and Run value: there is one LightCraft Cloud on a PC, and installing one edition
; over the other is an update of it. The one thing that is refused is the light edition over the home one (it would stop the home PC
; publishing): see InitializeSetup. The edition installed is recorded in {app}\edition.txt (a v0.2.0 install has none: it is light).
;
; What it does that matters:
;  * Before it replaces anything it stops a running app with the new exe's `quit` command (the app ends after the job it is running; if it
;    does not end in 60 seconds, or is an older version that does not know `quit`, it is ended; a sync survives that, see docs/desktop-app.md),
;    and it starts the app again afterwards. If the install fails, the old app is started again.
;  * "Start when I log in" writes the same value as the app's Settings box: HKCU\...\Run\LightCraft Cloud = "<exe>" run.
;  * Uninstalling removes the program files and that Run value. The settings (%APPDATA%\lightcraft-cloud-desktop), the state
;    (%LOCALAPPDATA%\lightcraft-cloud-desktop), rclone's config and the photo folder are never touched.

#ifndef AppVersion
  #error Pass /DAppVersion=x.y.z
#endif
#ifndef Stage
  #error Pass /DStage=<folder with the files to install>
#endif
#ifndef OutDir
  #define OutDir "."
#endif

#ifdef Home
  #define Edition "home"
  #define SetupSuffix "windows-home-setup"
  #define DisplayName "LightCraft Cloud (home PC)"
#else
  #define Edition "light"
  #define SetupSuffix "windows-setup"
  #define DisplayName "LightCraft Cloud"
#endif
#define AppName "LightCraft Cloud"
#define AppExe "lightcraft-cloud-desktop.exe"
#define RunKey "Software\Microsoft\Windows\CurrentVersion\Run"

[Setup]
AppId={{AD15624E-CC27-4CAD-9106-87BE24794BB7}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=tinytom21
AppPublisherURL=https://github.com/tinytom21/lightcraft-cloud
AppSupportURL=https://github.com/tinytom21/lightcraft-cloud/issues
AppUpdatesURL=https://github.com/tinytom21/lightcraft-cloud/releases
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutDir}
OutputBaseFilename=lightcraft-cloud-{#AppVersion}-{#SetupSuffix}
SetupIconFile=..\..\apps\desktop\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#DisplayName}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
; the app is stopped and started by the code below (with `quit`), not by Windows' Restart Manager
CloseApplications=no
RestartApplications=no

[Tasks]
Name: "autostart"; Description: "Start when I log in"; GroupDescription: "Options:"

[Files]
Source: "{#Stage}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\librclone.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-MIT.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-APACHE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-rclone.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Stage}\LICENSE-Inter-OFL.txt"; DestDir: "{app}"; Flags: ignoreversion
; the new exe, to run `quit` from before the old one is replaced (an old app from the zip has no `quit` of its own)
Source: "{#Stage}\{#AppExe}"; DestDir: "{tmp}"; Flags: dontcopy

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Parameters: "run"; WorkingDir: "{app}"

[Registry]
; (the value is removed again by the uninstall code below, whoever wrote it)
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"" run"; Check: WantAutostart

[Run]
Filename: "{app}\{#AppExe}"; Parameters: "run"; WorkingDir: "{app}"; Description: "Start {#AppName}"; Flags: nowait postinstall skipifsilent; Check: NotWasRunning

[UninstallDelete]
Type: files; Name: "{app}\edition.txt"
Type: dirifempty; Name: "{app}"

[Code]
var
  WasRunning: Boolean;

function NotWasRunning: Boolean;
begin
  Result := not WasRunning;
end;

const
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{AD15624E-CC27-4CAD-9106-87BE24794BB7}_is1';

{ "home" or "light": the edition installed now ('' when nothing is, or the install is v0.2.0's, which only ever was the light one). }
function InstalledEdition: String;
var
  Dir: String;
  Text: AnsiString;
begin
  Result := '';
  if RegQueryStringValue(HKCU, UninstallKey, 'InstallLocation', Dir) then
    if LoadStringFromFile(AddBackslash(Dir) + 'edition.txt', Text) then
      Result := Trim(String(Text));
end;

function InitializeSetup: Boolean;
begin
  Result := True;
#ifndef Home
  { The light edition cannot publish. Over a home PC's install it would replace the engine and stop the home PC without a word, so it
    does not: nothing is changed, and the exit code is not 0. (The home edition over the light one is an upgrade and is fine.) }
  if InstalledEdition = 'home' then
  begin
    MsgBox('This PC has the home PC edition of LightCraft Cloud, and this is the other-PC edition, which cannot publish your photos. ' +
      'Installing it would stop this PC from being your home PC, so nothing was changed.' + #13#10#13#10 +
      'To go back to the other-PC edition on purpose, uninstall LightCraft Cloud first (Settings > Apps), then run this installer.', mbError, MB_OK);
    Result := False;
  end;
#endif
end;

function PreviouslyInstalled: Boolean;
begin
  Result := RegKeyExists(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{AD15624E-CC27-4CAD-9106-87BE24794BB7}_is1');
end;

function RunValueExists: Boolean;
begin
  Result := RegValueExists(HKCU, '{#RunKey}', '{#AppName}');
end;

{ Whether to write the Run value. A silent run (the app's own update) keeps what the user has: a value that is there is rewritten to point
  here (this is also how someone who ran the zip is moved over), and one that is not stays off; only a first install follows the task. }
function WantAutostart: Boolean;
begin
  if WizardSilent and RunValueExists then
    Result := True
  else if WizardSilent and PreviouslyInstalled then
    Result := False
  else
    Result := WizardIsTaskSelected('autostart');
end;

procedure InitializeWizard;
begin
  { the box starts as the user left it }
  if RunValueExists then
    WizardSelectTasks('autostart')
  else if PreviouslyInstalled then
    WizardSelectTasks('!autostart');
end;

{ End every copy of the app for this user, windows included. Used after `quit` has had its say: the sync is built to survive this. }
procedure EndAllCopies;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(500);
end;

{ Ask the app to stop with `Exe quit`. True when an app was running. }
function StopApp(const Exe: String): Boolean;
var
  Code: Integer;
begin
  Result := False;
  if Exec(Exe, 'quit --wait 60', '', SW_HIDE, ewWaitUntilTerminated, Code) then
  begin
    { 0 stopped, 10 was not running, 11 still running after the wait, 12 did not answer (an older app) }
    Result := Code <> 10;
  end
  else
    Result := True;
  EndAllCopies;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  ExtractTemporaryFile('{#AppExe}');
  WasRunning := StopApp(ExpandConstant('{tmp}\{#AppExe}'));
  Result := '';
end;

{ Record which edition is installed (the light installer refuses to go over a home install). }
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    SaveStringToFile(ExpandConstant('{app}\edition.txt'), '{#Edition}' + #13#10, False);
end;

{ Start the app again if it was running (also when the install failed: the old one is still there). }
procedure DeinitializeSetup;
var
  Code: Integer;
  Exe: String;
begin
  if WasRunning then
  begin
    Exe := ExpandConstant('{app}\{#AppExe}');
    if FileExists(Exe) then
      Exec(Exe, 'run', ExpandConstant('{app}'), SW_SHOWNORMAL, ewNoWait, Code);
  end;
end;

function InitializeUninstall: Boolean;
begin
  StopApp(ExpandConstant('{app}\{#AppExe}'));
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RegDeleteValue(HKCU, '{#RunKey}', '{#AppName}');
end;
