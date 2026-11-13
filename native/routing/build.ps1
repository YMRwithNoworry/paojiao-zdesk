param(
    [Parameter(Mandatory=$true)][string]$OutputDir,
    [string]$BuildDir,
    [string[]]$ForbiddenPath=@()
)
$ErrorActionPreference='Stop'
if(-not [IO.Path]::IsPathRooted($OutputDir)){throw 'OutputDir must be an absolute path'}
$outputRoot=[IO.Path]::GetFullPath($OutputDir)
if(-not $BuildDir){
    $hasher=[Security.Cryptography.SHA256]::Create()
    try {$identity=([BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($PSScriptRoot)))).Replace('-','').Substring(0,12)}finally{$hasher.Dispose()}
    $BuildDir=Join-Path ([IO.Path]::GetTempPath()) ('zsdesk-routing-'+$identity+'-msvc-x64')
}
if(-not [IO.Path]::IsPathRooted($BuildDir)){throw 'BuildDir must be an absolute path'}
$buildRoot=[IO.Path]::GetFullPath($BuildDir)
if($buildRoot.StartsWith($outputRoot+'\',[StringComparison]::OrdinalIgnoreCase) -or $buildRoot -eq $outputRoot){throw 'BuildDir must be outside OutputDir'}
$vswhere=Join-Path ([Environment]::GetFolderPath('ProgramFilesX86')) 'Microsoft Visual Studio\Installer\vswhere.exe'
if(-not (Test-Path -LiteralPath $vswhere)){throw 'Visual Studio C++ tools are required'}
$installation=& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if(-not $installation){throw 'An installed x64 MSVC toolchain is required'}
$toolVersion=[IO.File]::ReadAllText((Join-Path $installation 'VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt')).Trim()
$tools=Join-Path $installation ('VC\Tools\MSVC\'+$toolVersion)
$compilerDir=Join-Path $tools 'bin\Hostx64\x64'
$cl=Join-Path $compilerDir 'cl.exe'
$kits=(Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots').KitsRoot10
$sdk=(Get-ChildItem -LiteralPath (Join-Path $kits 'Include') -Directory | Where-Object {Test-Path -LiteralPath (Join-Path $_.FullName 'um\Windows.h')} | Sort-Object Name -Descending | Select-Object -First 1).Name
if(-not $sdk -or -not (Test-Path -LiteralPath $cl)){throw 'MSVC and Windows SDK are required'}
[IO.Directory]::CreateDirectory($outputRoot)|Out-Null
[IO.Directory]::CreateDirectory($buildRoot)|Out-Null
[IO.Directory]::CreateDirectory((Join-Path $buildRoot 'temp'))|Out-Null
$lock=[IO.File]::Open((Join-Path $buildRoot 'routing-build.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
$oldTemp=$env:TEMP;$oldTmp=$env:TMP;$oldPath=$env:PATH;$oldInclude=$env:INCLUDE;$oldLib=$env:LIB
try {
    $env:TEMP=Join-Path $buildRoot 'temp';$env:TMP=$env:TEMP
    $env:PATH=$compilerDir+';'+$oldPath
    $env:INCLUDE=(Join-Path $tools 'include')+';'+(Join-Path $kits ('Include\'+$sdk+'\ucrt'))+';'+(Join-Path $kits ('Include\'+$sdk+'\shared'))+';'+(Join-Path $kits ('Include\'+$sdk+'\um'))+';'+(Join-Path $kits ('Include\'+$sdk+'\winrt'))
    $env:LIB=(Join-Path $tools 'lib\x64')+';'+(Join-Path $kits ('Lib\'+$sdk+'\ucrt\x64'))+';'+(Join-Path $kits ('Lib\'+$sdk+'\um\x64'))
    $release=Join-Path $buildRoot 'Release';$controllerObj=Join-Path $buildRoot 'controller-obj';$adapterObj=Join-Path $buildRoot 'adapter-obj'
    foreach($path in @($release,$controllerObj,$adapterObj)){[IO.Directory]::CreateDirectory($path)|Out-Null}
    $common=@('/nologo','/std:c++17','/utf-8','/O2','/MT','/EHsc','/W4','/WX','/wd4505','/DUNICODE','/D_UNICODE')
    $rc=Join-Path $kits ('bin\'+$sdk+'\x64\rc.exe')
    $resource=Join-Path $controllerObj 'controller.res'
    & $rc /nologo /c65001 ('/fo'+$resource) (Join-Path $PSScriptRoot 'controller.rc')
    if($LASTEXITCODE -ne 0){throw 'Routing resource build failed'}
    & $cl @common (Join-Path $PSScriptRoot 'ShellRoutingControl.cpp') $resource ('/Fe'+(Join-Path $release 'ShellRoutingControl.exe')) ('/Fo'+$controllerObj.Replace('\','/')+'/') /link /SUBSYSTEM:WINDOWS /ENTRY:wmainCRTStartup '/PDBALTPATH:%_PDB%' user32.lib advapi32.lib shell32.lib ole32.lib bcrypt.lib shlwapi.lib version.lib
    if($LASTEXITCODE -ne 0){throw 'Routing controller build failed'}
    $minhook=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..\crates\zsdesk-windows\native\minhook'))
    $upstream=@((Join-Path $minhook 'src\buffer.c'),(Join-Path $minhook 'src\hook.c'),(Join-Path $minhook 'src\trampoline.c'),(Join-Path $minhook 'src\hde\hde64.c'))
    & $cl /nologo /utf-8 /O2 /MT /TC /W4 /WX /wd4201 /wd4310 /wd4244 /wd4701 /c ('/I'+(Join-Path $minhook 'include')) @upstream ('/Fo'+$adapterObj.Replace('\','/')+'/')
    if($LASTEXITCODE -ne 0){throw 'MinHook source build failed'}
    $objects=@('buffer.obj','hook.obj','trampoline.obj','hde64.obj')|ForEach-Object {Join-Path $adapterObj $_}
    & $cl @common /LD ('/I'+(Join-Path $minhook 'include')) (Join-Path $PSScriptRoot 'adapter\ZSDeskRevealRouter.cpp') @objects ('/Fe'+(Join-Path $release 'ZSDeskRevealRouter.dll')) ('/Fo'+$adapterObj.Replace('\','/')+'/') /link '/PDBALTPATH:%_PDB%' user32.lib shell32.lib ole32.lib psapi.lib advapi32.lib bcrypt.lib
    if($LASTEXITCODE -ne 0){throw 'Routing adapter build failed'}
    $files=@('ShellRoutingControl.exe','ZSDeskRevealRouter.dll')
    foreach($name in $files){Copy-Item -LiteralPath (Join-Path $buildRoot ('Release\'+$name)) -Destination (Join-Path $outputRoot $name)}
    $privateRoots=@($env:USERPROFILE,[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..')),$buildRoot,$outputRoot)+$ForbiddenPath
    foreach($name in $files){
        $bytes=[IO.File]::ReadAllBytes((Join-Path $outputRoot $name))
        $views=@([Text.Encoding]::UTF8.GetString($bytes),[Text.Encoding]::Unicode.GetString($bytes),[Text.Encoding]::Unicode.GetString($bytes,1,$bytes.Length-1))
        foreach($private in $privateRoots|Where-Object {$_}){foreach($variant in @($private,$private.Replace('\','/'))){foreach($view in $views){if($view.IndexOf($variant,[StringComparison]::OrdinalIgnoreCase) -ge 0){throw ('Private build path found in '+$name)}}}}
    }
    $license=Join-Path $PSScriptRoot '..\..\crates\zsdesk-windows\native\minhook\LICENSE.txt'
    Copy-Item -LiteralPath $license -Destination (Join-Path $outputRoot 'MinHook-LICENSE.txt')
    $manifest=[ordered]@{schema='ZSDesk.RoutingPackage.v1';adapter_abi=2;controller=[ordered]@{file=$files[0];sha256=(Get-FileHash -LiteralPath (Join-Path $outputRoot $files[0])).Hash.ToLowerInvariant()};adapter=[ordered]@{file=$files[1];sha256=(Get-FileHash -LiteralPath (Join-Path $outputRoot $files[1])).Hash.ToLowerInvariant()};licenses=@('MinHook-LICENSE.txt');profiles=@('native-shell','chromium','wechat','explicit-module');default_enabled=$false;default_startup=$false}
    [IO.File]::WriteAllText((Join-Path $outputRoot 'manifest.json'),($manifest|ConvertTo-Json -Depth 5)+"`n",[Text.UTF8Encoding]::new($false))
    $manifest|ConvertTo-Json -Depth 5
} finally {$env:TEMP=$oldTemp;$env:TMP=$oldTmp;$env:PATH=$oldPath;$env:INCLUDE=$oldInclude;$env:LIB=$oldLib;$lock.Dispose()}
