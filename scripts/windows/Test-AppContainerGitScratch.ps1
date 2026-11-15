# Report-only control for the historical scratch-drive Git cwd failure.
# No ownership change, safe.directory setting, or additional grant is made.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$LauncherPath,
    [Parameter(Mandatory)][ValidateSet('BeforeDosProbes', 'AfterReadProbe', 'AfterWriteProbe')][string]$Phase
)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$work = Join-Path $env:RUNNER_TEMP 'actest'
$git = (Get-Command git.exe).Source
New-Item -ItemType Directory -Force $work | Out-Null
Push-Location $work
try {
    if (-not (Test-Path .git)) {
        & $git init -q -b main
        if ($LASTEXITCODE -ne 0) { throw 'Scratch control init failed.' }
        "original`n" | Out-File tracked.txt -NoNewline
        & $git add tracked.txt
        if ($LASTEXITCODE -ne 0) { throw 'Scratch control host add failed.' }
        & $git -c user.name=diag -c user.email=diag@example.invalid commit -qm init
        if ($LASTEXITCODE -ne 0) { throw 'Scratch control host commit failed.' }
        "stage me`n" | Out-File pending.txt -NoNewline
    }
    function Show-Snapshot($label) {
        foreach ($entry in @(@('scratch-parent', $env:RUNNER_TEMP), @('repository', $work), @('git-dir', (Join-Path $work '.git')))) {
            $acl = Get-Acl -LiteralPath $entry[1]
            $sddl = $acl.GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]'Access, Owner')
            # Keep masks, inheritance flags, ACE ordering and well-known SIDs;
            # omit machine/account identifiers and per-launch profile SID data.
            $sddl = $sddl -replace 'S-1-5-21(?:-\d+){4}', 'ACCOUNT'
            $sddl = $sddl -replace 'S-1-15-2(?:-\d+){2,}', 'APPCONTAINER'
            Write-Output "ACL $Phase $label $($entry[0]): $sddl"
        }
    }
    function Probe($label, $exe, $argv) {
        Write-Output "CONTROL $Phase $label"
        & $LauncherPath --fs-write $work $exe @argv
        Write-Output "CONTROL EXIT=$LASTEXITCODE"
    }
    # The first sequence exactly preserves the old launch order, without an
    # extra cwd/listing command or ACL query before its Git operations.
    if ($Phase -ne 'BeforeDosProbes') { Show-Snapshot 'before-launches' }
    Probe 'old-order cmd echo' cmd @('/c', 'echo hello')
    Probe 'old-order whoami' "$env:WINDIR\System32\whoami.exe" @()
    Probe 'old-order git version' $git @('--version')
    Probe 'old-order git status' $git @('status')
    Probe 'old-order git add' $git @('add', '--', 'pending.txt')
    Show-Snapshot 'after-launches'
    Probe 'git status after ACL snapshot' $git @('status')
    # This is evidence collection, not a success gate. Required user-owned Git
    # effects remain separately gated by the direct and through-Newt probes.
    Write-Output 'CONTROL complete (report-only; inspect every exit code).'
} finally { Pop-Location }
exit 0
