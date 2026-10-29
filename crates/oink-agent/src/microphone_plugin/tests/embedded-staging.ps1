# Production hash verification, file pin and execution control flow in a temp
# directory. Never run the bootstrap entry point, UAC, or vendor/package tools.
param([string]$Bootstrap)
$ErrorActionPreference='Stop'
$errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile($Bootstrap,[ref]$null,[ref]$errors)
if ($errors.Count) { throw 'Bootstrap syntax errors' }
foreach ($name in @('PinInstaller','RunStagedInstaller')) {
    $definition=$ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name},$false)
    Invoke-Expression $definition.Extent.Text
}
$directory=Split-Path -Parent $Bootstrap
$token='0123456789abcdef0123456789abcdef'
$path=Join-Path $directory ('installer-'+$token+'.ps1')
$content='fixture only; no executable installer'
function Protected($value) {
    # The real policy is shared verbatim with the installer. Test files are
    # deliberately user-owned; assert this flow checks both boundaries instead.
    if ($value -notin @($directory,$path)) { throw 'Unexpected protected path' }
    $script:protectedPaths+=,$value
}
function WriteInstaller($path,$content) {
    if (Test-Path -LiteralPath $path) { throw 'Fixture already exists' }
    [IO.File]::WriteAllText($path,$content,(New-Object Text.UTF8Encoding($false)))
    if ($script:tamper) { [IO.File]::AppendAllText($path,'tampered') }
}
function Start-Process {
    param($FilePath,$WindowStyle,$ArgumentList,[switch]$Wait,[switch]$PassThru)
    if ($FilePath -ne (Join-Path ([Environment]::SystemDirectory) 'WindowsPowerShell/v1.0/powershell.exe') -or $WindowStyle -ne 'Hidden' -or !$Wait -or !$PassThru) { throw 'Unexpected installer process' }
    $expected=@('-NoLogo','-NoProfile','-NonInteractive','-WindowStyle','Hidden','-ExecutionPolicy','Bypass','-File',('"'+$path+'"'),'uninstall',$token)
    if (($ArgumentList -join ',') -ne ($expected -join ',')) { throw 'Unexpected installer arguments' }
    if ([IO.File]::ReadAllText($path) -cne $content) { throw 'Wrong embedded contents' }
    $locked=$false
    try { $writer=[IO.File]::Open($path,'Open','Write','ReadWrite'); $writer.Dispose() } catch { $locked=$true }
    if (!$locked) { throw 'Installer can be written while running' }
    $locked=$false
    try { [IO.File]::Delete($path) } catch { $locked=$true }
    if (!$locked) { throw 'Installer can be replaced while running' }
    $script:launches++
    if ($script:launchFailure) { throw 'fixture launch failure' }
    return @{ExitCode=$script:exitCode}
}
foreach ($scenario in @('success','child-failure','launch-failure','tampered')) {
    $script:launches=0; $script:protectedPaths=@()
    $script:tamper=$scenario -eq 'tampered'; $script:launchFailure=$scenario -eq 'launch-failure'
    $script:exitCode=if ($scenario -eq 'child-failure') {1} else {0}
    $threw=$false
    try { $code=RunStagedInstaller $directory $content 'uninstall' $token } catch { $threw=$true }
    if ($threw -ne ($scenario -in @('tampered','launch-failure'))) { throw "Wrong throw outcome: $scenario" }
    if (!$threw -and $code -ne $script:exitCode) { throw 'Child failure lost' }
    if ($script:launches -ne $(if($script:tamper){0}else{1})) { throw 'Incorrect launch count' }
    if (($script:protectedPaths -join ',') -ne "$directory,$path") { throw 'Directory/file protection not checked' }
    if (Test-Path -LiteralPath $path) { throw 'Transient script retained' }
}
[IO.File]::WriteAllText($path,'pre-existing fixture')
$threw=$false
try { RunStagedInstaller $directory $content 'uninstall' $token } catch { $threw=$true }
if (!$threw -or [IO.File]::ReadAllText($path) -cne 'pre-existing fixture') { throw 'Existing file was replaced or removed' }
Remove-Item -LiteralPath $path -Force
Write-Output '5 embedded staging cases passed; actual hash/pin and temp-file cleanup, mocked launch/privileged ACL creation'
