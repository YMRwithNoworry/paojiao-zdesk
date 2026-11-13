#ifndef SourceDir
#error SourceDir must point to a verified ZSDesk payload
#endif
#ifndef OutputDir
#error OutputDir must point to the installer output directory
#endif
#ifndef MyAppVersion
#error MyAppVersion is required
#endif
#ifndef Edition
#define Edition "full"
#endif
#ifndef EditionName
#define EditionName "全量版"
#endif
#ifndef HasDesktop
#define HasDesktop 1
#endif
#if Edition == "desktop-box"
#define ProductAppId "{{A5AF6D62-4581-458B-9CC4-215C078FDDB6}"
#define ProductName "ZSDesk 桌面盒子"
#define ProductDirectory "ZSDeskDesktopBox"
#define ProductMutex "Local\ZSDesk.DesktopBox.Application"
#define ProductModelId "ZSDesk.DesktopBox"
#elif Edition == "themed"
#define ProductAppId "{{6C520A87-056A-4CD2-9989-F06398B6A284}"
#define ProductName "ZSDesk 主题版"
#define ProductDirectory "ZSDeskTheme"
#define ProductMutex "Local\ZSDesk.Theme.Application"
#define ProductModelId "ZSDesk.Theme"
#else
#define ProductAppId "{{470DF6E5-CCB7-4A3F-9ED0-F68009D5A91A}"
#define ProductName "ZSDesk"
#define ProductDirectory "ZSDesk"
#define ProductMutex "Local\ZSDesk.Native.Application"
#define ProductModelId "ZSDesk.Native"
#endif
#if !FileExists(SourceDir + "\ZSDesk.exe") || !FileExists(SourceDir + "\manifest.json")
#error The executable or verified package manifest is missing
#endif

