# Test setup only: model a non-elevated user's own repository on an elevated
# CI runner. Change ONLY ownership, never the DACL or Git's safe.directory.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Repository)
$ErrorActionPreference = 'Stop'
# Set-Acl persists more than owner state. Request OWNER_SECURITY_INFORMATION
# directly so this fixture cannot silently alter a filesystem grant.
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class NewtGitFixtureOwner {
    [DllImport("advapi32.dll", CharSet = CharSet.Unicode, ExactSpelling = true)]
    public static extern uint SetNamedSecurityInfoW(string path, int type,
        uint information, [In] byte[] owner, IntPtr group, IntPtr dacl, IntPtr sacl);
}
'@
$owner = [Security.Principal.WindowsIdentity]::GetCurrent().User
$ownerBytes = New-Object byte[] $owner.BinaryLength
$owner.GetBinaryForm($ownerBytes, 0)
foreach ($path in @($Repository, (Join-Path $Repository '.git'))) {
    $acl = Get-Acl -LiteralPath $path
    $before = $acl.GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::Access)
    $result = [NewtGitFixtureOwner]::SetNamedSecurityInfoW($path, 1, 1, $ownerBytes,
        [IntPtr]::Zero, [IntPtr]::Zero, [IntPtr]::Zero)
    if ($result -ne 0) { throw [System.ComponentModel.Win32Exception]::new([int]$result) }
    $after = Get-Acl -LiteralPath $path
    if ($after.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $owner.Value) {
        throw 'Git fixture owner readback did not match the invoking user SID.'
    }
    if ($after.GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::Access) -cne $before) {
        throw 'Git fixture ownership setup changed the DACL.'
    }
}
Write-Output 'Git fixture: invoking user owns repository and .git; DACLs unchanged.'
