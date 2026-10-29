# Embedded in the agent; stdin is the agent's embedded installer, never a caller
# file. Shipped 0.2.2 updaters reject a standalone Microphone.ps1 MSI File row.
# The native caller supplies only validated mode/action/token literals.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
[Console]::InputEncoding = New-Object Text.UTF8Encoding($false)
$payload = [Console]::In.ReadToEnd()
# Reuse the installer's owner/ACL/reparse policy without executing its body.
$ast = [Management.Automation.Language.Parser]::ParseInput($payload, [ref]$null, [ref]$null)
$protected = $ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Protected'}, $false)
if (!$protected) { throw 'Embedded microphone protection policy is missing' }
Invoke-Expression $protected.Extent.Text

function PinInstaller($path, $expected) {
    Protected $path
    $file = [IO.File]::Open($path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $hasher = [Security.Cryptography.SHA256]::Create()
        try {
            $actual = [Convert]::ToBase64String($hasher.ComputeHash($file))
            $bytes = (New-Object Text.UTF8Encoding($false)).GetBytes($expected)
            $wanted = [Convert]::ToBase64String($hasher.ComputeHash($bytes))
            if ($actual -cne $wanted) { throw 'Microphone installer script verification failed' }
        } finally { $hasher.Dispose() }
        return $file
    } catch { $file.Dispose(); throw }
}
function WriteInstaller($path, $content) {
    # CreateNew and the final protected DACL reject pre-existing files, links and
    # non-admin writers before any script bytes are written.
    $acl = New-Object Security.AccessControl.FileSecurity
    $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;BU)')
    $file = New-Object IO.FileStream($path, [IO.FileMode]::CreateNew, [Security.AccessControl.FileSystemRights]::FullControl, [IO.FileShare]::None, 4096, [IO.FileOptions]::None, $acl)
    try {
        $bytes = (New-Object Text.UTF8Encoding($false)).GetBytes($content)
        $file.Write($bytes, 0, $bytes.Length)
        $file.Flush($true)
    } catch {
        $file.Dispose()
        Remove-Item -LiteralPath $path -Force
        throw
    } finally { $file.Dispose() }
}
function RunStagedInstaller($directory, $content, $action, $token) {
    Protected $directory
    $path = Join-Path $directory ('installer-' + $token + '.ps1')
    WriteInstaller $path $content
    $pin = $null
    try {
        $pin = PinInstaller $path $content
        $powershell = Join-Path ([Environment]::SystemDirectory) 'WindowsPowerShell/v1.0/powershell.exe'
        $process = Start-Process -FilePath $powershell -WindowStyle Hidden -ArgumentList @(
            '-NoLogo','-NoProfile','-NonInteractive','-WindowStyle','Hidden','-ExecutionPolicy','Bypass',
            '-File', ('"' + $path + '"'), $action, $token) -Wait -PassThru
        return $process.ExitCode
    } finally {
        if ($pin) { $pin.Dispose() }
        Remove-Item -LiteralPath $path -Force
    }
}

try {
    if ($Mode -eq 'elevate') {
        $agent = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Oink\oink-agent.exe'
        Protected $agent
        # Keep the already-installed, protected executable immutable across UAC.
        $pin = [IO.File]::Open($agent, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        try {
            $process = Start-Process -FilePath $agent -Verb RunAs -WindowStyle Hidden -ArgumentList @(
                'complete-microphone-installation', $Action, $operationToken) -Wait -PassThru
            if ($process.ExitCode -ne 0) { throw "Virtual microphone operation failed (agent exit $($process.ExitCode))" }
        } finally { $pin.Dispose() }
    } elseif ($Mode -eq 'execute') {
        $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
        if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Administrator authorization required' }
        $directory = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'OinkMicrophoneProgress'
        $acl = New-Object Security.AccessControl.DirectorySecurity
        $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)')
        [IO.Directory]::CreateDirectory($directory, $acl) | Out-Null
        exit (RunStagedInstaller $directory $payload $Action $operationToken)
    } else { throw 'Unknown microphone bootstrap mode' }
    exit 0
} catch {
    if ($_.Exception.NativeErrorCode -eq 1223 -or $_.Exception.InnerException.NativeErrorCode -eq 1223) {
        [Console]::Error.WriteLine('Microphone authorization cancelled'); exit 1223
    }
    [Console]::Error.WriteLine($_.Exception.Message); exit 1
}
