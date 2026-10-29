# Embedded in the signed agent and MSI custom action. Action is a fixed literal.
# No installation-time network access; never called automatically to install.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
# The elevated agent stages this embedded, verified script under protected
# ProgramData and holds it without write/delete sharing until this process exits.
if (!$Action) { $Action = $args[0] }
$operationToken = if ($args.Count -gt 1) { $args[1] } else { '' }
if ($operationToken -and $operationToken -notmatch '^[a-f0-9]{32}$') { throw 'Invalid microphone operation token' }
$archiveHash = 'b950e39f01af1d04ea623c8f6d8eb9b6ea5c477c637295fabf20631c85116bfb'
$setupHash = '734c35dfa6d98f48782a451633ceb471166ec70d60482fd89a1123d0ee3c4f41'
$signer = 'A77952D93229D0EC36E2543081EEA7D125732B9C'
$marker = 'HKLM:\SOFTWARE\Oink\Microphone'
$stage = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'OinkMicrophone'
$progressDirectory = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'OinkMicrophoneProgress'
$progressFile = Join-Path $progressDirectory 'status.json'
$progressReady = $false
$progressStep = 'verify'
$audioDefaults = @()
$bundle = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Oink\AudioPlugin\VBCABLE_Driver_Pack45.zip'
function Driver {
    if (!('OinkMicrophoneDevices' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32;
public static class OinkMicrophoneDevices {
    [StructLayout(LayoutKind.Sequential)] struct Device { public int Size; public Guid Class; public uint DevInst; public IntPtr Reserved; }
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr SetupDiGetClassDevsW(IntPtr cls, string enumerator, IntPtr parent, uint flags);
    [DllImport("setupapi.dll", SetLastError=true)] static extern bool SetupDiEnumDeviceInfo(IntPtr set, uint index, ref Device device);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool SetupDiGetDeviceInstanceIdW(IntPtr set, ref Device device, StringBuilder id, uint size, out uint needed);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool SetupDiGetDeviceRegistryPropertyW(IntPtr set, ref Device device, uint property, out uint type, byte[] data, uint size, out uint needed);
    [DllImport("setupapi.dll")] static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    public sealed class Driver { public string DeviceID; public string InfName; public string DriverVersion; }
    static string[] Property(IntPtr set, ref Device device, uint property) {
        byte[] data=new byte[65536]; uint type,needed;
        if (!SetupDiGetDeviceRegistryPropertyW(set,ref device,property,out type,data,(uint)data.Length,out needed)) {
            int error=Marshal.GetLastWin32Error();
            if(error==13) return new string[0];
            throw new Win32Exception(error);
        }
        return Encoding.Unicode.GetString(data,0,(int)needed).TrimEnd('\0').Split('\0');
    }
    public static Driver[] Query() {
        // Present devices include disabled ones but exclude uninstalled/ghost devnodes.
        IntPtr set=SetupDiGetClassDevsW(IntPtr.Zero,null,IntPtr.Zero,6);
        if(set==new IntPtr(-1)) throw new Win32Exception(Marshal.GetLastWin32Error());
        var found=new List<Driver>();
        var timer=System.Diagnostics.Stopwatch.StartNew();
        try {
            for(uint i=0;;i++) {
                if(timer.Elapsed.TotalSeconds>10) throw new TimeoutException("Microphone device enumeration timed out");
                Device device=new Device(); device.Size=Marshal.SizeOf(typeof(Device));
                if(!SetupDiEnumDeviceInfo(set,i,ref device)) {
                    int error=Marshal.GetLastWin32Error(); if(error==259) break; throw new Win32Exception(error);
                }
                var id=new StringBuilder(4096); uint needed;
                if(!SetupDiGetDeviceInstanceIdW(set,ref device,id,(uint)id.Capacity,out needed)) throw new Win32Exception(Marshal.GetLastWin32Error());
                bool match=id.ToString().IndexOf("VBAUDIOVACWDM",StringComparison.OrdinalIgnoreCase)>=0;
                foreach(string hw in Property(set,ref device,1)) match |= String.Equals(hw,"VBAudioVACWDM",StringComparison.OrdinalIgnoreCase);
                if(!match) continue;
                string[] keys=Property(set,ref device,9);
                if(keys.Length==0) continue;
                using(var root=RegistryKey.OpenBaseKey(RegistryHive.LocalMachine,RegistryView.Registry64))
                using(var key=root.OpenSubKey(@"SYSTEM\CurrentControlSet\Control\Class\"+keys[0])) {
                    if(key==null) continue;
                    string inf=key.GetValue("InfPath") as string, version=key.GetValue("DriverVersion") as string;
                    if(inf!=null && version!=null) found.Add(new Driver {DeviceID=id.ToString(),InfName=inf,DriverVersion=version});
                }
            }
        } finally { SetupDiDestroyDeviceInfoList(set); }
        return found.ToArray();
    }
}
'@
    }
    @([OinkMicrophoneDevices]::Query())
}
function Record { if (Test-Path -LiteralPath $marker) { Get-ItemProperty -LiteralPath $marker } }
function Matches($record, $drivers) {
    $record -and $record.Archive -eq $archiveHash -and $drivers.Count -eq 1 -and
    $record.Device -eq $drivers[0].DeviceID -and $record.Inf -eq $drivers[0].InfName -and
    $record.Version -eq $drivers[0].DriverVersion
}
function RemoveOwnedPackage($record) {
    # Called only after exact live-device ownership was checked and vendor -u
    # succeeded. The vendor removes the devnode, but leaves its staged INF and
    # stopped service. Delete only that receipt's package, without /force or
    # /uninstall: Windows must refuse if another device still uses the package.
    if (!$record -or $record.Archive -ne $archiveHash -or $record.Inf -notmatch '^oem[0-9]+\.inf$') {
        throw 'Invalid microphone driver package receipt'
    }
    $published = Join-Path ([Environment]::GetFolderPath('Windows')) ('INF\' + $record.Inf)
    if (!(Test-Path -LiteralPath $published)) { return } # Vendor already removed it.
    $packaged = Join-Path $stage 'vbMmeCable64_win10.inf'
    Protected $packaged
    if ((Get-FileHash -LiteralPath $published -Algorithm SHA256).Hash -ne (Get-FileHash -LiteralPath $packaged -Algorithm SHA256).Hash) {
        throw 'Microphone driver package differs from the verified vendor payload'
    }
    $pnputil = Join-Path ([Environment]::SystemDirectory) 'pnputil.exe'
    $process = Start-Process -FilePath $pnputil -WindowStyle Hidden -ArgumentList '/delete-driver',$record.Inf -Wait -PassThru
    if ($process.ExitCode -notin @(0,3010)) { throw "Microphone driver package removal failed: $($process.ExitCode)" }
}
function Protected($path) {
    # Reject junctions at every level and ACLs that grant untrusted principals write access.
    $item = Get-Item -LiteralPath $path -Force
    while ($item) {
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Reparse point in microphone payload path' }
        if ($item.FullName -eq [Environment]::GetFolderPath('CommonApplicationData')) { break }
        $acl = Get-Acl -LiteralPath $item.FullName
        $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
        $trusted = @('S-1-5-18','S-1-5-32-544','S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
        if ($owner -notin $trusted) { throw 'Untrusted microphone payload owner' }
        foreach ($rule in $acl.GetAccessRules($true,$true,[Security.Principal.SecurityIdentifier])) {
            if ($rule.AccessControlType -eq 'Allow' -and $rule.IdentityReference.Value -notin $trusted -and
                ([int]$rule.FileSystemRights -band 0x000D0116)) { throw 'Writable microphone payload path' }
        }
        if ($item.FullName -eq [Environment]::GetFolderPath('ProgramFiles')) { break }
        $item = if ($item -is [IO.FileInfo]) { $item.Directory } else { $item.Parent }
    }
}
# Only the elevated process writes here. The directory is created with its final
# DACL atomically; existing paths must pass the same owner/ACL/reparse checks as
# the installer payload. A random token prevents stale/cross-session receipts.
function BeginProgress {
    if (!$operationToken) { return }
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)')
    [IO.Directory]::CreateDirectory($progressDirectory, $acl) | Out-Null
    Protected $progressDirectory
    $script:progressReady = $true
}
function Progress($step, $detail = '') {
    $script:progressStep = $step
    if (!$progressReady) { return }
    if (Test-Path -LiteralPath $progressFile) { Protected $progressFile }
    @{token=$operationToken;step=$step;detail=$detail;audio_defaults=$audioDefaults} | ConvertTo-Json -Depth 4 -Compress | Set-Content -LiteralPath $progressFile -Encoding UTF8
}
function AudioDefaults {
    if (!('OinkMicrophoneAudioDefaults' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public sealed class OinkMicrophoneAudioDefaults : IDisposable {
    [ComImport, Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface Enumerator {
        [PreserveSig] int EnumAudioEndpoints(int flow, uint state, out IntPtr collection);
        [PreserveSig] int GetDefaultAudioEndpoint(int flow, int role, out Device device);
        [PreserveSig] int GetDevice([MarshalAs(UnmanagedType.LPWStr)] string id, out Device device);
    }
    [ComImport, Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface Device {
        [PreserveSig] int Activate(ref Guid id, uint context, IntPtr parameters, out IntPtr result);
        [PreserveSig] int OpenPropertyStore(uint access, out IntPtr properties);
        [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
        [PreserveSig] int GetState(out uint state);
    }
    // Windows exposes default-endpoint reads publicly, but its setter is the
    // IPolicyConfig COM interface used by the sound settings UI. Preserve its
    // ten preceding vtable slots; none of these unused methods is invoked.
    // Only restore an in-memory pre-vendor choice, never a receipt-supplied ID.
    // Windows 11 getter/state and QueryInterface were verified read-only;
    // setter decisions use mocks. No format, volume or routing policy is set.
    [ComImport, Guid("F8679F50-850A-41CF-9C72-430F290290C8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface Policy {
        [PreserveSig] int Unused0(); [PreserveSig] int Unused1();
        [PreserveSig] int Unused2(); [PreserveSig] int Unused3();
        [PreserveSig] int Unused4(); [PreserveSig] int Unused5();
        [PreserveSig] int Unused6(); [PreserveSig] int Unused7();
        [PreserveSig] int Unused8(); [PreserveSig] int Unused9();
        [PreserveSig] int SetDefaultEndpoint([MarshalAs(UnmanagedType.LPWStr)] string id, int role);
    }
    const int NotFound = unchecked((int)0x80070490);
    Enumerator enumerator = (Enumerator)Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid("BCDE0395-E52F-467C-8E3D-C4579291692E"), true));
    Policy policy;
    public string Get(int flow, int role) {
        Device device;
        int result = enumerator.GetDefaultAudioEndpoint(flow, role, out device);
        try {
            if (result == NotFound) return null;
            Marshal.ThrowExceptionForHR(result);
            string id; Marshal.ThrowExceptionForHR(device.GetId(out id)); return id;
        } finally { if (device != null) Marshal.ReleaseComObject(device); }
    }
    public bool Available(string id) {
        Device device;
        int result = enumerator.GetDevice(id, out device);
        try {
            if (result == NotFound) return false;
            Marshal.ThrowExceptionForHR(result);
            uint state; Marshal.ThrowExceptionForHR(device.GetState(out state));
            return state == 1; // Only present, enabled endpoints can be defaults.
        } finally { if (device != null) Marshal.ReleaseComObject(device); }
    }
    public void Set(string id, int role) {
        if (policy == null) policy = (Policy)Activator.CreateInstance(Type.GetTypeFromCLSID(new Guid("870AF99C-171D-4F9E-AF0D-E63DF40C2BC9"), true));
        Marshal.ThrowExceptionForHR(policy.SetDefaultEndpoint(id, role));
    }
    public void Dispose() {
        if (policy != null) { Marshal.ReleaseComObject(policy); policy = null; }
        if (enumerator != null) { Marshal.ReleaseComObject(enumerator); enumerator = null; }
    }
}
'@
    }
    return New-Object OinkMicrophoneAudioDefaults
}
function SnapshotAudioDefaults($api) {
    # Render/capture, each with independent console/multimedia/communications
    # roles. A missing prior default is not an instruction to choose a device.
    foreach ($flow in 0,1) {
        foreach ($role in 0,1,2) { @{flow=$flow;role=$role;id=$api.Get($flow,$role)} }
    }
}
function RestoreAudioDefaults($api, $before) {
    foreach ($entry in $before) {
        $change = @{flow=@('render','capture')[$entry.flow];role=@('console','multimedia','communications')[$entry.role];before=$entry.id;after=$null;outcome='query_failed';detail=''}
        try {
            $change.after = $api.Get($entry.flow,$entry.role)
            if ($entry.id -ceq $change.after) { continue }
            if (!$entry.id) { $change.outcome = 'no_previous_default' }
            elseif (!$api.Available($entry.id)) { $change.outcome = 'previous_unavailable' }
            else {
                $change.outcome = 'restore_failed'
                $api.Set($entry.id,$entry.role)
                if ($api.Get($entry.flow,$entry.role) -cne $entry.id) { throw 'Default endpoint did not return to its previous value' }
                $change.outcome = 'restored'
            }
        } catch {
            $message = $_.Exception.Message
            $change.detail = $message.Substring(0,[Math]::Min(256,$message.Length))
        }
        $change
    }
}
function SaveAudioDefaults($changes) {
    # Persist the last vendor operation for diagnosis, including SYSTEM MSI
    # cleanup where there is no interactive parent polling a progress receipt.
    try {
        $acl = New-Object Security.AccessControl.DirectorySecurity
        $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)')
        [IO.Directory]::CreateDirectory($progressDirectory, $acl) | Out-Null
        Protected $progressDirectory
        $path = Join-Path $progressDirectory 'audio-defaults.json'
        if (Test-Path -LiteralPath $path) { Protected $path }
        @{utc=[DateTime]::UtcNow.ToString('o');action=$Action;token=$operationToken;changes=@($changes)} | ConvertTo-Json -Depth 4 -Compress | Set-Content -LiteralPath $path -Encoding UTF8
    } catch { [Console]::Error.WriteLine('Cannot save default audio restoration report: ' + $_.Exception.Message) }
}
function InvokeVendorSetup($exe, $arguments) {
    $api = AudioDefaults
    try {
        $before = @(SnapshotAudioDefaults $api)
        try {
            return Start-Process -FilePath $exe -WorkingDirectory $stage -WindowStyle Hidden -ArgumentList $arguments -Wait -PassThru
        } finally {
            # The vendor can switch defaults even on a nonzero exit. Restore
            # only changed roles whose former endpoint is still available.
            $script:audioDefaults = @(RestoreAudioDefaults $api $before)
            SaveAudioDefaults $audioDefaults
        }
    } finally { $api.Dispose() }
}
function MarkRestartRequired([ValidateSet('install','uninstall')][string]$operation, $registry = $null) {
    # Volatile registry keys survive app restarts but Windows discards them at
    # the next system boot. No wall-clock/uptime approximation and no WMI.
    $root = if ($null -ne $registry) { $registry } else { [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64) }
    try {
        # Ensure the parent is durable even on the very first Oink installation.
        $parent = $root.CreateSubKey('SOFTWARE\Oink', [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, [Microsoft.Win32.RegistryOptions]::None)
        try {
            $key = $parent.CreateSubKey('MicrophoneRestartRequired', [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, [Microsoft.Win32.RegistryOptions]::Volatile)
            try {
                $key.SetValue('LastOperation', $operation, [Microsoft.Win32.RegistryValueKind]::String)
                $key.SetValue('Required', 1, [Microsoft.Win32.RegistryValueKind]::DWord)
            } finally { $key.Dispose() }
        } finally { $parent.Dispose() }
    } finally { $root.Dispose() }
}
function VerifySetup {
    $exe = Join-Path $stage 'VBCABLE_Setup_x64.exe'
    Protected $stage
    Protected $exe
    if ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash -ne $setupHash) { throw 'VB-CABLE setup hash mismatch' }
    # Authenticode validates the timestamp and trust chain, including expired signing certificates.
    $signature = Get-AuthenticodeSignature -LiteralPath $exe
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Thumbprint -ne $signer) {
        throw 'VB-CABLE Authenticode verification failed'
    }
    return $exe
}

try {
    if ($Action -eq 'query' -or $Cleanup) {
        $drivers = @(Driver)
        $record = Record
        $present = $drivers.Count -gt 0
        if ($Action -eq 'query') {
            @{installed=$present; managed=[bool](Matches $record $drivers)} | ConvertTo-Json -Compress
            exit 0
        }
        if ($Cleanup -and !(Matches $record $drivers)) { exit 0 }
    }
    if ($Action -notin @('install','uninstall')) { throw 'Unknown microphone action' }
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Administrator authorization required' }
    # Serialize explicit clicks across sessions and MSI removal.
    $mutex = New-Object Threading.Mutex($false, 'Global\OinkMicrophoneInstaller')
    if (!$mutex.WaitOne(0)) { throw 'A microphone operation is already pending' }
    try {
        if ($operationToken) { BeginProgress; Progress 'verify' }
        $drivers = @(Driver)
        $record = Record
        $present = $drivers.Count -gt 0
        if ($Action -eq 'install') {
            if ($present) { throw 'VB-CABLE is already installed; Oink will not take ownership' }
            Protected $bundle
            if ((Get-FileHash -LiteralPath $bundle -Algorithm SHA256).Hash -ne $archiveHash) { throw 'VB-CABLE archive hash mismatch' }
            if (Test-Path -LiteralPath $stage) { Protected $stage; Remove-Item -LiteralPath $stage -Recurse -Force }
            $acl = New-Object Security.AccessControl.DirectorySecurity
            $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)')
            [IO.Directory]::CreateDirectory($stage, $acl) | Out-Null
            Progress 'extract'
            Expand-Archive -LiteralPath $bundle -DestinationPath $stage
            $exe = VerifySetup
            Progress 'install_driver'
            $process = InvokeVendorSetup $exe @('-i','-h')
            if ($process.ExitCode -notin @(0,3010)) { throw "VB-CABLE install failed: $($process.ExitCode)" }
            MarkRestartRequired $Action
            Progress 'finish'
            $drivers = @(Driver)
            if ($drivers.Count -ne 1) { throw 'VB-CABLE installation could not be identified; ownership was not recorded' }
            New-Item -Path $marker -Force | Out-Null
            foreach ($entry in @{Archive=$archiveHash;Device=$drivers[0].DeviceID;Inf=$drivers[0].InfName;Version=$drivers[0].DriverVersion}.GetEnumerator()) {
                New-ItemProperty -Path $marker -Name $entry.Key -Value $entry.Value -PropertyType String -Force | Out-Null
            }
        } else {
            if (!(Matches $record $drivers)) {
                if ($Action -eq 'uninstall') { throw 'VB-CABLE is absent or not managed by Oink' }
            }
            $exe = VerifySetup
            Progress 'uninstall_driver'
            $process = InvokeVendorSetup $exe @('-u','-h')
            # Pack45's hash-pinned x64 uninstaller returns 1 after successful
            # removal with DI_NEEDRESTART/DI_NEEDREBOOT, not a Win32 error.
            # Its removal routine returns -1 on failure, 0 on completion, and
            # 1 when SP_DEVINSTALL_PARAMS.Flags & 0x180 is set. Confirmed from
            # the shipped binary and a Windows 11 needs-reboot removal log.
            # Accept 1 only here, after VerifySetup; pnputil has its own codes
            # and must still refuse an in-use or nonmatching owned package.
            if ($process.ExitCode -notin @(0,1,3010)) { throw "VB-CABLE uninstall failed: $($process.ExitCode)" }
            MarkRestartRequired $Action
            RemoveOwnedPackage $record
            Progress 'finish'
            Remove-Item -LiteralPath $marker -Force
            Remove-Item -LiteralPath $stage -Recurse -Force
        }
    } finally { $mutex.ReleaseMutex(); $mutex.Dispose() }
    exit 0
} catch {
    $failure = $_.Exception.Message
    try { Progress $progressStep $failure } catch {}
    [Console]::Error.WriteLine($failure); exit 1
}
