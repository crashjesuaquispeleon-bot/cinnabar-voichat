# Builds the Windows payload, MSI and self-contained setup EXE. Requires WiX v5,
# WixToolset.BootstrapperApplications.wixext v5, and ImageMagick.
# Usage: build-installer.ps1 [-Out <dir>].
param([string]$Out = ".local/dist/windows-release", [string]$Binaries = "",
      [string]$Root = (Join-Path $PSScriptRoot "../.."))
$ErrorActionPreference = "Stop"
$root = (Resolve-Path -LiteralPath $Root).Path
$release = if ($Binaries) { (Resolve-Path -LiteralPath $Binaries).Path } else { Join-Path $root "target/release" }
$runtimeNames = @(Get-Content -LiteralPath (Join-Path $root "packaging/common/runtime-binaries.txt") | ForEach-Object {
    ($_ -split '\s+')[0] + ".exe"
})
foreach ($name in @($runtimeNames) + @("assetc.exe")) {
    if (-not (Test-Path (Join-Path $release $name))) { throw "missing $release\$name; run make package-binaries" }
}
$manifest = Get-Content -LiteralPath (Join-Path $root "Cargo.toml") -Raw
$workspace = [regex]::Match($manifest, '(?ms)^\[workspace\.package\]\s*\r?\n(.*?)(?=^\[|\z)')
$versionMatch = [regex]::Match($workspace.Groups[1].Value, '(?m)^\s*version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) { throw "missing workspace.package.version in source Cargo.toml" }
$version = $versionMatch.Groups[1].Value

$outPath = [IO.Path]::GetFullPath($Out)
New-Item -ItemType Directory -Force $outPath | Out-Null

# Bundle Microsoft's current x64 VC++ v14 runtime. Verify its Authenticode signature and
# use its exact version for Burn detection, so newer runtimes already on the PC are not downgraded.
$vcRedist = Join-Path $outPath "vc_redist.x64.exe"
Invoke-WebRequest -Uri "https://aka.ms/vc14/vc_redist.x64.exe" -OutFile $vcRedist
$vcSignature = Get-AuthenticodeSignature -LiteralPath $vcRedist
if ($vcSignature.Status -ne "Valid" -or
    $vcSignature.SignerCertificate.Subject -notmatch "Microsoft Corporation") {
    throw "Downloaded VC++ redistributable does not have a valid Microsoft Authenticode signature"
}
$vcVersionText = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($vcRedist).ProductVersion
$vcVersionMatch = [regex]::Match($vcVersionText, '(\d+)\.(\d+)\.(\d+)\.(\d+)')
if (-not $vcVersionMatch.Success) { throw "Cannot read VC++ redistributable product version: $vcVersionText" }
$vcVersion = [version]$vcVersionMatch.Groups[0].Value
if ($vcVersion.Major -ne 14) { throw "Unexpected VC++ redistributable major version: $vcVersion" }
$payload = Join-Path $outPath "payload"
# Only remove this builder's payload directory after resolving it within the output root.
if ([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($payload)) -ne $outPath) { throw "unsafe payload path" }
Remove-Item -LiteralPath $payload -Recurse -Force -ErrorAction SilentlyContinue
$resources = Join-Path $payload "resources"
$kit = Join-Path $resources "prep-kit"
New-Item -ItemType Directory -Force (Join-Path $resources "assets"), (Join-Path $resources "licenses"), (Join-Path $resources "fonts") | Out-Null
foreach ($name in $runtimeNames) { Copy-Item (Join-Path $release $name) $payload }
# DX12 compiles shaders with this DXC beside the client; without it the client falls back to FXC.
& (Join-Path $PSScriptRoot "fetch-dxc.ps1") -Root $root -Out $payload
& (Join-Path $PSScriptRoot "stage-prep-kit.ps1") -Root $root -Kit $kit -Assetc (Join-Path $release "assetc.exe")
Copy-Item (Join-Path $root "crates/assets/data/block-physics-v2225.bin") (Join-Path $resources "assets")
Copy-Item (Join-Path $root "THIRD_PARTY_NOTICES.md") (Join-Path $resources "assets")
Copy-Item (Join-Path $root "LICENSE") (Join-Path $resources "licenses/Cinnabar-LICENSE.md")
Copy-Item (Join-Path $root "assets/licenses/*") (Join-Path $resources "licenses")
$fontSource = Get-Content -Raw (Join-Path $root "assets/cinnangles-sans-source.json") | ConvertFrom-Json
$font = Join-Path $root "assets/fonts/$($fontSource.font_file)"
if (-not (Test-Path $font)) { throw "missing $font" }
Copy-Item $font (Join-Path $resources "fonts")

$icon = Join-Path $outPath "cinnabar.ico"
magick -background none -density 384 (Join-Path $PSScriptRoot "../icons/cinnabar.svg") -define icon:auto-resize=256,128,64,48,32,16 $icon
if ($LASTEXITCODE -ne 0) { throw "icon rendering failed" }
$logo = Join-Path $outPath "cinnabar.png"
magick -background none (Join-Path $PSScriptRoot "../icons/cinnabar.svg") -resize 64x64 $logo
if ($LASTEXITCODE -ne 0) { throw "logo rendering failed" }
& (Join-Path $PSScriptRoot "set-executable-icon.ps1") -Executable (Join-Path $payload "cinnabar.exe") -Icon $icon

$signPaths = @($runtimeNames | ForEach-Object { Join-Path $payload $_ }) + @(Join-Path $kit "bin/assetc.exe")
& (Join-Path $PSScriptRoot "execution-level.ps1") -Executable $signPaths -Embed
& (Join-Path $PSScriptRoot "sign.ps1") -Path $signPaths
$msi = Join-Path $outPath "Cinnabar-$version-x64.msi"
wix build -arch x64 -d Version=$version -d Payload=$payload -d Icon=$icon -o $msi (Join-Path $PSScriptRoot "cinnabar.wxs")
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }
& (Join-Path $PSScriptRoot "sign.ps1") -Path $msi
$setup = Join-Path $outPath "Cinnabar-$version-x64-setup.exe"
wix build -arch x64 -ext WixToolset.BootstrapperApplications.wixext -ext WixToolset.Util.wixext ` -d Version=$version -d Msi=$msi -d VcRedist=$vcRedist ` -d VcRedistMajor=$($vcVersion.Major) -d VcRedistMinor=$($vcVersion.Minor) ` -d VcRedistBuild=$($vcVersion.Build) -d VcRedistRevision=$($vcVersion.Revision) ` -d Icon=$icon -d Logo=$logo -o $setup (Join-Path $PSScriptRoot "setup.wxs")
if ($LASTEXITCODE -ne 0) { throw "setup bundle build failed" }
& (Join-Path $PSScriptRoot "execution-level.ps1") -Executable $setup
$engine = Join-Path $outPath "setup-engine.exe"
wix burn detach $setup -engine $engine
if ($LASTEXITCODE -ne 0) { throw "setup engine detach failed" }
& (Join-Path $PSScriptRoot "execution-level.ps1") -Executable $engine
if ($env:WINDOWS_CERT_PFX_BASE64) {
    # Burn caches its detached engine; sign that executable as well as the whole bundle.
    & (Join-Path $PSScriptRoot "sign.ps1") -Path $engine
    $signed = Join-Path $outPath "setup-signed.exe"
    wix burn reattach $setup -engine $engine -o $signed
    if ($LASTEXITCODE -ne 0) { throw "setup engine reattach failed" }
    Move-Item -LiteralPath $signed -Destination $setup -Force
}
& (Join-Path $PSScriptRoot "sign.ps1") -Path $setup
$bundleCheck = Join-Path $outPath "bundle-check"
& (Join-Path $PSScriptRoot "check-setup.ps1") -Setup $setup -Msi $msi -Out $bundleCheck
if ($LASTEXITCODE -ne 0) { throw "setup bundle verification failed" }
Remove-Item -LiteralPath $bundleCheck -Recurse -Force
Write-Output $msi
Write-Output $setup
