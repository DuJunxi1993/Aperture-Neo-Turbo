; Aperture Neo Turbo Inno Setup Script
; Requires Inno Setup 6.0 or later
; Compiled with:
;   ISCC /DMyAppVersion=1.0.0 /O"..\publish" /F"Aperture-Neo-Turbo-Setup-v1.0.0" Installer\installer.iss
;
; Bilingual installer (English + Chinese Simplified). Inno Setup shows a
; language selection dialog at startup when 2+ languages are listed in
; the [Languages] section; the chosen language is then used for every
; [Messages], [Tasks] description, [CustomMessages] lookup.
; English is the default (first in the list); Chinese(simplified) second.
;
; Per-user, no-admin installer:
;   - PrivilegesRequired=lowest (never elevates, no UAC)
;   - Installs to {localappdata}\Programs\ApertureNeoTurbo
;   - All registry keys under HKCU
;   - Detects an existing installation of the same AppId and silently
;     uninstalls it (after user confirmation) before installing the new
;     version. User settings/cache are preserved by the app (they live
;     under %APPDATA%\ApertureNeoTurbo and %LOCALAPPDATA%\ApertureNeoTurbo).
;   - Kills any running aperture-neo-turbo.exe before uninstalling.
;
; v1.0.0 features:
;   - Registers the app as the default image viewer via per-user file
;     associations (HKCU\Software\Classes\<ProgId> + extension defaults),
;     toggled by the assocfiles task (default checked).
;   - Per-user file associations for supported image formats: jpg, jpeg,
;     png, bmp, gif, tiff, tif, webp, heic, heif, avif, ico.
;   - Desktop + Start-Menu shortcuts (desktop icon default off).
;
; NO runtime dependencies to install. The exe is statically linked
; (Rust stdlib, Skia, bundled SQLite) and uses only OS-built-in
; D3D12/DXGI/D2D/WIC DLLs. Unlike the C# ApertureNeo (which needs
; .NET 10 Desktop Runtime + WebView2), there is nothing to detect or
; bootstrap here.

#define MyAppName "Aperture Neo Turbo"
#define MyAppPublisher "DuJunxi1993"
#define MyAppExeName "aperture-neo-turbo.exe"
#define MyAppURL "https://github.com/DuJunxi1993/Aperture-Neo-Turbo"

