#Requires -Version 7.0
<#
.SYNOPSIS
Prepare bounded AppContainer DOS-path lookup permissions, once per boot.
.DESCRIPTION
Explicit elevated host setup of fixed NT namespace objects and metadata-only
MountPointManager access. No path, mask, or SID inputs; no broader fallback.
.PARAMETER NamespaceOnly
Prepare the fixed namespace query permissions without opening/changing the
MountPointManager device. Intended for the staged confined diagnostic.
.PARAMETER SelfTest
Test the shared ACL transformation in memory; do not open or change any device.
#>
[CmdletBinding(DefaultParameterSetName = 'Apply')]
param(
    [Parameter(ParameterSetName = 'Namespace')][switch]$NamespaceOnly,
    [Parameter(ParameterSetName = 'Test')][switch]$SelfTest
)

. "$PSScriptRoot/AppContainerHostAcl.ps1"
if ($SelfTest) {
    Invoke-NewtAppContainerPreparation -Operation SelfTest
} elseif ($NamespaceOnly) {
    Invoke-NewtAppContainerPreparation -Operation NamespaceOnly
} else {
    Invoke-NewtAppContainerPreparation -Operation DosPaths
}
