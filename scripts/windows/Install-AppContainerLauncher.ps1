# The rc.7 launcher is distributed in the release archive, not crates.io.
[CmdletBinding()]
param(
    [string]$Destination = (Join-Path $HOME '.cargo/bin'),
    [string]$Archive
)
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot/Get-PinnedArtifact.ps1"
$release = 'agent-bridle-mcp-v0.8.0-rc.7-windows-x86_64'
$sha256 = '6ca23258e1bf8c3eb7881d0836c8186039c176e6f472cfe9d948acf313c4816d'
$stage = Join-Path ([IO.Path]::GetTempPath()) ('aclaunch-' + [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Path $stage | Out-Null
    $download = Join-Path $stage "$release.tar.gz"
    Get-PinnedArtifact -Uri "https://github.com/Gilamonster-Foundation/agent-bridle/releases/download/v0.8.0-rc.7/$release.tar.gz" -Destination $download -Sha256 $sha256 -Archive $Archive
    tar.exe -xzf $download -C $stage "$release/agent-bridle-aclaunch.exe"
    if ($LASTEXITCODE -ne 0) { throw 'Verified launcher archive extraction failed.' }
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    Copy-Item -LiteralPath (Join-Path $stage "$release/agent-bridle-aclaunch.exe") -Destination $Destination
    Write-Output 'Installed verified agent-bridle-aclaunch v0.8.0-rc.7'
} finally {
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
}
