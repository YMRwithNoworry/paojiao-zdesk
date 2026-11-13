<#
.SYNOPSIS
Creates a merged Listary preferences copy for opening folders and locating files in ZSDesk.
.EXAMPLE
./Prepare-Listary7.ps1 -ZSDeskPath 'C:\Program Files\ZSDesk\ZSDesk.exe' -PreferencesPath "$env:APPDATA\Listary\UserProfile\Settings\Preferences.json" -OutputPath 'C:\Users\Public\Preferences.ZSDesk.json'
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$ZSDeskPath,
    [Parameter(Mandatory=$true)][string]$PreferencesPath,
    [Parameter(Mandatory=$true)][string]$OutputPath
)
$ErrorActionPreference='Stop'

function Absolute-Path([string]$Path) {
    if(-not [IO.Path]::IsPathRooted($Path)){throw 'All paths must be absolute'}
    [IO.Path]::GetFullPath($Path)
}
function Set-Field($Object,[string]$Name,$Value) {
    if($Object.PSObject.Properties.Name -ccontains $Name){$Object.$Name=$Value}
    else{$Object | Add-Member -MemberType NoteProperty -Name $Name -Value $Value}
}
function Object-Field($Parent,[string]$Name) {
    if($Parent.PSObject.Properties.Name -cnotcontains $Name){Set-Field $Parent $Name ([pscustomobject]@{})}
    $value=$Parent.$Name
    if($null -eq $value -or $value -isnot [pscustomobject]){throw "Invalid object at $Name"}
    return $value
}
function Array-Field($Parent,[string]$Name) {
    if($Parent.PSObject.Properties.Name -cnotcontains $Name){Set-Field $Parent $Name @()}
    if($null -eq $Parent.$Name -or $Parent.$Name -isnot [Array]){throw "Invalid array at $Name"}
}

$ZSDeskPath=Absolute-Path $ZSDeskPath
$PreferencesPath=Absolute-Path $PreferencesPath
$OutputPath=Absolute-Path $OutputPath
if($OutputPath.Equals($PreferencesPath,[StringComparison]::OrdinalIgnoreCase)){throw 'OutputPath must differ from the original preferences file'}
if(Test-Path -LiteralPath $OutputPath){throw 'OutputPath already exists; choose an unused output file'}
$parent=Split-Path -Parent $OutputPath
if(-not (Test-Path -LiteralPath $parent -PathType Container)){throw 'The output directory must already exist'}
$executable=Get-Item -LiteralPath $ZSDeskPath
if($executable.PSIsContainer -or $executable.Extension -ine '.exe' -or $executable.VersionInfo.ProductName -ine 'ZSDesk'){
    throw 'ZSDeskPath must identify a ZSDesk executable'
}
$preferences=Get-Item -LiteralPath $PreferencesPath
if($preferences.PSIsContainer -or $preferences.Length -gt 16MB){throw 'PreferencesPath must identify a JSON file smaller than 16 MiB'}
$originalHash=(Get-FileHash -LiteralPath $PreferencesPath -Algorithm SHA256).Hash
$document=[IO.File]::ReadAllText($PreferencesPath,[Text.Encoding]::UTF8) | ConvertFrom-Json
if($null -eq $document -or $document -isnot [pscustomobject]){throw 'Preferences JSON must contain an object'}
$settings=Object-Field $document 'Settings'
Set-Field $settings 'OpenFolder.Path' $ZSDeskPath
# A final path component prevents a drive root's trailing backslash from
# escaping the closing quote when Listary expands its raw parameter template.
Set-Field $settings 'OpenFolder.Param' '"%1\."'

$actions=Object-Field $document 'Actions'
$items=Object-Field $actions 'Items'
foreach($field in @('Deletions','Moves','Insertions','Updates')){Array-Field $items $field}
$id='34620ed5-029f-4d20-9264-d281b7ecda10'
$ownedRows=@($items.Insertions | Where-Object { $_.Item.Id -eq $id })
if($ownedRows.Count -gt 1){throw 'The ZSDesk action ID occurs more than once'}
if($ownedRows.Count -eq 1){
    $action=$ownedRows[0].Item
    if($action -isnot [pscustomobject] -or $action.'$customType' -ne 'CustomCommand'){throw 'The ZSDesk action ID is used by an incompatible action'}
}else{
    $action=[pscustomobject]@{Id=$id;'$customType'='CustomCommand'}
    Set-Field $items 'Insertions' @(@($items.Insertions)+@([pscustomobject]@{Index=-1;Info=$null;Item=$action}))
}
$values=[ordered]@{Title='在 ZSDesk 中定位文件';Path=$ZSDeskPath;Parameter='--select "{action_path}"';Keyword='zsdesk';Enabled=$true}
foreach($entry in $values.GetEnumerator()){Set-Field $action $entry.Key $entry.Value}
Set-Field $items 'Deletions' @($items.Deletions | Where-Object { $_ -ne $id })
# Existing per-action hotkeys and other future fields remain intact. Update
# only the properties owned by this adapter when an override row is present.
foreach($update in @($items.Updates | Where-Object { $_.Id -eq $id })){
    $properties=Object-Field $update 'UpdatedProperties'
    foreach($entry in $values.GetEnumerator()){Set-Field $properties $entry.Key $entry.Value}
}
$json=ConvertTo-Json -InputObject $document -Depth 100
if((Get-FileHash -LiteralPath $PreferencesPath -Algorithm SHA256).Hash -ne $originalHash){throw 'The original preferences changed while preparing the copy'}
$encoding=New-Object Text.UTF8Encoding($false)
$bytes=$encoding.GetBytes($json+[Environment]::NewLine)
$stream=[IO.File]::Open($OutputPath,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
try{$stream.Write($bytes,0,$bytes.Length);$stream.Flush()}finally{$stream.Dispose()}
$readback=[IO.File]::ReadAllText($OutputPath,[Text.Encoding]::UTF8) | ConvertFrom-Json
if($readback.Settings.'OpenFolder.Path' -ne $ZSDeskPath -or @($readback.Actions.Items.Insertions | Where-Object { $_.Item.Id -eq $id }).Count -ne 1){throw 'Output validation failed'}
[pscustomobject]@{OutputPath=$OutputPath;ZSDeskPath=$ZSDeskPath;FolderOpening=$true;SelectedFileAction=$true;QuickSwitch=$false;OriginalPreserved=$true}
