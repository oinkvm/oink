# Production decisions and vendor boundary; endpoint API/process/audit are mocks.
# No real default endpoint writes, vendor execution, UAC or device mutations.
param([string]$Installer,[string]$Receipt)
$ErrorActionPreference='Stop'
$ast=[Management.Automation.Language.Parser]::ParseFile($Installer,[ref]$null,[ref]$null)
foreach($name in @('SnapshotAudioDefaults','RestoreAudioDefaults','InvokeVendorSetup','Progress')) {
    $definition=$ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name},$false)
    Invoke-Expression $definition.Extent.Text
}
function Assert($condition,$message) { if(!$condition) { throw $message } }
function FakeApi {
    $values=@{}
    foreach($flow in 0,1) { foreach($role in 0,1,2) { $values[('{0}:{1}' -f $flow,$role)]="$flow-$role-old" } }
    $api=[pscustomobject]@{Values=$values;AvailableIds=@($values.Values);Writes=@();ReadFailure='';SetFailure='';IgnoreWrite='';Disposed=$false}
    $api | Add-Member ScriptMethod Get {
        param($flow,$role)
        $key='{0}:{1}' -f $flow,$role
        if($key -eq $this.ReadFailure) { throw 'fixture query failure' }
        return $this.Values[$key]
    }
    $api | Add-Member ScriptMethod Available { param($id) return $this.AvailableIds -contains $id }
    $api | Add-Member ScriptMethod Set {
        param($id,$role)
        $flow=if($id.StartsWith('0-')){0}else{1}
        $key='{0}:{1}' -f $flow,$role
        $this.Writes+=@{key=$key;id=$id}
        if($key -eq $this.SetFailure) { throw 'fixture setter failure' }
        if($key -ne $this.IgnoreWrite) { $this.Values[$key]=$id }
    }
    $api | Add-Member ScriptMethod Dispose { $this.Disposed=$true }
    return $api
}
$api=FakeApi
$before=@(SnapshotAudioDefaults $api)
Assert ($before.Count -eq 6) 'All render/capture roles must be captured separately'
Assert (@(RestoreAudioDefaults $api $before).Count -eq 0 -and $api.Writes.Count -eq 0) 'Unchanged defaults must not be written'
foreach($key in @($api.Values.Keys)) { $api.Values[$key]='vendor-choice' }
$changes=@(RestoreAudioDefaults $api $before)
Assert ($changes.Count -eq 6 -and @($changes | Where-Object outcome -ne 'restored').Count -eq 0) 'All changed available roles must be restored'
foreach($entry in $before) { Assert ($api.Get($entry.flow,$entry.role) -ceq $entry.id) 'Roles were conflated' }
Assert (@($changes | Where-Object after -ne 'vendor-choice').Count -eq 0) 'Changed endpoint must be logged'

$api=FakeApi; $api.Values['0:1']=$null; $before=@(SnapshotAudioDefaults $api)
$api.Values['0:0']='fallback'; $api.Values['0:1']='vendor-first-default'
$api.AvailableIds=@($api.AvailableIds | Where-Object {$_ -ne '0-0-old'})
$changes=@(RestoreAudioDefaults $api $before)
Assert ($changes.Count -eq 2 -and $api.Writes.Count -eq 0) 'Removed/unavailable and previously absent endpoints must not be selected'
Assert (@($changes | Where-Object outcome -eq 'previous_unavailable').Count -eq 1) 'Missing endpoint not reported'
Assert (@($changes | Where-Object outcome -eq 'no_previous_default').Count -eq 1) 'New first default should remain'
Assert ($api.Values['1:0'] -eq '1-0-old') 'Unchanged recording default was touched'

foreach($failure in @('SetFailure','IgnoreWrite','ReadFailure')) {
    $api=FakeApi; $before=@(SnapshotAudioDefaults $api)
    $api.Values['0:0']='vendor-choice'; $api.Values['1:2']='vendor-capture'
    $api.$failure='0:0'
    $changes=@(RestoreAudioDefaults $api $before)
    Assert ($changes.Count -eq 2 -and $changes[0].outcome -like '*failed' -and $changes[0].detail) 'Restoration/query failure must be reported'
    Assert ($api.Values['1:2'] -eq '1-2-old') 'One role failure must not suppress another role restore'
}

function AudioDefaults { return $script:api }
function SaveAudioDefaults($changes) { $script:audit=@($changes) }
function Start-Process {
    param($FilePath,$WorkingDirectory,$WindowStyle,$ArgumentList,[switch]$Wait,[switch]$PassThru)
    Assert ($FilePath -eq 'fixture-vendor' -and $WorkingDirectory -eq 'fixture-stage' -and $WindowStyle -eq 'Hidden' -and $Wait -and $PassThru) 'Vendor launch contract changed'
    Assert (($ArgumentList -join ',') -eq "$script:flag,-h") 'Vendor arguments changed'
    $script:launches++
    # Even a failed vendor process may already have changed a default.
    $script:api.Values['0:0']='vendor-choice'
    if($script:launchFailure) { throw 'fixture launch failure' }
    return @{ExitCode=9}
}
$stage='fixture-stage'
foreach($flag in @('-i','-u')) {
    foreach($launchFailure in @($false,$true)) {
        $script:api=FakeApi; $script:launches=0; $script:audit=@()
        $threw=$false
        try { $result=InvokeVendorSetup 'fixture-vendor' @($flag,'-h') } catch { $threw=$true }
        Assert ($threw -eq $launchFailure) 'Vendor exception was lost'
        if(!$threw) { Assert ($result.ExitCode -eq 9) 'Vendor exit code was changed' }
        Assert ($api.Values['0:0'] -eq '0-0-old' -and $api.Disposed -and $launches -eq 1) 'Defaults must be restored and API released on both failure paths'
        Assert ($audit.Count -eq 1 -and $audit[0].outcome -eq 'restored') 'Vendor restoration was not audited'
    }
}
$script:api=FakeApi; $api.ReadFailure='0:0'; $script:launches=0
$threw=$false
try { InvokeVendorSetup 'fixture-vendor' @('-i','-h') } catch { $threw=$true }
Assert ($threw -and $launches -eq 0 -and $api.Disposed) 'Failed snapshot must prevent the vendor mutation'
if($Receipt) {
    $api=FakeApi; $before=@(SnapshotAudioDefaults $api)
    foreach($key in @($api.Values.Keys)) { $api.Values[$key]='vendor-choice' }
    $script:audioDefaults=@(RestoreAudioDefaults $api $before)
    $operationToken='bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'
    $progressFile=$Receipt; $progressReady=$true
    function Protected($path) { Assert ($path -eq $progressFile) 'Unexpected fixture progress path' }
    Progress 'finish'
}
Write-Output '11 default-audio decision/boundary cases passed; endpoint API and vendor launch mocked'
