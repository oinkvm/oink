# Execute production uninstall control flow against in-memory command doubles.
# Never dot-source the installer or launch vendor setup / pnputil.
param([string]$Installer)
$ErrorActionPreference='Stop'
$ast=[Management.Automation.Language.Parser]::ParseFile($Installer,[ref]$null,[ref]$null)
$cleanup=$ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'RemoveOwnedPackage'},$true)
Invoke-Expression $cleanup.Extent.Text
$branch=$ast.Find({param($node) $node -is [Management.Automation.Language.IfStatementAst] -and $node.Clauses[0].Item1.Extent.Text -eq '$Action -eq ''install'''},$true)
$body=$branch.ElseClause.Extent.Text
$uninstall=[scriptblock]::Create($body.Substring(1,$body.Length-2))
$archiveHash='verified-archive'
$stage='C:\fixture\stage'
$marker='HKLM:\fixture\receipt'
$Action='uninstall'
$record=[pscustomobject]@{Archive=$archiveHash;Inf='oem42.inf'}
$drivers=@('fixture-device')
function Matches($record,$drivers) { return $script:owned }
function VerifySetup { return 'C:\fixture\vendor.exe' }
function Protected($path) { if ($path -ne (Join-Path $stage 'vbMmeCable64_win10.inf')) { throw 'Unexpected protected path' } }
function Test-Path { param($LiteralPath) return $script:packageExists }
function Get-FileHash { param($LiteralPath,$Algorithm) return @{Hash=if($LiteralPath -like '*\INF\*' -and !$script:hashMatches){'other'}else{'same'}} }
function MarkRestartRequired($action) { $script:restart=$true }
function Progress($step) {}
function Remove-Item { param($LiteralPath,[switch]$Force,[switch]$Recurse) $script:removed+=,$LiteralPath }
# The default-endpoint wrapper is covered separately; keep this ownership test
# at its existing vendor/process boundary without executing native audio APIs.
function InvokeVendorSetup($exe,$arguments) {
    Start-Process -FilePath $exe -WindowStyle Hidden -ArgumentList $arguments -Wait -PassThru
}
function Start-Process {
    param($FilePath,$WindowStyle,$ArgumentList,[switch]$Wait,[switch]$PassThru)
    if($FilePath -eq 'C:\fixture\vendor.exe') {
        if (($ArgumentList -join ',') -ne '-u,-h') { throw 'Unexpected vendor command' }
        $script:vendorCalls++
        return @{ExitCode=$script:vendorExit}
    }
    if ($FilePath -ne (Join-Path ([Environment]::SystemDirectory) 'pnputil.exe') -or $WindowStyle -ne 'Hidden' -or !$Wait -or !$PassThru) { throw 'Unexpected package executable/options' }
    if (($ArgumentList -join ',') -ne '/delete-driver,oem42.inf') { throw 'Must delete only exact recorded package without force/uninstall' }
    $script:packageCalls++
    return @{ExitCode=$script:packageExit}
}
function RunCase($label,$owner,$vendor,$package,$exists,$hash,$success,$expectedPackages,$expectedVendor,$expectedRestart) {
    $script:owned=$owner; $script:vendorExit=$vendor; $script:packageExit=$package
    $script:packageExists=$exists; $script:hashMatches=$hash
    $script:removed=@(); $script:packageCalls=0; $script:vendorCalls=0; $script:restart=$false
    $passed=$true
    try { & $uninstall } catch { $passed=$false }
    if ($passed -ne $success -or $script:packageCalls -ne $expectedPackages -or $script:vendorCalls -ne $expectedVendor -or $script:restart -ne $expectedRestart) { throw "Wrong outcome: $label" }
    if ($success) {
        if (($script:removed -join ',') -ne "$marker,$stage") { throw "Receipt/stage cleanup missing: $label" }
    } elseif ($script:removed.Count -ne 0) { throw "Failed uninstall discarded ownership/retry payload: $label" }
}
RunCase 'complete' $true 0 0 $true $true $true 1 1 $true
RunCase 'vendor-needs-reboot' $true 1 0 $true $true $true 1 1 $true
RunCase 'vendor-and-package-need-reboot' $true 1 3010 $true $true $true 1 1 $true
RunCase 'vendor-needs-reboot-package-in-use' $true 1 5 $true $true $false 1 1 $true
RunCase 'vendor-needs-reboot-different-package' $true 1 0 $true $false $false 0 1 $true
RunCase 'package-code-one-is-still-failure' $true 1 1 $true $true $false 1 1 $true
RunCase 'pending-reboot' $true 3010 3010 $true $true $true 1 1 $true
RunCase 'vendor-already-deleted-package' $true 0 0 $false $true $true 0 1 $true
RunCase 'package-in-use' $true 0 5 $true $true $false 1 1 $true
RunCase 'vendor-failure' $true 5 0 $true $true $false 0 1 $false
RunCase 'vendor-negative-failure' $true -1 0 $true $true $false 0 1 $false
RunCase 'vendor-unknown-positive-failure' $true 2 0 $true $true $false 0 1 $false
RunCase 'unmanaged' $false 0 0 $true $true $false 0 0 $false
RunCase 'different-package-content' $true 0 0 $true $false $false 0 1 $true
$record.Inf='..\other.inf'
RunCase 'invalid-package-receipt' $true 0 0 $true $true $false 0 1 $true
Write-Output '15 production uninstall cases passed; no external installer/package command executed'
