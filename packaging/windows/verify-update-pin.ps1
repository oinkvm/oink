# Read-only published package verification; never installs or executes MSI code.
param([Parameter(Mandatory=$true)][string]$Directory)
$ErrorActionPreference = 'Stop'
$trust = Get-Content -Raw "$PSScriptRoot/../update-trust.json" | ConvertFrom-Json
$epoch = @($trust.windows | Where-Object { $_.epoch -eq 'win-2026-01' })
if ($epoch.Count -ne 1) { throw 'Missing Windows epoch.' }
if ($epoch[0].authenticode_spki_sha256.Count -eq 0) { exit 0 }
$updateOpenSsl = (Get-Command openssl -ErrorAction SilentlyContinue).Source
if (-not $updateOpenSsl) { $updateOpenSsl = Join-Path $env:ProgramFiles 'Git/usr/bin/openssl.exe' }
if (-not (Test-Path -LiteralPath $updateOpenSsl)) { throw 'OpenSSL 3 is required for SPKI verification.' }
$updateOpenSslVersion = & $updateOpenSsl version
if ($LASTEXITCODE -ne 0 -or $updateOpenSslVersion -notmatch '^OpenSSL 3[.]') { throw 'OpenSSL 3 is required for SPKI verification.' }
$record = Get-Content -Raw (Join-Path $Directory 'release.json') | ConvertFrom-Json
if ($record.signing -ne 'signpath-approved-authenticode-verified' -or $record.artifacts.Count -ne 1) { throw 'Signed release required by configured pins.' }
$path = Join-Path $Directory $record.artifacts[0].name
if ((Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant() -ne $record.artifacts[0].sha256) { throw 'Package changed.' }
$signature = Get-AuthenticodeSignature -LiteralPath $path
if ($signature.Status -ne 'Valid' -or -not $signature.TimeStamperCertificate) { throw 'MSI signature/timestamp invalid.' }
$certificate = Join-Path $Directory 'signer-public.cer'
$key = Join-Path $Directory 'signer-public.pem'
$spki = Join-Path $Directory 'signer-spki.der'
try {
    [IO.File]::WriteAllBytes($certificate, $signature.SignerCertificate.RawData)
    & $updateOpenSsl x509 -inform DER -in $certificate -pubkey -noout -out $key
    if ($LASTEXITCODE) { throw 'Certificate public key extraction failed.' }
    & $updateOpenSsl pkey -pubin -in $key -outform DER -out $spki
    if ($LASTEXITCODE) { throw 'SPKI extraction failed.' }
    $pin = (Get-FileHash -Algorithm SHA256 -LiteralPath $spki).Hash.ToLowerInvariant()
    if ($epoch[0].authenticode_spki_sha256 -cnotcontains $pin) { throw 'MSI signer pin mismatch.' }
} finally {
    Remove-Item -LiteralPath $certificate,$key,$spki -ErrorAction SilentlyContinue
}
