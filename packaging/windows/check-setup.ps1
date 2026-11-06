# Extract Burn's package container and bootstrapper metadata; require the matching MSI and signed VC++ runtime.
param([Parameter(Mandatory)][string]$Setup, [Parameter(Mandatory)][string]$Msi,
      [Parameter(Mandatory)][string]$Out, [string]$OutBA = "$Out-bootstrapper",
      [string]$Wix = "wix")
$ErrorActionPreference = "Stop"
& (Join-Path $PSScriptRoot "execution-level.ps1") -Executable $Setup
if (Test-Path -LiteralPath $Out) { throw "Setup extraction output already exists: $Out" }
if (Test-Path -LiteralPath $OutBA) { throw "Setup bootstrapper output already exists: $OutBA" }

# WiX Burn needs -outba to extract BootstrapperApplicationData.xml and preserve the
# attached-container filenames. Without it the payloads are emitted as opaque a0/a1 files.
& $Wix burn extract $Setup -o $Out -oba $OutBA
if ($LASTEXITCODE -ne 0) { throw "Setup extraction failed" }

$files = @(Get-ChildItem -LiteralPath $Out -Recurse -File)
$msiFiles = @($files | Where-Object { $_.Extension -ieq ".msi" })
$redistFiles = @($files | Where-Object { $_.Name -ieq "vc_redist.x64.exe" })
if ($msiFiles.Count -ne 1) { throw "Setup must embed exactly one MSI package; found $($msiFiles.Count)" }
if ($redistFiles.Count -ne 1) { throw "Setup must embed exactly one VC++ x64 redistributable; found $($redistFiles.Count)" }
if ((Get-FileHash -LiteralPath $msiFiles[0].FullName -Algorithm SHA256).Hash -ne
    (Get-FileHash -LiteralPath $Msi -Algorithm SHA256).Hash) { throw "Setup contains a different MSI" }
$signature = Get-AuthenticodeSignature -LiteralPath $redistFiles[0].FullName
if ($signature.Status -ne "Valid" -or
    $signature.SignerCertificate.Subject -notmatch "Microsoft Corporation") {
    throw "Setup contains an invalid or non-Microsoft VC++ redistributable"
}
Write-Output "Setup embeds the verified VC++ x64 prerequisite and the matching MSI"
