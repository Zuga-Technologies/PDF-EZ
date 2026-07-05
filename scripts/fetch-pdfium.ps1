# Fetches the prebuilt PDFium binary (BSD-3, bblanchon/pdfium-binaries) into
# src-tauri/pdfium/bin/pdfium.dll. Run once after cloning; the DLL is not
# committed. Non-V8 build by design (no embedded JavaScript engine).
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$dest = Join-Path $root "src-tauri\pdfium"
New-Item -ItemType Directory -Force $dest | Out-Null

$release = Invoke-RestMethod https://api.github.com/repos/bblanchon/pdfium-binaries/releases/latest
$asset = $release.assets | Where-Object name -eq "pdfium-win-x64.tgz"
$tgz = Join-Path $env:TEMP "pdfium-win-x64.tgz"
Invoke-WebRequest $asset.browser_download_url -OutFile $tgz
tar -xzf $tgz -C $dest
Write-Host "PDFium $($release.tag_name) -> $dest\bin\pdfium.dll"
