# Build with cargo build -p newt-carried-tools, then stage beside newt.exe.
# Foreign upstream SHA256 verification follows packaging/chocolatey/tools/chocolateyinstall.ps1.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinaryPath,
    [Parameter(Mandatory)][string]$NewtDirectory,
    # Offline packaging may supply the SAME pinned upstream archive.
    [string]$RipgrepArchive,
    # Upstream distributes BusyBox as an executable, not a compressed archive.
    [string]$BusyboxArchive
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
$busyboxVersion = 'FRP-6075-g169694ebd'
$busyboxName = "busybox-w64u-$busyboxVersion.exe"
$busyboxSha256 = '6e263d154d8548d1eb936f65d1d8312c80df31c45974e48d6335e4dcc0f4f34c'
try {
    New-Item -ItemType Directory -Path $stage | Out-Null
    $busybox = Join-Path $stage $busyboxName
    if ($BusyboxArchive) { Copy-Item -LiteralPath $BusyboxArchive -Destination $busybox }
    else { Invoke-WebRequest "https://frippery.org/files/busybox/$busyboxName" -OutFile $busybox }
    if ((Get-FileHash -LiteralPath $busybox -Algorithm SHA256).Hash.ToLowerInvariant() -ne $busyboxSha256) {
        throw "Pinned busybox-w32 $busyboxVersion SHA256 mismatch; refusing installation."
    }
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
    foreach ($name in @('cat','head','tail','wc','ls','mkdir','rm','cp','mv','sort','uniq','tr','cut','echo','grep','which')) {
        $alias = Join-Path $pack "$name.exe"
        # Hardlinks save disk on NTFS. Copies preserve argv[0] on other filesystems.
        try { New-Item -ItemType HardLink -Path $alias -Target $multicall -ErrorAction Stop | Out-Null }
        catch { Copy-Item -LiteralPath $multicall -Destination $alias }
    }
    # Only these three applet names are exposed. The staging binary is removed
    # with $stage; no busybox.exe, shell, or --install-generated aliases ship.
    $sed = Join-Path $pack 'sed.exe'
    Copy-Item -LiteralPath $busybox -Destination $sed
    foreach ($name in @('awk','xargs')) {
        $alias = Join-Path $pack "$name.exe"
        try { New-Item -ItemType HardLink -Path $alias -Target $sed -ErrorAction Stop | Out-Null }
        catch { Copy-Item -LiteralPath $sed -Destination $alias }
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot '../../crates/newt-carried-tools/LICENSE-busybox') -Destination $pack
    "busybox-w32 $busyboxVersion (GPL-2.0-only)`nCorresponding source: https://frippery.org/files/busybox/busybox-w32-$busyboxVersion.tgz" |
        Set-Content -LiteralPath (Join-Path $pack 'busybox-SOURCE.txt') -Encoding ascii
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
