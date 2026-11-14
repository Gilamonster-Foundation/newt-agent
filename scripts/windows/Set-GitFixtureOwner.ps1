# Test setup only: model a non-elevated user's own repository on an elevated
# CI runner. Change ONLY ownership, never the DACL or Git's safe.directory.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Repository)
$ErrorActionPreference = 'Stop'
$owner = [Security.Principal.WindowsIdentity]::GetCurrent().User
foreach ($path in @($Repository, (Join-Path $Repository '.git'))) {
    $acl = Get-Acl -LiteralPath $path
    $before = $acl.GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::Access)
    $acl.SetOwner($owner)
    Set-Acl -LiteralPath $path -AclObject $acl
    $after = Get-Acl -LiteralPath $path
    if ($after.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $owner.Value) {
        throw 'Git fixture owner readback did not match the invoking user SID.'
    }
    if ($after.GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::Access) -cne $before) {
        throw 'Git fixture ownership setup changed the DACL.'
    }
}
Write-Output 'Git fixture: invoking user owns repository and .git; DACLs unchanged.'
