# Execute only the pure ownership predicate. Never dot-source the installer.
$ErrorActionPreference='Stop'
$tokens=$null; $errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'Microphone.ps1'),[ref]$tokens,[ref]$errors)
if ($errors.Count) { throw 'Installer syntax errors' }
$predicate=@($ast.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Matches'},$false))
if ($predicate.Count -ne 1) { throw 'Ownership predicate missing or ambiguous' }
Invoke-Expression $predicate[0].Extent.Text
$archiveHash='fixture archive'
$driver=[pscustomobject]@{DeviceID='fixture device';InfName='oem42.inf';DriverVersion='1.0'}
$record=[pscustomobject]@{Archive=$archiveHash;Device=$driver.DeviceID;Inf=$driver.InfName;Version=$driver.DriverVersion}
if (!(Matches $record @($driver))) { throw 'Matching owned device rejected' }
if (Matches $null @($driver)) { throw 'Pre-existing unmanaged device was adopted' }
if (Matches $record @()) { throw 'Missing driver accepted' }
if (Matches $record @($driver,$driver)) { throw 'Ambiguous driver accepted' }
foreach ($field in @('Archive','Device','Inf','Version')) {
    $old=$record.$field
    $record.$field='replaced'
    if (Matches $record @($driver)) { throw "Replaced $field accepted" }
    $record.$field=$old
}
Write-Output '8 ownership predicate cases passed; installer not executed'
