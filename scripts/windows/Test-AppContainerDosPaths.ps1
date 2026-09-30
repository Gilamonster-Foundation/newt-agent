#Requires -Version 7.0
<#
.SYNOPSIS
Probe DOS/NT path resolution inside an AppContainer with leaf-only read grants.
.DESCRIPTION
Report-only mode tolerates DOS-name lookup and metadata-only device-open failure. Both modes require a
readable workspace marker and ACCESS_DENIED for an existing ungranted sibling
and a GENERIC_READ open of the MountPointManager device. No IOCTL is issued.
No network or child-process authority is granted; only this probe is launched.
.PARAMETER LauncherPath
Literal path to the trusted agent-bridle-aclaunch executable.
.PARAMETER RequireDos
Also require GetFinalPathNameByHandleW with VOLUME_NAME_DOS and the metadata-only
MountPointManager device open to succeed.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$LauncherPath,
    [switch]$RequireDos
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
if (-not $IsWindows) { throw 'This diagnostic requires Windows.' }
$launcher = (Resolve-Path -LiteralPath $LauncherPath).ProviderPath
if (-not (Test-Path -LiteralPath $launcher -PathType Leaf)) { throw 'Launcher must be a file.' }
$tag = [Guid]::NewGuid().ToString('N')
$root = Join-Path ([IO.Path]::GetTempPath()) "newt-ac-dos-$tag"
$workspace = Join-Path $root 'workspace'
$outside = Join-Path $root 'outside'
$allowed = Join-Path $workspace 'marker.txt'
$denied = Join-Path $outside 'marker.txt'
$exe = Join-Path $root 'probe.exe'
$result = 1
try {
    [IO.Directory]::CreateDirectory($workspace) | Out-Null
    [IO.Directory]::CreateDirectory($outside) | Out-Null
    [IO.File]::WriteAllText($allowed, 'newt-appcontainer-workspace-marker')
    [IO.File]::WriteAllText($denied, 'newt-appcontainer-sibling-marker')
    if ([IO.File]::ReadAllText($allowed) -cne 'newt-appcontainer-workspace-marker' -or
        [IO.File]::ReadAllText($denied) -cne 'newt-appcontainer-sibling-marker') {
        throw 'Host marker controls failed.'
    }
    & rustc --edition=2021 (Join-Path $PSScriptRoot 'probe_appcontainer_dos_paths.rs') -o $exe
    if ($LASTEXITCODE -ne 0) { throw 'Rust probe compilation failed.' }
    # An exact-file RX ACE, with no object/container inheritance flags.
    & icacls.exe $exe /grant '*S-1-15-2-1:RX' /Q 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Probe executable ACL preparation failed.' }
    $mode = if ($RequireDos) { '--require-dos' } else { '--report-only' }
    Push-Location -LiteralPath $workspace
    try {
        & $launcher --name "newt-dos-probe-$tag" --no-child-process --fs-read $workspace --fs-read $exe $exe $mode $allowed $denied
        $result = $LASTEXITCODE
    } finally { Pop-Location }
} finally {
    try {
        if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
    } catch {
        Write-Warning 'Probe scratch cleanup failed.'
        if ($result -eq 0) { $result = 1 }
    }
}
if ($result -ne 0) { throw "AppContainer DOS-path probe failed with exit code $result." }
