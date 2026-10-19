param(
    [Parameter(Mandatory=$true)][string]$Directory,
    [Parameter(Mandatory=$true)][string]$Record,
    [Parameter(Mandatory=$true)][string]$ToolCache
)
$ErrorActionPreference = 'Stop'
function Copy-ExtractedPe([xml]$Manifest, [string]$Extracted, [string]$Name, [string]$Destination) {
    # WiX extracts by File-table ID, not the installed long filename. Resolve
    # the ID through its decompiled name, then restore .exe for Authenticode's
    # subject provider. Never follow a decompiled Source path outside extraction.
    $entries = @($Manifest.SelectNodes("//*[local-name()='File']") | Where-Object { $_.GetAttribute('Name') -ceq $Name })
    if ($entries.Count -ne 1 -or $Name -notmatch '^[A-Za-z0-9_.-]+\.exe$') { throw 'Unexpected installed executable inventory.' }
    $id = $entries[0].GetAttribute('Id')
    if ($id -notmatch '^[A-Za-z_][A-Za-z0-9_.]*$') { throw 'Invalid extracted file identifier.' }
    $source = Join-Path (Join-Path $Extracted 'File') $id
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw 'Missing extracted executable.' }
    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    $target = Join-Path $Destination $Name
    Copy-Item -LiteralPath $source -Destination $target
    return $target
}
$identity = Get-Content -Raw "$PSScriptRoot/../../packaging/identity.json" | ConvertFrom-Json
$packages = @(Get-ChildItem -LiteralPath $Directory -Filter '*.msi')
if ($packages.Count -ne 1) { throw 'Expected one signed MSI.' }
$msi = $packages[0]
$signature = Get-AuthenticodeSignature -LiteralPath $msi.FullName
if ($signature.Status -ne 'Valid' -or -not $signature.TimeStamperCertificate) { throw 'MSI signature or timestamp invalid.' }
$wix = Join-Path $ToolCache 'wix-5.0.2/tools/net6.0/any/wix.dll'
$extracted = Join-Path $Directory 'verified-extracted'
& dotnet $wix msi decompile $msi.FullName -o (Join-Path $Directory 'verified.wxs') -x $extracted
if ($LASTEXITCODE -ne 0) { throw 'Signed MSI decompilation failed.' }
[xml]$manifest = Get-Content -Raw -LiteralPath (Join-Path $Directory 'verified.wxs')
$verifiedComponents = @()
foreach ($role in @('ui','agent','service','update_worker','crash_collector')) {
    $name = $identity.binaries.$role + '.exe'
    $file = Copy-ExtractedPe -Manifest $manifest -Extracted $extracted -Name $name -Destination (Join-Path $Directory 'verified-pe')
    $sig = Get-AuthenticodeSignature -LiteralPath $file
    if ($sig.Status -ne 'Valid' -or -not $sig.TimeStamperCertificate -or $sig.SignerCertificate.Thumbprint -ne $signature.SignerCertificate.Thumbprint) {
        throw 'Embedded executable signature, timestamp or signer mismatch.'
    }
    $verifiedComponents += @{ path=$name; sha256=(Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash.ToLowerInvariant() }
}
$recordData = Get-Content -Raw -LiteralPath $Record | ConvertFrom-Json
$recordData.signing = 'signpath-approved-authenticode-verified'
$recordData.embedded_executables = $verifiedComponents
$recordData.artifacts = @(@{ name=$msi.Name; size=$msi.Length; sha256=(Get-FileHash -Algorithm SHA256 -LiteralPath $msi.FullName).Hash.ToLowerInvariant() })
$recordData | ConvertTo-Json -Depth 10 | Set-Content -Encoding utf8 -LiteralPath $Record
"$($recordData.artifacts[0].sha256)  $($msi.Name)" | Set-Content -Encoding ascii -LiteralPath (Join-Path (Split-Path -Parent $Record) 'SHA256SUMS')