[Setup]
; Unique to Turbo — do NOT reuse the C# "{1B6E2D4A-...}" AppId so the
; two products can coexist.
AppId={{F7A2C4B1-9E4D-4B85-A6F2-3C8D5E91B0A7}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
; Per-user install path (no admin required). {userpf} resolves to
; %LOCALAPPDATA%\Programs on Windows 7+, the standard per-user location.
DefaultDirName={userpf}\{#MyAppName}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
OutputDir=..\publish
OutputBaseFilename=Aperture-Neo-Turbo-Setup
SetupIconFile=..\assets\apertureneo_turbo.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
; Per-user, never elevate. "lowest" = install without requesting admin.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
UninstallDisplayName={#MyAppName}
VersionInfoVersion={#MyAppVersion}
VersionInfoCompany={#MyAppPublisher}
VersionInfoProductName={#MyAppName}
VersionInfoDescription={#MyAppName} Setup

[Languages]
; English first (international default), Chinese(simplified) second.
; The Chinese (Simplified) language file is not bundled with the standard
; Inno Setup install, so we ship a copy in Languages/ChineseSimplified.isl
; and reference it via a relative path.
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "chinesesimp"; MessagesFile: "Languages\ChineseSimplified.isl"

[Messages]
; Default (English) WelcomeLabel2. The Chinese override takes effect when
; the user picks 简体中文 at the language dialog.
WelcomeLabel2=This will install [name/ver] on your computer.%n%nAperture Neo Turbo is a fast, lightweight GPU image viewer.%n%nNo runtime dependencies are installed - this app is fully self-contained.%n%nThis installer runs in your user profile (no administrator rights required).
WelcomeLabel2=即将在您的电脑上安装 [name/ver]。%n%nAperture Neo Turbo 是一款快速、轻量级的 GPU 图片查看器。%n%n本程序自带全部运行时依赖,无需额外安装。%n%n本安装器在您的用户配置目录下运行,无需管理员权限。; Languages: chinesesimp

[Tasks]
Name: "desktopicon"; Description: "{cm:Task_DesktopIcon_Description}"; GroupDescription: "{cm:Task_Group_Shortcuts}"

; v1.0.0: per-user file associations for supported image formats.
; Registers ApertureNeoTurbo.Image ProgID under HKCU\Software\Classes
; and points .jpg/.png/... at it. Removed on uninstall. Default checked.
Name: "assocfiles"; Description: "{cm:Task_AssocFiles_Description}"; GroupDescription: "{cm:Task_Group_Shell}"; Check: AssocFilesShouldBeChecked

; v1.0.0: optional cleanup of old per-user app data (thumbnails,
; settings, recent files). Useful for fresh-start scenarios or
; troubleshooting. Default unchecked — the user must opt in.
Name: "cleandata"; Description: "{cm:Task_CleanData_Description}"; GroupDescription: "{cm:Task_Group_Shell}"

[CustomMessages]
; English entries without a `; Languages:` qualifier are the default;
; the `; Languages: chinesesimp` overrides apply for 简体中文.

PreviousVersionPrompt=A previous version of Aperture Neo Turbo was detected on this computer.%n%nIt will be uninstalled automatically before this new version is installed.%n%nYour settings, thumbnails and favorites will be preserved.%n%nContinue?
PreviousVersionPrompt=检测到您的电脑已安装了旧版 Aperture Neo Turbo。%n%n安装新版之前,系统将自动卸载旧版。%n%n您的设置、缩略图和收藏夹将被保留。%n%n是否继续?; Languages: chinesesimp
UninstallFailedMsg=Failed to launch the previous version's uninstaller:%n%1%n%nPlease remove it manually (Settings - Apps - Installed apps) and run this installer again.
UninstallFailedMsg=无法启动旧版的卸载程序:%n%1%n%n请手动卸载旧版(设置 → 应用 → 已安装的应用),然后再次运行本安装程序。; Languages: chinesesimp

Task_DesktopIcon_Description=Create a &desktop shortcut
Task_DesktopIcon_Description=创建桌面快捷方式(&D); Languages: chinesesimp
Task_Group_Shortcuts=Shortcuts:
Task_Group_Shortcuts=快捷方式:; Languages: chinesesimp

Task_AssocFiles_Description=Register &Aperture Neo Turbo as the default image viewer
Task_AssocFiles_Description=将 Aperture Neo Turbo 注册为默认图片查看器(&A); Languages: chinesesimp

Task_CleanData_Description=Clear old &app data (thumbnails, settings, recent files)
Task_CleanData_Description=清除旧的应用程序数据(&A)(缩略图、设置、最近文件); Languages: chinesesimp

Task_Group_Shell=Shell integration:
Task_Group_Shell=系统集成:; Languages: chinesesimp

Run_Launch_Description=Launch {#MyAppName}
Run_Launch_Description=启动 {#MyAppName}; Languages: chinesesimp

[Files]
; Release binary — set via /dReleaseDir="<abs>" at compile time (e.g.
; ISCC /dReleaseDir="D:\...\aperture-neo-turbo\target\release").
Source: "{#ReleaseDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userstartmenu}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{userstartmenu}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{userdesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; v1.0.0: per-user file associations. Registers the ProgID and sets each
; supported image extension to open with Aperture Neo Turbo. All keys are
; removed on uninstall (uninsdeletekey for ProgID, uninsdeletevalue for
; extension defaults). Tasks: assocfiles so the user can opt out.
Root: HKCU; Subkey: "Software\Classes\ApertureNeoTurbo.Image"; ValueType: string; ValueName: ""; ValueData: "Aperture Neo Turbo Image"; Flags: uninsdeletekey; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\ApertureNeoTurbo.Image\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExeName},0"; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\ApertureNeoTurbo.Image\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.jpg"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.jpeg"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.png"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.bmp"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.gif"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.tiff"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.tif"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.webp"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.heic"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.heif"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.avif"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles
Root: HKCU; Subkey: "Software\Classes\.ico"; ValueType: string; ValueName: ""; ValueData: "ApertureNeoTurbo.Image"; Flags: uninsdeletevalue; Tasks: assocfiles

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:Run_Launch_Description}"; Flags: nowait postinstall skipifsilent

[Code]
const
  AppIdGuid = '{F7A2C4B1-9E4D-4B85-A6F2-3C8D5E91B0A7}';

function AssocFilesShouldBeChecked(): Boolean;
begin
  Result := True;
end;

// Resolve a previous installer (same AppId) and, if present, run its
// unins000.exe silently so this installer can replace it. After user
// confirmation. Returns True if the new install should proceed.
procedure KillAppProcess();
var
  ResultCode: Integer;
begin
  Exec('taskkill.exe', '/F /IM aperture-neo-turbo.exe /T', '', SW_HIDE,
       ewWaitUntilTerminated, ResultCode);
  if ResultCode = 0 then
    Log('Killed running aperture-neo-turbo.exe processes.')
  else
    Log('taskkill.exe returned ' + IntToStr(ResultCode) +
        ' (process was likely not running).');
end;

function RemovePreviousVersion(): Boolean;
var
  UninstallKey: String;
  UninstallStr: String;
  UninstDir: String;
  UninstExe: String;
  ResultCode: Integer;
  Found: Boolean;
  FileFound: Boolean;
begin
  Result := True;
  Found := False;
  UninstallKey := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\' + AppIdGuid + '_is1';

  if RegQueryStringValue(HKLM, UninstallKey, 'UninstallString', UninstallStr) then
    Found := True
  else if RegQueryStringValue(HKCU, UninstallKey, 'UninstallString', UninstallStr) then
    Found := True;

  if not Found then Exit;

  if (UninstallStr <> '') and (UninstallStr[1] = '"') then
    UninstallStr := Copy(UninstallStr, 2, Length(UninstallStr) - 2);
  UninstDir := ExtractFilePath(UninstallStr);

  KillAppProcess();

  UninstExe := '';
  FileFound := False;
  if FileExists(UninstDir + 'unins000.exe') then
  begin
    UninstExe := UninstDir + 'unins000.exe';
    FileFound := True;
  end
  else if FileExists(UninstDir + 'unins001.exe') then
  begin
    UninstExe := UninstDir + 'unins001.exe';
    FileFound := True;
  end
  else if FileExists(UninstDir + 'unins002.exe') then
  begin
    UninstExe := UninstDir + 'unins002.exe';
    FileFound := True;
  end;

  if not FileFound then
  begin
    Log('Previous version uninstaller not found at ' + UninstDir +
        ' (unins000/001/002.exe all missing); skipping uninstall.');
    Exit;
  end;

  if MsgBox(
    CustomMessage('PreviousVersionPrompt'),
    mbConfirmation, MB_YESNO) = IDNO then
  begin
    Result := False;
    Exit;
  end;

  if not Exec(UninstExe, '/SILENT /NORESTART', '', SW_HIDE,
              ewWaitUntilTerminated, ResultCode) then
  begin
    if MsgBox(
      FmtMessage(CustomMessage('UninstallFailedMsg'), [UninstExe]),
      mbError, MB_YESNO) = IDNO then
      Result := False;
  end
  else if ResultCode <> 0 then
  begin
    Log('Previous version uninstaller exited with code ' + IntToStr(ResultCode) +
        '; continuing with new install.');
  end;
end;

// v1.0.0: delete old per-user app data. Thumbnail cache lives in
// %LOCALAPPDATA%\ApertureNeoTurbo\thumbs\cache.db; settings + recents in
// %APPDATA%\ApertureNeoTurbo\. Both are recreated by the app on launch.
procedure CleanOldUserData();
var
  LocalDir: string;
  AppDataDir: string;
begin
  Log('Cleaning old per-user app data (task: cleandata)...');

  LocalDir := GetEnv('LOCALAPPDATA');
  if LocalDir <> '' then
  begin
    LocalDir := AddBackslash(LocalDir) + 'ApertureNeoTurbo';
    Log('Deleting local app data (thumbnails): ' + LocalDir);
    DelTree(LocalDir, True, True, True);
  end;

  AppDataDir := ExpandConstant('{userappdata}\ApertureNeoTurbo');
  Log('Deleting app data (settings/recents): ' + AppDataDir);
  DelTree(AppDataDir, True, True, True);

  Log('Old per-user app data cleared.');
end;

function InitializeSetup(): Boolean;
begin
  if not RemovePreviousVersion() then
  begin
    Result := False;
    Exit;
  end;
  // No runtime dependencies to detect or install — the exe is
  // statically linked (see the header comment).
  Result := True;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
  begin
    if WizardIsTaskSelected('cleandata') then
      CleanOldUserData();
  end;
end;
