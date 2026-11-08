# Exercise the real extraction function without invoking signing or an installer.
$ErrorActionPreference='Stop'
$tokens=$null; $errors=$null
$ast=[System.Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'verify.ps1'),[ref]$tokens,[ref]$errors)
if($errors.Count){throw 'Verifier syntax invalid'}
$function=$ast.Find({param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Copy-ExtractedPe'},$true)
Invoke-Expression $function.Extent.Text
$root=Join-Path ([IO.Path]::GetTempPath()) ('oink-verify-'+[guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Path (Join-Path $root 'File') -Force | Out-Null
    [IO.File]::WriteAllBytes((Join-Path $root 'File\payload'),[byte[]](1,2,3))
    foreach($name in @('OinkPasteMenu.dll','oink.exe')) {
        [xml]$manifest="<Wix><File Name='$name' Id='payload'/></Wix>"
        $path=Copy-ExtractedPe $manifest $root $name (Join-Path $root 'restored')
        if((Split-Path -Leaf $path) -cne $name -or (Get-FileHash $path).Hash -ne (Get-FileHash (Join-Path $root 'File\payload')).Hash){throw 'PE name or bytes changed'}
    }
    foreach($name in @('../escape.dll','script.ps1','name:stream.dll')) {
        [xml]$manifest="<Wix><File Name='$name' Id='payload'/></Wix>"
        $rejected=$false
        try {Copy-ExtractedPe $manifest $root $name (Join-Path $root 'restored') | Out-Null} catch {$rejected=$true}
        if(-not $rejected){throw "Accepted invalid name $name"}
    }
    Write-Output 'PASS final signer extraction: DLL + EXE preserve names/bytes; 3 invalid names rejected'
} finally {Remove-Item -LiteralPath $root -Recurse -Force}
