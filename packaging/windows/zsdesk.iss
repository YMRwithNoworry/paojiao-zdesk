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
LZMADictionarySize=16384
LZMANumFastBytes=273
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
VersionInfoVersion={#MyAppVersion}

[Languages]
Name: "chinesesimp"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "setup-logo.bmp"; Flags: dontcopy
Source: "setup-logo-2x.bmp"; Flags: dontcopy
Source: "setup-button.bmp"; Flags: dontcopy
Source: "setup-button-2x.bmp"; Flags: dontcopy
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
chinesesimp.Options=选择安装位置
english.Options=Choose install location
chinesesimp.HideOptions=收起安装位置
english.HideOptions=Hide install location
chinesesimp.InProgress=正在安装…
english.InProgress=Installing…
chinesesimp.Finishing=正在完成配置…
english.Finishing=Finishing setup…
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
chinesesimp.MainLogin=登录 Windows 时打开文件管理主窗口
english.MainLogin=Open the file manager when I sign in to Windows
chinesesimp.FencesLogin=登录 Windows 时启动桌面格子
english.FencesLogin=Start desktop fences when I sign in to Windows
chinesesimp.LoginUnavailable=无法读取自启设置，将保留现有设置。
english.LoginUnavailable=Sign-in settings could not be read. Existing settings will be kept.
chinesesimp.LoginFailed=未能保存登录启动设置。请在 ZSDesk 中检查开机启动选项。
english.LoginFailed=Could not save the sign-in settings. Check the startup options in ZSDesk.
chinesesimp.CloseFailed=ZSDesk 仍有窗口或文件操作未结束，请完成文件操作并关闭相关窗口后重试。
english.CloseFailed=ZSDesk still has an open window or file operation. Complete the operation and close the window, then retry.
chinesesimp.RestoreFailed=当前安装的系统集成还原失败，卸载已停止；请在 ZSDesk 中恢复系统集成后重试。
english.RestoreFailed=System integration could not be restored. Uninstall stopped; restore integration in ZSDesk and try again.

[Code]
const
  SurfaceWidth = 520;
  SurfaceHeight = 440;
  MutedColor = $00706964;

var
  Landing: TWizardPage;
  Location: TNewEdit;
  Shortcut: TNewCheckBox;
  Surface: TPanel;
  Logo: TBitmapImage;
  Brand, EditionLabel, Status: TNewStaticText;
  Primary, Other: TBitmapButton;
  Browse: TNewButton;
  Progress: TNewProgressBar;
  ArtSuffix, UIFont: String;
  ArtScale: Integer;
  Expanded, Loaded: Boolean;
  MainLogin, FencesLogin: TNewCheckBox;
  LoginNotice: TNewStaticText;
  LoginFolder: String;
  OriginalLogin: Integer;
  HelperReady: Boolean;

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

procedure ExtractUpdateHelper;
begin
  if HelperReady then Exit;
  ExtractTemporaryFile('ZSDesk-update-helper.exe');
  ExtractTemporaryFile('msvcp140.dll');
  ExtractTemporaryFile('vcruntime140.dll');
  ExtractTemporaryFile('vcruntime140_1.dll');
  HelperReady := True;
end;

procedure LoadLoginChoices(Sender: TObject);
var
  Folder, QuotedFolder: String;
  Bits: Integer;
begin
  if WizardSilent then Exit;
  Folder := Trim(Location.Text);
  if (Folder = '') or (Pos('"', Folder) <> 0) then Exit;
  Folder := RemoveBackslashUnlessRoot(ExpandFileName(Folder));
  if CompareText(Folder, LoginFolder) = 0 then Exit;
  LoginFolder := Folder;
  Bits := 16;
  if FileExists(AddBackslash(Folder) + 'ZSDesk.exe') then begin
    ExtractUpdateHelper;
    QuotedFolder := Folder;
    if Copy(Folder, Length(Folder), 1) = '\' then QuotedFolder := Folder + '\';
    if not Exec(ExpandConstant('{tmp}\ZSDesk-update-helper.exe'),
      '--installer-startup-status "' + QuotedFolder + '"', ExpandConstant('{tmp}'),
      SW_HIDE, ewWaitUntilTerminated, Bits) then Bits := -1;
  end;
  if (Bits < 16) or (Bits > 19) then OriginalLogin := -1
  else OriginalLogin := Bits - 16;
  if MainLogin <> nil then begin
    MainLogin.Enabled := OriginalLogin >= 0;
    MainLogin.Checked := MainLogin.Enabled and ((OriginalLogin and 1) <> 0);
  end;
  if FencesLogin <> nil then begin
    FencesLogin.Enabled := OriginalLogin >= 0;
    FencesLogin.Checked := FencesLogin.Enabled and ((OriginalLogin and 2) <> 0);
  end;
  LoginNotice.Visible := Expanded and (OriginalLogin < 0);
end;

function LoginChoice(Box: TNewCheckBox; Bit: Integer): String;
begin
  Result := 'keep';
  if (Box = nil) or (OriginalLogin < 0) then Exit;
  if Box.Checked = ((OriginalLogin and Bit) <> 0) then Exit;
  if Box.Checked then Result := 'on' else Result := 'off';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Choices: String;
  ExitCode: Integer;
begin
  if (CurStep <> ssPostInstall) or WizardSilent or (LoginFolder = '') then Exit;
  Choices := LoginChoice(MainLogin, 1) + ' ' + LoginChoice(FencesLogin, 2);
  if Choices = 'keep keep' then Exit;
  if not Exec(ExpandConstant('{app}\ZSDesk.exe'), '--installer-startup-apply ' + Choices,
    ExpandConstant('{app}'), SW_HIDE, ewWaitUntilTerminated, ExitCode) or (ExitCode <> 0) then
    RaiseException(CustomMessage('LoginFailed'));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ExitCode: Integer;
begin
  Result := '';
  if FileExists(ExpandConstant('{app}\ZSDesk.exe')) then
  begin
    ExtractUpdateHelper;
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
    LoadLoginChoices(nil);
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
  Result.Font.Name := UIFont;
  Result.Font.Size := Size;
  Result.Font.Color := Color;
end;

function ActionOnSurface(Click: TNotifyEvent): TBitmapButton;
begin
  Result := TBitmapButton.Create(Surface);
  Result.Parent := Surface;
  Result.Stretch := True;
  Result.BackColor := clWhite;
  Result.Cursor := crHand;
  Result.OnClick := Click;
end;

procedure SetAction(Button: TBitmapButton; const Caption: String; IsPrimary: Boolean);
var
  B: TBitmap;
  Bounds: TRect;
  X, Y: Integer;
begin
  if Button.Caption = Caption then Exit;
  Button.Caption := Caption;
  B := Button.Bitmap;
  if IsPrimary then begin
    B.LoadFromFile(ExpandConstant('{tmp}\setup-button') + ArtSuffix + '.bmp');
    B.Canvas.Font.Color := clWhite;
    B.Canvas.Font.Height := -14 * ArtScale;
  end else begin
    B.Width := 220 * ArtScale;
    B.Height := 24 * ArtScale;
    B.Canvas.Brush.Style := bsSolid;
    B.Canvas.Brush.Color := clWhite;
    Bounds.Left := 0;
    Bounds.Top := 0;
    Bounds.Right := B.Width;
    Bounds.Bottom := B.Height;
    B.Canvas.FillRect(Bounds);
    B.Canvas.Font.Color := MutedColor;
    B.Canvas.Font.Height := -13 * ArtScale;
  end;
  B.Canvas.Font.Name := UIFont;
  B.Canvas.Brush.Style := bsClear;
  X := (B.Width - B.Canvas.TextWidth(Caption)) div 2;
  Y := (B.Height - B.Canvas.TextHeight(Caption)) div 2;
  B.Canvas.TextOut(X, Y, Caption);
end;

{ The bitmap button reserves two physical pixels on each side for keyboard focus. }
procedure PlaceAction(Button: TBitmapButton; Top, Height: Integer);
begin
  Button.SetBounds(ScaleX((SurfaceWidth - 220) div 2) - 2, ScaleY(Top) - 2,
    ScaleX(220) + 4, ScaleY(Height) + 4);
end;

procedure LayoutSurface(PageID: Integer);
var
  OnLanding, OptionsVisible: Boolean;
  Row: Integer;
begin
  OnLanding := PageID = Landing.ID;
  OptionsVisible := OnLanding and Expanded;
  if OptionsVisible then begin
    Place(Logo, 228, 14, 64, 64);
    Place(Brand, 20, 82, 480, 41);
    Place(EditionLabel, 24, 124, 472, 21);
    PlaceAction(Other, 151, 24);
  end else begin
    Place(Logo, 204, 60, 112, 112);
    Place(Brand, 20, 194, 480, 41);
    Place(EditionLabel, 24, 235, 472, 21);
    if OnLanding then PlaceAction(Other, 270, 24) else PlaceAction(Other, 402, 24);
  end;
  Place(Location, 70, 185, 290, 26);
  Place(Browse, 368, 184, 82, 28);
  Place(Shortcut, 70, 221, 380, 23);
  Row := 248;
  if MainLogin <> nil then begin
    Place(MainLogin, 70, Row, 380, 23);
    MainLogin.Visible := OptionsVisible;
    Row := Row + 27;
  end;
  if FencesLogin <> nil then begin
    Place(FencesLogin, 70, Row, 380, 23);
    FencesLogin.Visible := OptionsVisible;
    Row := Row + 27;
  end;
  Place(LoginNotice, 30, Row, 460, 36);
  LoginNotice.Visible := OptionsVisible and (OriginalLogin < 0);
  PlaceAction(Primary, 348, 44);
  Place(Status, 30, 276, 460, 26);
  Place(Progress, 110, 308, 300, 6);
  Location.Visible := OptionsVisible;
  Browse.Visible := OptionsVisible;
  Shortcut.Visible := OptionsVisible;
  Status.Visible := not OnLanding;
  Progress.Visible := PageID = wpInstalling;
  Primary.Visible := OnLanding or (PageID = wpFinished);
  Other.Visible := Primary.Visible;
  if OnLanding then begin
    SetAction(Primary, CustomMessage('InstallNow'), True);
    if Expanded then SetAction(Other, CustomMessage('HideOptions'), False)
    else SetAction(Other, CustomMessage('Options'), False);
  end else if PageID = wpFinished then begin
    Status.Caption := CustomMessage('Complete');
    SetAction(Primary, CustomMessage('Launch'), True);
    SetAction(Other, CustomMessage('Done'), False);
  end else
    Status.Caption := CustomMessage('InProgress');
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
    if Expanded then LoadLoginChoices(nil);
    LayoutSurface(Landing.ID);
    if Expanded then WizardForm.ActiveControl := Location;
  end;
end;

function LoginCheckBox(const MessageName: String): TNewCheckBox;
begin
  Result := TNewCheckBox.Create(Surface);
  Result.Parent := Surface;
  Result.Font.Name := UIFont;
  Result.Caption := CustomMessage(MessageName);
end;

procedure InitializeWizard;
begin
  Landing := CreateCustomPage(wpWelcome, '{#ProductName}', '');
  WizardForm.Caption := 'ZSDesk · ' + CustomMessage('Edition');
  WizardForm.ClientWidth := ScaleX(SurfaceWidth);
  WizardForm.ClientHeight := ScaleY(SurfaceHeight);
  WizardForm.Position := poScreenCenter;
  WizardForm.BorderIcons := [biSystemMenu, biMinimize];
  if ActiveLanguage = 'chinesesimp' then UIFont := 'Microsoft YaHei UI'
  else UIFont := 'Segoe UI';
  ArtScale := 1;
  if ScaleX(100) > 100 then begin
    ArtScale := 2;
    ArtSuffix := '-2x';
  end;
  ExtractTemporaryFile('setup-logo' + ArtSuffix + '.bmp');
  ExtractTemporaryFile('setup-button' + ArtSuffix + '.bmp');
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
  Logo.Bitmap.LoadFromFile(ExpandConstant('{tmp}\setup-logo') + ArtSuffix + '.bmp');
  Brand := LabelOnSurface(24, $00111111);
  Brand.Font.Name := 'Segoe UI';
  Brand.Font.Style := [fsBold];
  Brand.Caption := 'ZSDesk';
  EditionLabel := LabelOnSurface(9, MutedColor);
  EditionLabel.Caption := CustomMessage('Edition') + '  ·  {#MyAppVersion}';
  Status := LabelOnSurface(10, $00262626);
  Other := ActionOnSurface(@OtherClick);
  Location := TNewEdit.Create(Surface);
  Location.Parent := Surface;
  Location.Font.Name := UIFont;
  Location.OnExit := @LoadLoginChoices;
  Browse := TNewButton.Create(Surface);
  Browse.Parent := Surface;
  Browse.OnClick := @ChooseFolder;
  Browse.Caption := CustomMessage('BrowseFolder');
  Browse.Font.Name := UIFont;
  Shortcut := TNewCheckBox.Create(Surface);
  Shortcut.Parent := Surface;
  Shortcut.Font.Name := UIFont;
  Shortcut.Caption := ExpandConstant('{cm:CreateDesktopIcon}');
#if Edition != "desktop-box"
  MainLogin := LoginCheckBox('MainLogin');
#endif
#if Int(HasDesktop) != 0
  FencesLogin := LoginCheckBox('FencesLogin');
#endif
  LoginNotice := LabelOnSurface(9, MutedColor);
  LoginNotice.Caption := CustomMessage('LoginUnavailable');
  LoginNotice.WordWrap := True;
  Primary := ActionOnSurface(@PrimaryClick);
  Progress := TNewProgressBar.Create(Surface);
  Progress.Parent := Surface;
  Progress.Max := 100;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  Folder: String;
begin
  Result := True;
  if WizardSilent then Exit;
  if CurPageID = Landing.ID then
  begin
    Folder := Trim(Location.Text);
    if (Length(Folder) < 3) or (Pos('"', Folder) <> 0) or ((Pos(':\', Folder) <> 2) and (Pos('\\', Folder) <> 1)) then
    begin
      Expanded := True;
      LayoutSurface(Landing.ID);
      WizardForm.ActiveControl := Location;
      MsgBox(CustomMessage('InvalidFolder'), mbError, MB_OK);
      Result := False;
      Exit;
    end;
    WizardForm.DirEdit.Text := Folder;
    if Expanded or (LoginFolder <> '') then LoadLoginChoices(nil);
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
  if Progress.Position >= 100 then
    Status.Caption := CustomMessage('Finishing')
  else
    Status.Caption := CustomMessage('InProgress') + ' ' + IntToStr(Progress.Position) + '%';
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
