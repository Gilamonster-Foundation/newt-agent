# Shared pinned-byte acquisition for Windows installers. Cache/offline bytes
# get the same check as fresh downloads; callers extract only after return.
function Get-PinnedArtifact {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Uri,
        [Parameter(Mandatory)][string]$Destination,
        [Parameter(Mandatory)][string]$Sha256,
        [string]$Archive
    )
    if ($Archive) { Copy-Item -LiteralPath $Archive -Destination $Destination }
    else {
        curl.exe --fail --location --retry 4 --retry-all-errors --retry-max-time 180 --connect-timeout 20 --max-time 60 --output $Destination $Uri
        if ($LASTEXITCODE -ne 0) { throw 'Pinned artifact download failed after bounded retries.' }
    }
    if ((Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Sha256) {
        throw "Pinned artifact SHA256 mismatch; refusing installation: $Uri"
    }
}
