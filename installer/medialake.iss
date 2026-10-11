; MediaLake installer (Inno Setup 6). Build:  scripts\windows\build-release.ps1 -Installer
; Per-user install: no administrator rights needed.

#define AppName "MediaLake"
#ifndef AppVersion
  #define AppVersion "0.2.0"
#endif
#define AppExe "MediaLake.exe"
[Setup]
AppId={{6F2A6A6E-5E0B-4F5A-9C7B-5EA4C4E9A1D1}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=MediaLake
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=MediaLake-Setup-{#AppVersion}
LicenseFile=..\LICENSE
SetupIconFile=..\ui\MediaLake.App\Assets\MediaLake.ico
UninstallDisplayIcon={app}\{#AppExe}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes
AppMutex=MediaLake.Instance
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "autostart"; Description: "Start MediaLake when I sign in"; GroupDescription: "Options:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Options:"; Flags: unchecked

[Files]
; the published, self-contained app (see scripts\windows\build-release.ps1)
Source: "..\dist\app\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion createallsubdirs

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "MediaLake"; \
    ValueData: """{app}\{#AppExe}"" --background"; Tasks: autostart; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#AppExe}"; Description: "Start MediaLake now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "KillApp"
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM medialake-server.exe"; Flags: runhidden; RunOnceId: "KillServer"

[Code]
{ The keyboard shortcut and the model are chosen in the "Welcome to MediaLake" window that the app shows
  on its first start (both optional), not here: the installer only copies files. }

function DataDir: String;
begin
  Result := ExpandConstant('{localappdata}\MediaLake');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    if (not UninstallSilent) and DirExists(DataDir) then
      if MsgBox('Also delete your MediaLake index, settings and the downloaded models?',
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DelTree(DataDir, True, True, True);
  end;
end;
