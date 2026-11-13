[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$ZSDeskPath,[Parameter(Mandatory=$true)][string]$WorkDir)
$ErrorActionPreference='Stop'
if(-not [IO.Path]::IsPathRooted($WorkDir)){throw 'WorkDir must be absolute'}
$WorkDir=[IO.Path]::GetFullPath($WorkDir)
if(Test-Path -LiteralPath $WorkDir){
    if(@(Get-ChildItem -LiteralPath $WorkDir).Count -gt 0){throw 'WorkDir must be an empty test directory'}
}else{[void][IO.Directory]::CreateDirectory($WorkDir)}
$prepare=Join-Path (Split-Path -Parent $PSScriptRoot) 'Prepare-Listary7.ps1'
$id='34620ed5-029f-4d20-9264-d281b7ecda10'
$fixture=[ordered]@{
    Settings=[ordered]@{'General.Language'='zh-CN';'OpenFolder.Path'='C:\Previous\Manager.exe';Future=@{Color='蓝色';Values=@(1,2,3)}}
    Actions=@{Items=@{Deletions=@('unrelated-deleted',$id);Moves=@(@{Id='unrelated';Index=0});Insertions=@(@{Index=-1;Info=$null;Item=@{Id='unrelated';Title='原有动作';Path='C:\Windows\notepad.exe';Parameter='"{action_path}"';'$customType'='CustomCommand'}});Updates=@(@{Id=$id;UpdatedProperties=@{Hotkey=1234;Path='C:\Previous\Manager.exe'}})}}
    Commands=@{Items=@{Deletions=@();Moves=@();Insertions=@();Updates=@()}}
    UnknownFuture=@{Text='中文与空格';Null=$null;Flag=$true;List=@('a','b')}
}
$fixturePath=Join-Path $WorkDir 'original.json';$first=Join-Path $WorkDir 'merged.json';$second=Join-Path $WorkDir 'merged-again.json'
[IO.File]::WriteAllText($fixturePath,($fixture|ConvertTo-Json -Depth 20),(New-Object Text.UTF8Encoding($false)))
$before=(Get-FileHash -LiteralPath $fixturePath).Hash
$result=& $prepare -ZSDeskPath $ZSDeskPath -PreferencesPath $fixturePath -OutputPath $first
$repeat=& $prepare -ZSDeskPath $ZSDeskPath -PreferencesPath $first -OutputPath $second
if((Get-FileHash -LiteralPath $fixturePath).Hash -ne $before){throw 'Original preferences changed'}
if((Get-FileHash -LiteralPath $first).Hash -ne (Get-FileHash -LiteralPath $second).Hash){throw 'Repeated preparation is not idempotent'}
$merged=Get-Content -LiteralPath $first -Raw -Encoding UTF8|ConvertFrom-Json
if($merged.UnknownFuture.Text -ne '中文与空格' -or $merged.Settings.Future.Color -ne '蓝色' -or $merged.UnknownFuture.List.Count -ne 2){throw 'Unrelated future fields were lost'}
if($merged.Actions.Items.Insertions.Count -ne 2 -or $merged.Actions.Items.Insertions[0].Item.Id -ne 'unrelated'){throw 'Existing actions were changed'}
if($merged.Actions.Items.Updates[0].UpdatedProperties.Hotkey -ne 1234){throw 'Existing action hotkey was lost'}
if($merged.Actions.Items.Deletions.Count -ne 1 -or $merged.Actions.Items.Deletions[0] -ne 'unrelated-deleted'){throw 'Deletion list merge failed'}
if($merged.Settings.'OpenFolder.Param' -ne '"%1\."'){throw 'Folder argument template differs'}
if($merged.Actions.Items.Insertions[1].Item.Parameter -ne '--select "{action_path}"'){throw 'Selection argument template differs'}
if($result.QuickSwitch -or $repeat.QuickSwitch){throw 'One-way adapter must not advertise Quick Switch'}
$refused=$false
try{& $prepare -ZSDeskPath $ZSDeskPath -PreferencesPath $fixturePath -OutputPath $fixturePath|Out-Null}catch{$refused=$true}
if(-not $refused){throw 'Preparing over the live preferences file was not refused'}
$refused=$false
try{& $prepare -ZSDeskPath $ZSDeskPath -PreferencesPath $fixturePath -OutputPath $first|Out-Null}catch{$refused=$true}
if(-not $refused){throw 'Existing output was overwritten'}
$report=[ordered]@{state='passed';original_unchanged=$true;idempotent=$true;unknown_settings_preserved=$true;existing_actions_preserved=$true;existing_hotkey_preserved=$true;overwrite_refused=$true;folder_and_selection_templates=$true;quick_switch_claimed=$false}
$report|ConvertTo-Json|Set-Content -LiteralPath (Join-Path $WorkDir 'verification.current.json') -Encoding UTF8
$report|ConvertTo-Json -Compress