[Setup]
AppId={#ProductAppId}
AppName={#ProductName}
AppMutex={#ProductMutex}
AppVersion={#MyAppVersion}
AppVerName=ZSDesk {#MyAppVersion} {#EditionName}
AppPublisher=ZSDesk contributors
AppPublisherURL=https://github.com/qiu7824/zsdesk
AppSupportURL=https://github.com/qiu7824/zsdesk/issues
AppUpdatesURL=https://github.com/qiu7824/zsdesk/releases
DefaultDirName={localappdata}\Programs\{#ProductDirectory}
DefaultGroupName={#ProductName}
AllowNoIcons=yes
UsePreviousAppDir=yes
DisableWelcomePage=yes
DisableReadyPage=yes
DisableProgramGroupPage=yes
DisableDirPage=yes
OutputDir={#OutputDir}
OutputBaseFilename=ZSDesk-{#MyAppVersion}-{#Edition}-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
WizardSizePercent=120,120
WizardResizable=no
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.18362
SetupIconFile=..\..\crates\zsdesk-app\resources\zsdesk.ico
UninstallDisplayIcon={app}\ZSDesk.exe
CloseApplications=no
RestartApplications=no
ShowLanguageDialog=auto
LanguageDetectionMethod=uilanguage
VersionInfoDescription=ZSDesk {#EditionName} Setup

[Languages]
Name: "chinesesimp"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{group}\{#ProductName}"; Filename: "{app}\ZSDesk.exe"; AppUserModelID: "{#ProductModelId}"; Check: not WizardNoIcons
Name: "{autodesktop}\{#ProductName}"; Filename: "{app}\ZSDesk.exe"; AppUserModelID: "{#ProductModelId}"; Tasks: desktopicon; Check: not WizardNoIcons
#if (Int(HasDesktop) != 0) && (Edition != "desktop-box")
Name: "{group}\{cm:Fences}"; Filename: "{app}\ZSDesk.exe"; Parameters: "--desktop"; Check: not WizardNoIcons
#endif

[Run]
Filename: "{app}\ZSDesk.exe"; Description: "{cm:LaunchProgram,ZSDesk}"; Flags: nowait postinstall skipifsilent

[CustomMessages]
chinesesimp.Fences=ZSDesk 桌面格子
english.Fences=ZSDesk Desktop Fences
#if Edition == "desktop-box"
chinesesimp.LandingTitle=独立桌面盒子，分类整理。
english.LandingTitle=Standalone desktop fences. Organize your workspace.
chinesesimp.LandingDetail=文件夹映射 · 格子标签页 · 拖放整理
english.LandingDetail=Folder mapping · Fence tabs · Drag and drop
chinesesimp.KeepSettings=保留桌面盒子设置；配置与文件管理版独立。
english.KeepSettings=Desktop box settings are kept separately from the file manager.
#else
chinesesimp.LandingTitle=原生文件管理，桌面有序。
english.LandingTitle=Native file management. An organized desktop.
chinesesimp.LandingDetail=多标签 · 快速搜索 · 文件预览
english.LandingDetail=Tabs · Fast search · File preview
chinesesimp.KeepSettings=保留现有个人设置；文件打开方式可在软件内设置。
english.KeepSettings=Personal settings are kept. Folder integration is configured in the app.
#endif
chinesesimp.InstallFolder=安装位置
english.InstallFolder=Install location
chinesesimp.BrowseFolder=浏览…
english.BrowseFolder=Browse…
chinesesimp.InstallNow=立即安装
english.InstallNow=Install now
chinesesimp.InvalidFolder=请输入完整的安装路径。
english.InvalidFolder=Enter a full installation path.
chinesesimp.RestoreFailed=当前安装的系统集成还原失败，卸载已停止；请在 ZSDesk 中恢复系统集成后重试。
english.RestoreFailed=System integration could not be restored. Uninstall stopped; restore integration in ZSDesk and try again.

[Code]
var
  Landing: TWizardPage;
  Location: TNewEdit;
  Shortcut: TNewCheckBox;

procedure ChooseFolder(Sender: TObject);
var
  Folder: String;
begin
  Folder := Location.Text;
  if BrowseForFolder(CustomMessage('InstallFolder'), Folder, True) then
  begin
    if CompareText(ExtractFileName(RemoveBackslashUnlessRoot(Folder)), '{#ProductDirectory}') <> 0 then
      Folder := AddBackslash(Folder) + '{#ProductDirectory}';
    Location.Text := Folder;
  end;
end;

procedure InitializeWizard;
var
  Brand, Title, Detail, FolderLabel, Settings: TNewStaticText;
  Browse: TNewButton;
begin
  Landing := CreateCustomPage(wpWelcome, '{#ProductName}', '{#EditionName}  {#MyAppVersion}');
  Brand := TNewStaticText.Create(Landing);
  Brand.Parent := Landing.Surface;
  Brand.Caption := 'ZSDesk';
  Brand.Font.Name := 'Segoe UI';
  Brand.Font.Size := 28;
  Brand.Font.Style := [fsBold];
  Brand.Font.Color := $00C56818;
  Brand.SetBounds(ScaleX(12), ScaleY(4), ScaleX(360), ScaleY(55));
  Title := TNewStaticText.Create(Landing);
  Title.Parent := Landing.Surface;
  Title.Caption := CustomMessage('LandingTitle');
  Title.Font.Size := 12;
  Title.SetBounds(ScaleX(14), ScaleY(68), Landing.SurfaceWidth-ScaleX(28), ScaleY(28));
  Detail := TNewStaticText.Create(Landing);
  Detail.Parent := Landing.Surface;
  Detail.Caption := CustomMessage('LandingDetail');
  Detail.Font.Color := $00707070;
  Detail.SetBounds(ScaleX(14), ScaleY(102), Landing.SurfaceWidth-ScaleX(28), ScaleY(26));
  FolderLabel := TNewStaticText.Create(Landing);
  FolderLabel.Parent := Landing.Surface;
  FolderLabel.Caption := CustomMessage('InstallFolder');
  FolderLabel.SetBounds(ScaleX(14), ScaleY(149), ScaleX(200), ScaleY(20));
  Location := TNewEdit.Create(Landing);
  Location.Parent := Landing.Surface;
  Location.Text := WizardDirValue;
  Location.SetBounds(ScaleX(14), ScaleY(173), Landing.SurfaceWidth-ScaleX(112), ScaleY(24));
  Browse := TNewButton.Create(Landing);
  Browse.Parent := Landing.Surface;
  Browse.Caption := CustomMessage('BrowseFolder');
  Browse.SetBounds(Landing.SurfaceWidth-ScaleX(88), ScaleY(172), ScaleX(74), ScaleY(26));
  Browse.OnClick := @ChooseFolder;
  Shortcut := TNewCheckBox.Create(Landing);
  Shortcut.Parent := Landing.Surface;
  Shortcut.Caption := ExpandConstant('{cm:CreateDesktopIcon}');
  Shortcut.Checked := WizardIsTaskSelected('desktopicon');
  Shortcut.SetBounds(ScaleX(14), ScaleY(208), Landing.SurfaceWidth-ScaleX(28), ScaleY(24));
  Settings := TNewStaticText.Create(Landing);
  Settings.Parent := Landing.Surface;
  Settings.Caption := CustomMessage('KeepSettings');
  Settings.Font.Color := $00707070;
  Settings.WordWrap := True;
  Settings.SetBounds(ScaleX(14), ScaleY(243), Landing.SurfaceWidth-ScaleX(28), ScaleY(40));
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  Folder: String;
begin
  Result := True;
  if CurPageID = Landing.ID then
  begin
    Folder := Trim(Location.Text);
    if (Length(Folder) < 3) or ((Pos(':\', Folder) <> 2) and (Pos('\\', Folder) <> 1)) then
    begin
      MsgBox(CustomMessage('InvalidFolder'), mbError, MB_OK);
      Result := False;
      Exit;
    end;
    WizardForm.DirEdit.Text := Folder;
    if Shortcut.Checked then WizardSelectTasks('desktopicon') else WizardSelectTasks('!desktopicon');
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = Landing.ID then
  begin
    Location.Text := WizardDirValue;
    WizardForm.NextButton.Caption := CustomMessage('InstallNow');
  end;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := PageID = wpSelectTasks;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ExitCode: Integer;
begin
  if (CurUninstallStep = usUninstall) and FileExists(ExpandConstant('{app}\ZSDesk.exe')) then
  begin
    if not Exec(ExpandConstant('{app}\ZSDesk.exe'), '--uninstall-integration', ExpandConstant('{app}'), SW_HIDE, ewWaitUntilTerminated, ExitCode) then
      RaiseException(CustomMessage('RestoreFailed'));
    if ExitCode <> 0 then RaiseException(CustomMessage('RestoreFailed'));
  end;
end;
