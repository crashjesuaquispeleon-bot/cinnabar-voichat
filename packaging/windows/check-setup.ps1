# Extract Burn's package container without installing; require the matching MSI and signed VC++ runtime.
param([Parameter(Mandatory)][string]$Setup, [Parameter(Mandatory)][string]$Msi,
      [Parameter(Mandatory)][string]$Out, [string]$Wix = "wix")
$ErrorActionPreference = "Stop"
& (Join-Path $PSScriptRoot "execution-level.ps1") -Executable $Setup
if (Test-Path -LiteralPath $Out) { throw "Setup extraction output already exists: $Out" }
& $Wix burn extract $Setup -o $Out
if ($LASTEXITCODE -ne 0) { throw "Setup extraction failed" }

$files = @(Get-ChildItem -LiteralPath $Out -Recurse -File)
$msiFiles = @($files | Where-Object { $_.Extension -ieq ".msi" })
$redistFiles = @($files | Where-Object { $_.Name -ieq "vc_redist.x64.exe" })
if ($msiFiles.Count -ne 1) { throw "Setup must embed exactly one MSI package" }
if ($redistFiles.Count -ne 1) { throw "Setup must embed exactly one VC++ x64 redistributable" }
if ((Get-FileHash -LiteralPath $msiFiles[0].FullName -Algorithm SHA256).Hash -ne
    (Get-FileHash -LiteralPath $Msi -Algorithm SHA256).Hash) { throw "Setup contains a different MSI" }
$signature = Get-AuthenticodeSignature -LiteralPath $redistFiles[0].FullName
if ($signature.Status -ne "Valid" -or
    $signature.SignerCertificate.Subject -notmatch "Microsoft Corporation") {
    throw "Setup contains an invalid or non-Microsoft VC++ redistributable"
}
Write-Output "Setup embeds the verified VC++ x64 prerequisite and the matching MSI"
