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
Compression=lzma2/ultra64
MergeDuplicateFiles=yes
SolidCompression=yes
WizardStyle=modern
WizardSizePercent=100,100
WizardResizable=no
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.18362
SetupIconFile=..\..\crates\zsdesk-app\resources\zsdesk.ico
UninstallDisplayIcon={app}\ZSDesk.exe
CloseApplications=yes
CloseApplicationsFilter=ZSDesk.exe
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
Source: "setup-logo.bmp"; Flags: dontcopy
Source: "{#SourceDir}\ZSDesk.exe"; DestName: "ZSDesk-update-helper.exe"; Flags: dontcopy
Source: "{#SourceDir}\msvcp140.dll"; Flags: dontcopy
Source: "{#SourceDir}\vcruntime140.dll"; Flags: dontcopy
Source: "{#SourceDir}\vcruntime140_1.dll"; Flags: dontcopy
Source: "{#SourceDir}\*"; DestDir: "{app}"; Excludes: "ZSDeskWindowHook-*.dll,\engines\everything\Everything.exe"; Flags: ignoreversion recursesubdirs createallsubdirs
#if FileExists(SourceDir + "\engines\everything\Everything.exe")
#define EngineHash GetSHA256OfFile(SourceDir + "\engines\everything\Everything.exe")
Source: "{#SourceDir}\engines\everything\Everything.exe"; DestDir: "{app}\engines\everything"; Flags: ignoreversion; Check: NeedsSearchEngine
#endif
Source: "{#SourceDir}\ZSDeskWindowHook-*.dll"; DestDir: "{app}"; Flags: onlyifdoesntexist

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
chinesesimp.Options=安装选项
english.Options=Installation options
chinesesimp.HideOptions=收起安装选项
english.HideOptions=Hide options
chinesesimp.InProgress=正在安装
english.InProgress=Installing
chinesesimp.Complete=安装完成
english.Complete=Installation complete
chinesesimp.Launch=启动 ZSDesk
english.Launch=Launch ZSDesk
chinesesimp.Done=完成
english.Done=Finish
chinesesimp.Edition={#EditionName}
#if Edition == "desktop-box"
english.Edition=Desktop Box
#elif Edition == "themed"
english.Edition=Theme edition
#elif Edition == "desktop"
english.Edition=Desktop edition
#elif Edition == "network"
english.Edition=Network edition
#elif Edition == "standard"
english.Edition=Standard edition
#else
english.Edition=Full edition
#endif
chinesesimp.InstallFolder=安装位置
english.InstallFolder=Install location
chinesesimp.BrowseFolder=浏览…
english.BrowseFolder=Browse…
chinesesimp.InstallNow=立即安装
english.InstallNow=Install now
chinesesimp.InvalidFolder=请输入完整的安装路径。
english.InvalidFolder=Enter a full installation path.
chinesesimp.CloseFailed=ZSDesk 仍有窗口或文件操作未结束，请完成文件操作并关闭相关窗口后重试。
english.CloseFailed=ZSDesk still has an open window or file operation. Complete the operation and close the window, then retry.
chinesesimp.RestoreFailed=当前安装的系统集成还原失败，卸载已停止；请在 ZSDesk 中恢复系统集成后重试。
english.RestoreFailed=System integration could not be restored. Uninstall stopped; restore integration in ZSDesk and try again.

[Code]
var
  Landing: TWizardPage;
  Location: TNewEdit;
  Shortcut: TNewCheckBox;
  Surface: TPanel;
  Logo: TBitmapImage;
  Brand, Detail, FolderLabel: TNewStaticText;
  Primary, Other, Browse: TNewButton;
  Progress: TNewProgressBar;
  Expanded, Loaded: Boolean;

#ifdef EngineHash
function NeedsSearchEngine: Boolean;
var
  Target: String;
begin
  Target := ExpandConstant('{app}\engines\everything\Everything.exe');
  Result := True;
  if FileExists(Target) then begin
    Result := CompareText(GetSHA256OfFile(Target), '{#EngineHash}') <> 0;
    if not Result then Log('Keeping the identical Everything service executable.');
  end;
end;
#endif

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ExitCode: Integer;
begin
  Result := '';
  if FileExists(ExpandConstant('{app}\ZSDesk.exe')) then
  begin
    ExtractTemporaryFile('ZSDesk-update-helper.exe');
    ExtractTemporaryFile('msvcp140.dll');
    ExtractTemporaryFile('vcruntime140.dll');
    ExtractTemporaryFile('vcruntime140_1.dll');
    if not Exec(ExpandConstant('{tmp}\ZSDesk-update-helper.exe'), '--shutdown-for-update "' + ExpandConstant('{app}\ZSDesk.exe') + '"', ExpandConstant('{tmp}'), SW_HIDE, ewWaitUntilTerminated, ExitCode) then
      Result := CustomMessage('CloseFailed')
    else if ExitCode <> 0 then Result := CustomMessage('CloseFailed');
  end;
end;

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

procedure Place(Control: TControl; X, Y, W, H: Integer);
begin
  Control.SetBounds(ScaleX(X), ScaleY(Y), ScaleX(W), ScaleY(H));
end;

function LabelOnSurface(Size: Integer; Color: Integer): TNewStaticText;
begin
  Result := TNewStaticText.Create(Surface);
  Result.Parent := Surface;
  Result.AutoSize := False;
  Result.Alignment := taCenter;
  Result.Font.Size := Size;
  Result.Font.Color := Color;
end;

function ButtonOnSurface(Click: TNotifyEvent): TNewButton;
begin
  Result := TNewButton.Create(Surface);
  Result.Parent := Surface;
  Result.OnClick := Click;
end;

procedure LayoutSurface(PageID: Integer);
var
  OnLanding, OptionsVisible: Boolean;
  Y: Integer;
begin
  OnLanding := PageID = Landing.ID;
  OptionsVisible := OnLanding and Expanded;
  Y := 64;
  if OptionsVisible then Y := 20;
  Place(Logo, 226, Y, 68, 68);
  Place(Brand, 20, Y+78, 480, 46);
  Place(Detail, 20, Y+130, 480, 26);
  Place(FolderLabel, 44, 194, 410, 20);
  Place(Location, 44, 218, 346, 26);
  Place(Browse, 398, 217, 78, 28);
  Place(Shortcut, 44, 254, 432, 24);
  Place(Primary, 151, 306, 218, 40);
  Place(Other, 170, 356, 180, 30);
  Place(Progress, 90, 268, 340, 12);
  FolderLabel.Visible := OptionsVisible;
  Location.Visible := OptionsVisible;
  Browse.Visible := OptionsVisible;
  Shortcut.Visible := OptionsVisible;
  Progress.Visible := PageID = wpInstalling;
  Primary.Visible := OnLanding or (PageID = wpFinished);
  Other.Visible := Primary.Visible;
  if OnLanding then begin
    Detail.Caption := CustomMessage('Edition') + '   ' + '{#MyAppVersion}';
    Primary.Caption := CustomMessage('InstallNow');
    if Expanded then Other.Caption := CustomMessage('HideOptions')
    else Other.Caption := CustomMessage('Options');
  end else if PageID = wpFinished then begin
    Detail.Caption := CustomMessage('Complete');
    Primary.Caption := CustomMessage('Launch');
    Other.Caption := CustomMessage('Done');
  end else
    Detail.Caption := CustomMessage('InProgress');
end;

procedure PrimaryClick(Sender: TObject);
begin
  if WizardForm.CurPageID = wpFinished then
    if WizardForm.RunList.Items.Count > 0 then WizardForm.RunList.Checked[0] := True;
  WizardForm.NextButton.OnClick(WizardForm.NextButton);
end;

procedure OtherClick(Sender: TObject);
begin
  if WizardForm.CurPageID = wpFinished then begin
    if WizardForm.RunList.Items.Count > 0 then WizardForm.RunList.Checked[0] := False;
    WizardForm.NextButton.OnClick(WizardForm.NextButton);
  end else begin
    Expanded := not Expanded;
    LayoutSurface(Landing.ID);
    if Expanded then WizardForm.ActiveControl := Location;
  end;
end;

procedure InitializeWizard;
begin
  Landing := CreateCustomPage(wpWelcome, '{#ProductName}', '');
  WizardForm.ClientWidth := ScaleX(520);
  WizardForm.ClientHeight := ScaleY(410);
  Surface := TPanel.Create(WizardForm);
  Surface.Parent := WizardForm;
  Surface.SetBounds(0,0,WizardForm.ClientWidth,WizardForm.ClientHeight);
  Surface.Anchors := [akLeft,akRight,akTop,akBottom];
  Surface.BevelOuter := bvNone;
  Surface.ParentBackground := False;
  Surface.Color := clWhite;
  Surface.Visible := False;
  Logo := TBitmapImage.Create(Surface);
  Logo.Parent := Surface;
  Logo.Stretch := True;
  ExtractTemporaryFile('setup-logo.bmp');
  Logo.Bitmap.LoadFromFile(ExpandConstant('{tmp}'+Chr(92)+'setup-logo.bmp'));
  Brand := LabelOnSurface(25,$0028241F);
  Brand.Font.Name := 'Segoe UI';
  Brand.Font.Style := [fsBold];
  Brand.Caption := 'ZSDesk';
  Detail := LabelOnSurface(11,$006C625C);
  FolderLabel := LabelOnSurface(9,$003D3630);
  FolderLabel.Alignment := taLeftJustify;
  FolderLabel.Caption := CustomMessage('InstallFolder');
  Location := TNewEdit.Create(Surface);
  Location.Parent := Surface;
  Browse := ButtonOnSurface(@ChooseFolder);
  Browse.Caption := CustomMessage('BrowseFolder');
  Shortcut := TNewCheckBox.Create(Surface);
  Shortcut.Parent := Surface;
  Shortcut.Caption := ExpandConstant('{cm:CreateDesktopIcon}');
  Primary := ButtonOnSurface(@PrimaryClick);
  Primary.Font.Style := [fsBold];
  Other := ButtonOnSurface(@OtherClick);
  Progress := TNewProgressBar.Create(Surface);
  Progress.Parent := Surface;
  Progress.Max := 100;
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
      Expanded := True;
      LayoutSurface(Landing.ID);
      WizardForm.ActiveControl := Location;
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
  Surface.Visible := (CurPageID = Landing.ID) or (CurPageID = wpInstalling) or (CurPageID = wpFinished);
  if not Surface.Visible then Exit;
  if (CurPageID = Landing.ID) and not Loaded then begin
    Location.Text := WizardDirValue;
    Shortcut.Checked := WizardIsTaskSelected('desktopicon');
    Loaded := True;
  end;
  Surface.BringToFront;
  LayoutSurface(CurPageID);
  if Primary.Visible and not WizardSilent then WizardForm.ActiveControl := Primary;
end;

procedure CurInstallProgressChanged(CurProgress, MaxProgress: Integer);
begin
  if MaxProgress <= 0 then Exit;
  Progress.Position := Round(100.0 * CurProgress / MaxProgress);
  Detail.Caption := CustomMessage('InProgress') + '   ' + IntToStr(Progress.Position) + '%';
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
