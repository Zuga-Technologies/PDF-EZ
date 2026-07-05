# Fetches Tesseract OCR (Apache-2.0, UB Mannheim build) into src-tauri/tesseract.
# Extracts the NSIS installer with 7-Zip — nothing is installed system-wide.
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$dest = Join-Path $root "src-tauri\tesseract"
$rel = Invoke-RestMethod https://api.github.com/repos/UB-Mannheim/tesseract/releases/latest
$asset = $rel.assets | Where-Object name -match 'w64-setup.*\.exe$' | Select-Object -First 1
$setup = Join-Path $env:TEMP "tess-setup.exe"
Invoke-WebRequest $asset.browser_download_url -OutFile $setup
$x = Join-Path $env:TEMP "tess-extract"
& "C:\Program Files\7-Zip\7z.exe" x -y -o"$x" $setup | Out-Null
New-Item -ItemType Directory -Force "$dest\tessdata\configs" | Out-Null
Copy-Item "$x\tesseract.exe" $dest
Copy-Item "$x\*.dll" $dest
Copy-Item "$x\tessdata\eng.traineddata","$x\tessdata\osd.traineddata","$x\tessdata\pdf.ttf" "$dest\tessdata\"
Copy-Item "$x\tessdata\configs\*" "$dest\tessdata\configs\"
Remove-Item $x -Recurse -Force -Confirm:$false
Write-Host "tesseract $($rel.tag_name) -> $dest"
