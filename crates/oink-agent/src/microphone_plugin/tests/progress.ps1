# Real staged PowerShell child and production receipt writer/poller, with only
# privileged path/ACL setup replaced by an ordinary isolated temp directory.
# Never execute the installer's body, UAC, vendor setup or device/registry APIs.
param([string]$Action,[string]$Token)
$ErrorActionPreference='Stop'
[Console]::InputEncoding=New-Object Text.UTF8Encoding($false)
$payload=[Console]::In.ReadToEnd() | ConvertFrom-Json
$bootstrap=[Management.Automation.Language.Parser]::ParseInput($payload.bootstrap,[ref]$null,[ref]$null)
$installer=[Management.Automation.Language.Parser]::ParseInput($payload.installer,[ref]$null,[ref]$null)
foreach($name in @('PinInstaller','RunStagedInstaller')) {
    $function=$bootstrap.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name},$false)
    Invoke-Expression $function.Extent.Text
}
$tokenAssignment=$installer.Find({param($node) $node -is [Management.Automation.Language.AssignmentStatementAst] -and $node.Left.Extent.Text -eq '$operationToken'},$false)
$writer=$installer.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Progress'},$false)
function Protected($path) {
    if (!(Split-Path -Parent $path).StartsWith($PSScriptRoot) -and $path -ne $PSScriptRoot) { throw 'Fixture escaped temp directory' }
}
function WriteInstaller($path,$content) { [IO.File]::WriteAllText($path,$content,(New-Object Text.UTF8Encoding($false))) }
$content=$tokenAssignment.Extent.Text+"`n"+$writer.Extent.Text+"`n"+@'
$ErrorActionPreference='Stop'
$progressReady=$true
$progressFile=Join-Path $PSScriptRoot 'status.json'
function Protected($path) { if ($path -ne $progressFile) { throw 'Unexpected receipt path' } }
$steps=if($args[0] -eq 'install'){@('verify','extract','install_driver','finish')}else{@('verify','uninstall_driver','finish')}
foreach($step in $steps) {
    Progress $step
    $ack=Join-Path $PSScriptRoot ($step+'.ack')
    $deadline=[DateTime]::UtcNow.AddSeconds(10)
    while (!(Test-Path -LiteralPath $ack)) {
        if([DateTime]::UtcNow -gt $deadline) { throw "Receipt was not observed while installer was running: $step" }
        Start-Sleep -Milliseconds 10
    }
}
'@
exit (RunStagedInstaller $PSScriptRoot $content $Action $Token)
