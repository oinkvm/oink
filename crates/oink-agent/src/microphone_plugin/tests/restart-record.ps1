# Execute the production restart decisions and the volatile restart record
# against an isolated per-user registry root. Never dot-source the installer,
# launch vendor setup, query devices or write HKLM.
param([string]$Installer)
$ErrorActionPreference='Stop'
$ast=[Management.Automation.Language.Parser]::ParseFile($Installer,[ref]$null,[ref]$null)
foreach ($name in @('InstallNeedsRestart','UninstallNeedsRestart','RecordOperation')) {
    $function=$ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name},$true)
    Invoke-Expression $function.Extent.Text
}
function Device($status,$problem) { [pscustomobject]@{DeviceID='ROOT\MEDIA\0000';Status=[uint32]$status;Problem=[uint32]$problem} }
$started=0x0180200B # DN_ROOT_ENUMERATED|DRIVER_LOADED|STARTED|DISABLEABLE|NT_ENUMERATOR|NT_DRIVER
$cases=@(
    @('fresh install, device started',0,@(Device $started 0),$false),
    @('device failed to start after a vetoed removal',0,@(Device 0x01802401 10),$true),
    @('Windows asks for a restart (DN_NEED_RESTART)',0,@(Device ($started -bor 0x100) 0),$true),
    @('Windows asks for a restart (CM_PROB_NEED_RESTART)',0,@(Device 0x01802401 14),$true),
    @('other device problem is not a restart',0,@(Device 0x01802401 28),$false),
    @('generic reboot-required code',3010,@(Device $started 0),$true),
    @('no device identified',0,@(),$false)
)
foreach ($case in $cases) {
    if ((InstallNeedsRestart $case[1] $case[2]) -ne $case[3]) { throw "Wrong install decision: $($case[0])" }
}
foreach ($code in @(-1,-106,1,2,5)) {
    $failed=$false
    try { InstallNeedsRestart $code @() | Out-Null } catch { $failed=$true }
    if (!$failed) { throw "Install failure accepted: $code" }
}
foreach ($pair in @(@(0,$false),@(1,$true),@(3010,$true))) {
    if ((UninstallNeedsRestart $pair[0]) -ne $pair[1]) { throw "Wrong uninstall decision: $($pair[0])" }
}
foreach ($code in @(-1,2,5)) {
    $failed=$false
    try { UninstallNeedsRestart $code | Out-Null } catch { $failed=$true }
    if (!$failed) { throw "Uninstall failure accepted: $code" }
}

$base="Software\OinkRestartRecordTest-$([guid]::NewGuid())"
function Root { [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey($base) }
function Marker {
    $key=[Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("$base\SOFTWARE\Oink\MicrophoneRestartRequired")
    if ($null -eq $key) { return $null }
    try { return [pscustomobject]@{Operation=$key.GetValue('LastOperation');Required=$key.GetValue('Required')} } finally { $key.Dispose() }
}
try {
    RecordOperation 'install' $false (Root)
    if ($null -ne (Marker)) { throw 'A restart-free operation created the restart marker' }
    RecordOperation 'uninstall' $true (Root)
    $marker=Marker
    if ($null -eq $marker -or $marker.Operation -ne 'uninstall' -or $marker.Required -ne 1) { throw 'Restart marker missing after a restart-requiring operation' }
    # A later restart-free operation keeps the owed restart and names itself.
    RecordOperation 'install' $false (Root)
    $marker=Marker
    if ($null -eq $marker -or $marker.Operation -ne 'install' -or $marker.Required -ne 1) { throw 'Owed restart was cleared or not renamed' }
    $volatile=$false
    try { [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("$base\SOFTWARE\Oink\MicrophoneRestartRequired",$true).CreateSubKey('child',[Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree,[Microsoft.Win32.RegistryOptions]::None) | Out-Null }
    catch { $volatile=$true } # A non-volatile child under a volatile key is refused.
    if (!$volatile) { throw 'Restart marker is not volatile' }
} finally { [Microsoft.Win32.Registry]::CurrentUser.DeleteSubKeyTree($base,$false) }
Write-Output 'Restart decision and record cases passed; installer, devices and HKLM untouched'
