# Native grounding of crates/newt-carried-tools/src/tests.rs and the staged layout.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ToolsDirectory,
    [switch]$RequireRunnerTemp
)
$ErrorActionPreference = 'Stop'
$tools = (Resolve-Path -LiteralPath $ToolsDirectory).Path
# Regression for restored target/carried-package/tools: CI staging must not be cached.
if ($RequireRunnerTemp) {
    if (!$env:RUNNER_TEMP) { throw 'RUNNER_TEMP is required for uncached CI staging.' }
    $tempRoot = (Resolve-Path -LiteralPath $env:RUNNER_TEMP).Path.TrimEnd('\') + '\'
    if (!$tools.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'CI carried tools must be staged under RUNNER_TEMP, outside cached target/.'
    }
}
$fixture = Join-Path (Split-Path $tools) ('test space-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
$utf8 = New-Object System.Text.UTF8Encoding($false)
$script:checks = 0
$originalPath = $env:PATH
$originalPathext = $env:PATHEXT
# Resolve cargo before isolating PATH; exclude Git's Unix command directory.
$cargo = (Get-Command cargo.exe -ErrorAction Stop).Source
$env:PATH = "$tools;$(Split-Path $cargo);$env:SystemRoot\System32;$env:SystemRoot"
$env:PATHEXT = '.COM;.EXE;.BAT;.CMD'
function Check-Tool([string]$Name, [string[]]$Argv, [int]$Exit, $Expected, [string]$InputText = "") {
    # All fixture arguments are controlled and contain no quotes/backslashes at the end.
    $p = New-Object System.Diagnostics.Process
    $p.StartInfo.FileName = Join-Path $tools "$Name.exe"
    $p.StartInfo.Arguments = (($Argv | ForEach-Object { '"' + $_ + '"' }) -join ' ')
    $p.StartInfo.WorkingDirectory = $fixture
    $p.StartInfo.UseShellExecute = $false
    $p.StartInfo.RedirectStandardInput = $true
    $p.StartInfo.RedirectStandardOutput = $true
    $p.StartInfo.RedirectStandardError = $true
    $p.Start() | Out-Null
    $p.StandardInput.Write($InputText)
    $p.StandardInput.Close()
    $stdout = $p.StandardOutput.ReadToEndAsync()
    $stderr = $p.StandardError.ReadToEndAsync()
    if (!$p.WaitForExit(15000)) {
        & taskkill.exe /PID $p.Id /T /F | Out-Null
        throw "$Name timed out"
    }
    $out = $stdout.Result.Replace("`r`n", "`n")
    $err = $stderr.Result
    if ($p.ExitCode -ne $Exit) { throw "$Name $Argv exit $($p.ExitCode), expected ${Exit}: $err" }
    if ($null -ne $Expected -and $out -cne $Expected) { throw "$Name $Argv output <$out>, expected <$Expected>" }
    if ($Exit -eq 2 -and $err -notmatch 'supported:') { throw "missing refusal contract: $err" }
    $script:checks++
}
function Check-Brush([string]$Source, [int]$Exit, $Expected) {
    Check-Tool newt-ambient-brush @() $Exit $Expected (ConvertTo-Json -InputObject $Source -Compress)
}
try {
    [IO.File]::WriteAllText((Join-Path $fixture 'data.txt'), "alpha`nbeta`nalpha beta`n", $utf8)
    [IO.File]::WriteAllText((Join-Path $fixture 'repeat.txt'), "b`na`na`n", $utf8)
    Check-Tool cat @('repeat.txt') 0 "b`na`na`n"
    Check-Tool head @('-1','repeat.txt') 0 "b`n"
    Check-Tool tail @('-1','repeat.txt') 0 "a`n"
    Check-Tool wc @('-l','repeat.txt') 0 "3 repeat.txt`n"
    Check-Tool mkdir @('ops') 0 ''
    Check-Tool cp @('repeat.txt','ops/a') 0 ''
    Check-Tool mv @('ops/a','ops/b') 0 ''
    Check-Tool ls @('ops') 0 "b`n"
    Check-Tool rm @('ops/b') 0 ''
    Check-Tool rm @('-r','ops') 0 ''
    Check-Tool sort @('repeat.txt') 0 "a`na`nb`n"
    Check-Tool uniq @('repeat.txt') 0 "b`na`n"
    Check-Tool cut @('-c1','repeat.txt') 0 "b`na`na`n"
    Check-Tool echo @('carried','echo') 0 "carried echo`n"
    Check-Tool tr @('a-z','A-Z') 0 "B`nA`nA`n" "b`na`na`n"
    Check-Tool grep @('-n','alpha','data.txt') 0 "1:alpha`n3:alpha beta`n"
    # #2790: BRE interior anchors / leading stars are literals, not rg operators.
    Check-Tool grep @('a^b') 0 "a^b`n" "a^b`nab`n"
    Check-Tool grep @('a$b') 0 ('a$b' + "`n") ('a$b' + "`nab`n")
    Check-Tool grep @('*') 0 "*alpha`n" "*alpha`nalpha`n"
    Check-Tool grep @('^*') 0 "*alpha`n" "*alpha`nalpha*`n"
    Check-Tool grep @('^*alpha$') 0 "*alpha`n" "*alpha`n*alphabeta`n"
    Check-Tool grep @('^alpha$') 0 "alpha`n" "alpha`nalphabet`nxalpha`n"
    Check-Tool grep @('^a*$') 0 "aaa`n" "aaa`naba`n"
    Check-Tool grep @('\(*\)') 2 '' "*`n"
    Check-Tool grep @('a**') 2 '' "aaa`n"
    Check-Tool grep @('-E','alpha|beta','data.txt') 0 "alpha`nbeta`nalpha beta`n"
    Check-Tool grep @('-c','absent','data.txt') 1 "0`n"
    Check-Tool grep @('-c','alpha','data.txt') 0 "2`n"
    Check-Tool grep @('-v','alpha','data.txt') 0 "beta`n"
    Check-Tool grep @('-in','ALPHA','data.txt') 0 "1:alpha`n3:alpha beta`n"
    Check-Tool grep @('-F','alpha|beta','data.txt') 1 ''
    Check-Tool grep @('alpha|beta','data.txt') 1 ''
    Check-Tool grep @('-wo','alpha','data.txt') 0 "alpha`nalpha`n"
    Check-Tool grep @('-h','-ealpha','-e','beta','data.txt') 0 "alpha`nbeta`nalpha beta`n"
    Check-Tool grep @('-l','alpha','data.txt') 0 "data.txt`n"
    Check-Tool grep @('-H','alpha','data.txt') 0 "data.txt:alpha`ndata.txt:alpha beta`n"
    [IO.File]::WriteAllText((Join-Path $fixture 'counts.txt'), "aa aa`nnone`n", $utf8)
    Check-Tool grep @('-co','aa','counts.txt') 0 "1`n"
    Check-Tool grep @('-lc','aa','counts.txt') 0 "counts.txt`n"
    Check-Tool grep @('-cl','aa','counts.txt') 0 "counts.txt`n"
    Check-Tool grep @('-vo','aa','counts.txt') 0 ''
    Check-Tool grep @('-q','alpha','data.txt') 2 ''
    Check-Tool grep @('-e') 2 ''
    Check-Tool mkdir @('search') 0 ''
    Check-Tool cp @('data.txt','search/data.txt') 0 ''
    Check-Tool grep @('-rnh','alpha','search') 0 "1:alpha`n3:alpha beta`n"
    Check-Tool grep @('-Rh','alpha','search') 0 "alpha`nalpha beta`n"
    Check-Tool cp @('--','data.txt','-file') 0 ''
    Check-Tool grep @('-e','alpha','--','-file') 0 "alpha`nalpha beta`n"
    # #2795: native argv[0] dispatch and model forms must work through Brush.
    # No extra BusyBox applets (especially sh) may shadow the carried interpreter.
    foreach ($name in @('busybox','sh','bash')) {
        if (Test-Path (Join-Path $tools "$name.exe")) { throw "unexpected applet: $name" }
    }
    foreach ($notice in @('LICENSE-busybox','busybox-SOURCE.txt')) {
        if (!(Test-Path (Join-Path $tools $notice))) { throw "missing notice: $notice" }
    }
    Check-Brush "sed -n '2,3p' data.txt" 0 "beta`nalpha beta`n"
    Check-Brush "cp data.txt edit.txt; sed -i 's/alpha/changed/' edit.txt && cat edit.txt" 0 "changed`nbeta`nchanged beta`n"
    Check-Brush 'awk ''{print $1}'' data.txt' 0 "alpha`nbeta`nalpha`n"
    Check-Brush "printf 'alpha beta' | xargs -n1 echo.exe" 0 "alpha`nbeta`n"
    Check-Brush "printf 'alpha beta\ngamma\n' | xargs -I{} echo.exe '[{}]'" 0 "[alpha beta]`n[gamma]`n"
    Check-Brush 'which cargo' 0 "$([IO.Path]::ChangeExtension($cargo, 'EXE'))`n"
    Check-Brush 'which sed awk xargs' 0 "$(Join-Path $tools 'sed.EXE')`n$(Join-Path $tools 'awk.EXE')`n$(Join-Path $tools 'xargs.EXE')`n"
    Check-Tool which @('definitely-missing-2795') 1 ''
    Check-Tool which @('--unsupported') 2 ''
    Check-Tool which @('-a','which') 0 "$(Join-Path $tools 'which.EXE')`n"
    # Both hardlinks and copy fallbacks preserve applet dispatch by basename.
    $copyDirectory = Join-Path $fixture 'copy fallback'
    New-Item -ItemType Directory -Path $copyDirectory | Out-Null
    foreach ($name in @('sed','awk','xargs')) {
        Copy-Item (Join-Path $tools "$name.exe") (Join-Path $copyDirectory "$name.exe")
    }
    $savedTools = $tools
    try {
        $tools = $copyDirectory
        Check-Tool sed @('-n','2,3p','data.txt') 0 "beta`nalpha beta`n"
        Check-Tool awk @('{print $1}','data.txt') 0 "alpha`nbeta`nalpha`n"
        Check-Tool xargs @('-n1','echo.exe') 0 "alpha`nbeta`n" 'alpha beta'
    } finally { $tools = $savedTools }
    # The real installer must refuse modified downloads BEFORE extracting/staging tools.
    $badArchive = Join-Path $fixture 'bad.zip'
    [IO.File]::WriteAllText($badArchive, 'not the pinned upstream bytes', $utf8)
    $refused = $false
    try {
        & (Join-Path $PSScriptRoot 'Install-CarriedTools.ps1') -BinaryPath (Join-Path $tools 'newt-carried-tools.exe') -NewtDirectory $fixture -RipgrepArchive $badArchive -BusyboxArchive (Join-Path $tools 'sed.exe') | Out-Null
    } catch { $refused = $_.Exception.Message -match 'SHA256 mismatch' }
    if (!$refused -or (Test-Path (Join-Path $fixture 'tools'))) { throw 'corrupt archive was not refused cleanly' }
    $script:checks++
    $refused = $false
    try {
        & (Join-Path $PSScriptRoot 'Install-CarriedTools.ps1') -BinaryPath (Join-Path $tools 'newt-carried-tools.exe') -NewtDirectory $fixture -BusyboxArchive $badArchive | Out-Null
    } catch { $refused = $_.Exception.Message -match 'busybox-w32.*SHA256 mismatch' }
    if (!$refused -or (Test-Path (Join-Path $fixture 'tools'))) { throw 'corrupt BusyBox was not refused cleanly' }
    $script:checks++
    Write-Output "PASS: $script:checks packaged native command checks"
} finally {
    $env:PATH = $originalPath
    $env:PATHEXT = $originalPathext
    Remove-Item -LiteralPath $fixture -Recurse -Force
}
