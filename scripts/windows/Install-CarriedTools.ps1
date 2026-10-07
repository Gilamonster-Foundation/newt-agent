# Build with cargo build -p newt-carried-tools, then stage beside newt.exe.
# Foreign upstream SHA256 verification follows packaging/chocolatey/tools/chocolateyinstall.ps1.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinaryPath,
    [Parameter(Mandatory)][string]$NewtDirectory,
    # Offline packaging may supply the SAME pinned upstream archive.
    [string]$RipgrepArchive
)
$ErrorActionPreference = 'Stop'
$binary = (Resolve-Path -LiteralPath $BinaryPath).Path
$destination = (Resolve-Path -LiteralPath $NewtDirectory).Path
$tools = Join-Path $destination 'tools'
if (Test-Path -LiteralPath $tools) { throw 'tools already exists; stage into a fresh release directory.' }
# A temporary locator, not an artifact identity. Only verified bytes reach tools/.
$stage = Join-Path $destination ('.carried-' + [guid]::NewGuid().ToString('N'))
$version = '14.1.1'
$archiveName = "ripgrep-$version-x86_64-pc-windows-msvc.zip"
$sha256 = 'd0f534024c42afd6cb4d38907c25cd2b249b79bbe6cc1dbee8e3e37c2b6e25a1'
try {
    New-Item -ItemType Directory -Path $stage | Out-Null
    $archive = Join-Path $stage $archiveName
    if ($RipgrepArchive) { Copy-Item -LiteralPath $RipgrepArchive -Destination $archive }
    else {
        Invoke-WebRequest "https://github.com/BurntSushi/ripgrep/releases/download/$version/$archiveName" -OutFile $archive
    }
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $sha256) {
        throw 'Pinned ripgrep 14.1.1 archive SHA256 mismatch; refusing installation.'
    }
    Expand-Archive -LiteralPath $archive -DestinationPath (Join-Path $stage 'unpacked')
    $pack = Join-Path $stage 'tools'
    New-Item -ItemType Directory -Path $pack | Out-Null
    $multicall = Join-Path $pack 'newt-carried-tools.exe'
    Copy-Item -LiteralPath $binary -Destination $multicall
    foreach ($name in @('cat','head','tail','wc','ls','mkdir','rm','cp','mv','sort','uniq','tr','cut','echo','grep')) {
        $alias = Join-Path $pack "$name.exe"
        # Hardlinks save disk on NTFS. Copies preserve argv[0] on other filesystems.
        try { New-Item -ItemType HardLink -Path $alias -Target $multicall -ErrorAction Stop | Out-Null }
        catch { Copy-Item -LiteralPath $multicall -Destination $alias }
    }
    $upstream = Join-Path $stage "unpacked\ripgrep-$version-x86_64-pc-windows-msvc"
    Copy-Item -LiteralPath (Join-Path $upstream 'rg.exe') -Destination $pack
    foreach ($license in @('COPYING','LICENSE-MIT','UNLICENSE')) {
        Copy-Item -LiteralPath (Join-Path $upstream $license) -Destination (Join-Path $pack "ripgrep-$license")
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../../crates/newt-carried-tools/LICENSE-uutils') -Destination $pack
    Move-Item -LiteralPath $pack -Destination $tools
    Write-Output "Installed carried tools at $tools"
} finally {
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
}
