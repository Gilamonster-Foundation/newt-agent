#Requires -Version 7.0
<#
.SYNOPSIS
Prepare the Windows NUL device for ordinary AppContainer tools, once per boot.
.DESCRIPTION
Run explicitly from an elevated PowerShell. Only the DACL of fixed
\\?\GLOBALROOT\Device\Null is changed: add a non-inheriting GR|GW equivalent
allow ACE for ALL APPLICATION PACKAGES. Existing ACEs and other security
components remain. This is host preparation, never a Newt runtime/model tool.
.PARAMETER SelfTest
Test the shared ACL transformation in memory; do not open or change any device.
#>
[CmdletBinding()]
param([switch]$SelfTest)

. "$PSScriptRoot/AppContainerHostAcl.ps1"
if ($SelfTest) {
    Invoke-NewtAppContainerPreparation -Operation SelfTest
} else {
    Invoke-NewtAppContainerPreparation -Operation Null
}
