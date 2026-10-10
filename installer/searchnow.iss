; SearchNow installer (Inno Setup 6). Build:  scripts\build-release.ps1 -Installer
; Per-user install: no administrator rights needed.

#define AppName "SearchNow"
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#define AppExe "SearchNow.exe"
#define ModelUrlBase "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/"
; Normal quality (default). Keep in sync with crates/searchnow-proto/src/models.rs
#define ModelFile "embeddinggemma-2-Q8_0.gguf"
#define ModelSha "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135"
#define MmprojFile "mmproj-embeddinggemma-2-Q8_0.gguf"
#define MmprojSha "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f"

[Setup]
AppId={{6F2A6A6E-5E0B-4F5A-9C7B-5EA4C4E9A1D1}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=SearchNow
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\dist
OutputBaseFilename=SearchNow-Setup-{#AppVersion}
SetupIconFile=..\ui\SearchNow.App\Assets\SearchNow.ico
UninstallDisplayIcon={app}\{#AppExe}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes
AppMutex=SearchNow.Instance
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "autostart"; Description: "Start SearchNow when I sign in"; GroupDescription: "Options:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Options:"; Flags: unchecked

[Files]
; the published, self-contained app (see scripts\build-release.ps1)
Source: "..\dist\app\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion createallsubdirs

[Icons]
Name: "{userprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{userdesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "SearchNow"; \
    ValueData: """{app}\{#AppExe}"" --background"; Tasks: autostart; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#AppExe}"; Description: "Start SearchNow now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "KillApp"
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM searchnow-server.exe"; Flags: runhidden; RunOnceId: "KillServer"

[Code]
var
  HotkeyPage: TInputQueryWizardPage;
  DownloadPage: TDownloadWizardPage;

function DataDir: String;
begin
  Result := ExpandConstant('{localappdata}\SearchNow');
end;

function ModelsDir: String;
begin
  Result := DataDir + '\models';
end;

{ ---------- hotkey validation: at least one modifier + exactly one key ---------- }

function IsValidKey(const K: String): Boolean;
var
  U: String;
  N: Integer;
begin
  U := Uppercase(K);
  Result := False;
  if Length(U) = 1 then
    Result := ((U[1] >= 'A') and (U[1] <= 'Z')) or ((U[1] >= '0') and (U[1] <= '9'))
  else if (U = 'SPACE') or (U = 'ENTER') or (U = 'TAB') or (U = 'ESCAPE') or (U = 'BACK') then
    Result := True
  else if (Length(U) >= 2) and (U[1] = 'F') then
  begin
    N := StrToIntDef(Copy(U, 2, 2), 0);
    Result := (N >= 1) and (N <= 24);
  end;
end;

function IsModifier(const T: String): Boolean;
var
  U: String;
begin
  U := Uppercase(T);
  Result := (U = 'CTRL') or (U = 'CONTROL') or (U = 'ALT') or (U = 'SHIFT') or (U = 'WIN');
end;

function IsValidHotkey(const Text: String): Boolean;
var
  S, Tok: String;
  P, Mods, Keys: Integer;
begin
  S := Text;
  Mods := 0;
  Keys := 0;
  Result := False;
  while S <> '' do
  begin
    P := Pos('+', S);
    if P = 0 then
    begin
      Tok := Trim(S);
      S := '';
    end
    else
    begin
      Tok := Trim(Copy(S, 1, P - 1));
      S := Copy(S, P + 1, Length(S));
    end;
    if IsModifier(Tok) then
      Mods := Mods + 1
    else if IsValidKey(Tok) then
      Keys := Keys + 1
    else
      Exit;
  end;
  Result := (Mods >= 1) and (Keys = 1);
end;

function NormalizeHotkey(const Text: String): String;
var
  S, Tok: String;
  P: Integer;
begin
  S := Text;
  Result := '';
  while S <> '' do
  begin
    P := Pos('+', S);
    if P = 0 then
    begin
      Tok := Trim(S);
      S := '';
    end
    else
    begin
      Tok := Trim(Copy(S, 1, P - 1));
      S := Copy(S, P + 1, Length(S));
    end;
    if Result <> '' then
      Result := Result + '+';
    Result := Result + Tok;
  end;
end;

{ ---------- wizard ---------- }

procedure InitializeWizard;
begin
  HotkeyPage := CreateInputQueryPage(wpSelectTasks, 'Global shortcut',
    'Which keys should open SearchNow?',
    'Press this shortcut anywhere to show the search window. You can change it later in the settings.' + #13#10 +
    'Use at least one of Ctrl, Alt, Shift, Win plus a key, for example Ctrl+Alt+Space.');
  HotkeyPage.Add('Shortcut:', False);
  HotkeyPage.Values[0] := 'Ctrl+Alt+Space';

  DownloadPage := CreateDownloadPage('Downloading the search model',
    'SearchNow needs the EmbeddingGemma 2 model (about 870 MB). It is downloaded once and stays on this computer.', nil);
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  ModelOk, MmprojOk: Boolean;
begin
  Result := True;
  if CurPageID = HotkeyPage.ID then
  begin
    if not IsValidHotkey(HotkeyPage.Values[0]) then
    begin
      MsgBox('Please enter a valid shortcut, e.g. Ctrl+Alt+Space (at least one modifier and exactly one key).', mbError, MB_OK);
      Result := False;
    end;
  end
  else if CurPageID = wpReady then
  begin
    ModelOk := FileExists(ModelsDir + '\{#ModelFile}');
    MmprojOk := FileExists(ModelsDir + '\{#MmprojFile}');
    if ModelOk and MmprojOk then
      Exit;
    DownloadPage.Clear;
    if not ModelOk then
      DownloadPage.Add('{#ModelUrlBase}{#ModelFile}', '{#ModelFile}', '{#ModelSha}');
    if not MmprojOk then
      DownloadPage.Add('{#ModelUrlBase}{#MmprojFile}', '{#MmprojFile}', '{#MmprojSha}');
    DownloadPage.Show;
    try
      try
        DownloadPage.Download;
      except
        if DownloadPage.AbortedByUser then
          Log('Model download aborted by user.')
        else
          SuppressibleMsgBox(AddPeriod(GetExceptionMessage), mbCriticalError, MB_OK, IDOK);
        Result := False;
      end;
    finally
      DownloadPage.Hide;
    end;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Cfg, Name: String;
  Names: array[0..1] of String;
  I: Integer;
begin
  if CurStep = ssPostInstall then
  begin
    ForceDirectories(ModelsDir);
    Names[0] := '{#ModelFile}';
    Names[1] := '{#MmprojFile}';
    for I := 0 to 1 do
    begin
      Name := Names[I];
      if (not FileExists(ModelsDir + '\' + Name)) and FileExists(ExpandConstant('{tmp}\') + Name) then
        CopyFile(ExpandConstant('{tmp}\') + Name, ModelsDir + '\' + Name, False);
    end;
    { first install: write the chosen hotkey; an existing config is left alone }
    Cfg := DataDir + '\config.toml';
    if not FileExists(Cfg) then
      SaveStringToFile(Cfg, 'hotkey = "' + NormalizeHotkey(HotkeyPage.Values[0]) + '"' + #13#10, False);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    if (not UninstallSilent) and DirExists(DataDir) then
      if MsgBox('Also delete your SearchNow index, settings and the downloaded model (about 900 MB)?',
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DelTree(DataDir, True, True, True);
  end;
end;
