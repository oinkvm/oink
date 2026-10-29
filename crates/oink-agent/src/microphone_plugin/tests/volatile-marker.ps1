# Executes only the production marker writer against an in-memory registry.
param([string]$Installer)
$ErrorActionPreference='Stop'
$ast=[Management.Automation.Language.Parser]::ParseFile($Installer,[ref]$null,[ref]$null)
$writer=$ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'MarkRestartRequired'},$true)
if (!$writer) { throw 'Missing marker writer' }
Invoke-Expression $writer.Extent.Text
function FakeRegistry($store, $prefix = '', $options = [Microsoft.Win32.RegistryOptions]::None) {
    $root=[pscustomobject]@{ Store=$store; Prefix=$prefix; Options=$options; Values=@{}; Disposed=$false }
    $root | Add-Member ScriptMethod CreateSubKey {
        param($path,$permission,$options)
        if ($permission -ne [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree) { throw 'Incorrect access' }
        $full=if ($this.Prefix) { $this.Prefix+'\'+$path } else { $path }
        if (!$this.Store.ContainsKey($full)) { $this.Store[$full]=FakeRegistry $this.Store $full $options }
        return $this.Store[$full]
    }
    $root | Add-Member ScriptMethod SetValue {
        param($name,$value,$kind)
        if ($name -eq 'LastOperation' -and $kind -ne [Microsoft.Win32.RegistryValueKind]::String) { throw 'Operation must be REG_SZ' }
        $this.Values[$name]=$value
    }
    $root | Add-Member ScriptMethod Dispose { $this.Disposed=$true }
    return $root
}
$store=@{}
$path='SOFTWARE\Oink\MicrophoneRestartRequired'
$first=FakeRegistry $store
MarkRestartRequired 'install' $first
if (!$first.Disposed -or $store[$path].Options -ne [Microsoft.Win32.RegistryOptions]::Volatile -or $store[$path].Values.Required -ne 1) { throw 'Marker must be volatile and handles disposed' }
if ($store['SOFTWARE\Oink'].Options -ne [Microsoft.Win32.RegistryOptions]::None) { throw 'Parent must be durable' }
if ($store[$path].Values.LastOperation -ne 'install') { throw 'Successful install was not recorded' }
# A new application process does not erase machine state.
$second=FakeRegistry $store
if (!$second.Store.ContainsKey($path) -or $second.Store[$path].Values.LastOperation -ne 'install') { throw 'Marker and operation did not survive application restart' }
# Repeated successful operations keep the same reboot requirement.
MarkRestartRequired 'uninstall' $second
if ($store[$path].Values.LastOperation -ne 'uninstall' -or $store[$path].Values.Required -ne 1) { throw 'Latest successful operation must replace previous operation' }
# Invalid operation must not overwrite the previous success.
try { MarkRestartRequired 'unknown' (FakeRegistry $store); throw 'Invalid operation accepted' } catch [System.Management.Automation.ParameterBindingException] {}
if ($store[$path].Values.LastOperation -ne 'uninstall') { throw 'Failed attempt changed last operation' }
foreach ($name in @($store.Keys)) {
    if ($store[$name].Options -eq [Microsoft.Win32.RegistryOptions]::Volatile) { $store.Remove($name) }
}
if ($store.ContainsKey($path)) { throw 'Marker survived simulated Windows restart' }
# A fresh process after reboot sees neither the marker nor its nested operation.
$afterBoot=FakeRegistry $store
if ($afterBoot.Store.ContainsKey($path)) { throw 'Last operation survived simulated Windows restart' }
Write-Output 'volatile marker and last-operation fake-registry lifecycle passed'

if (!$store.ContainsKey('SOFTWARE\Oink')) { throw 'Windows restart must retain durable parent' }
