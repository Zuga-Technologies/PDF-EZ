# Fetches the qpdf CLI (Apache-2.0) into src-tauri/qpdf/bin. Run once after
# cloning; binaries are not committed. Used by the Compress feature.
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$dest = Join-Path $root "src-tauri\qpdf\bin"
New-Item -ItemType Directory -Force $dest | Out-Null

$release = Invoke-RestMethod https://api.github.com/repos/qpdf/qpdf/releases/latest
$asset = $release.assets | Where-Object name -match 'msvc64\.zip$' | Select-Object -First 1
$zip = Join-Path $env:TEMP "qpdf.zip"
Invoke-WebRequest $asset.browser_download_url -OutFile $zip
$tmp = Join-Path $env:TEMP "qpdf-extract"
Expand-Archive $zip -DestinationPath $tmp -Force
Copy-Item (Join-Path (Get-ChildItem $tmp -Directory | Select-Object -First 1).FullName "bin\*") $dest
Remove-Item $tmp -Recurse -Force -Confirm:$false
Write-Host "qpdf $($release.tag_name) -> $dest"
