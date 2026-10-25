# Build the release binaries and pack dist/gantry-<version>-x86_64-windows.zip and dist/gantry-<version>-x86_64-windows.msi
# (run on Windows, by hand or by .github/workflows/release.yml). The MSI needs the WiX 5 tool: `dotnet tool install --global wix --version 5.0.2`.
$ErrorActionPreference = 'Stop'
$root = Resolve-Path "$PSScriptRoot\..\.."
Set-Location $root
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"$' | Select-Object -First 1).Matches[0].Groups[1].Value

cargo build --release -p gantry
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
$stage = Join-Path $root "dist\gantry-$version"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item "$target\release\gantry.exe", "$target\release\gantryd.exe", packaging\windows\gantry.ico, `
  LICENSE-MIT, LICENSE-APACHE, NOTICE, THIRD_PARTY_LICENSES.html $stage
# The embedded interface font's licence.
Copy-Item crates\ui-gpui\assets\fonts\OFL.txt "$stage\LICENSE-FONT-FIGTREE"

$zip = Join-Path $root "dist\gantry-$version-x86_64-windows.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip

$msi = Join-Path $root "dist\gantry-$version-x86_64-windows.msi"
wix build packaging\windows\gantry.wxs -arch x64 -d "Version=$version" -d "Stage=$stage" -o $msi
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }
Remove-Item -Force ([System.IO.Path]::ChangeExtension($msi, '.wixpdb')) -ErrorAction SilentlyContinue

$zip
$msi
