# Register pdez_shell.dll (PDF + Markdown thumbnails/preview pane) per-user.
# Idempotent. Run after installing PDF-EZ; -Unregister to remove.
param([switch]$Unregister)

$ErrorActionPreference = "Stop"
$installDir = "$env:LOCALAPPDATA\PDF-EZ"
$dll = Join-Path $installDir "pdez_shell.dll"
$thumbClsid = "{B7C0A2D4-8E11-4E5C-9F2A-01D6E4C3A9B1}"
$prevClsid  = "{B7C0A2D4-8E11-4E5C-9F2A-01D6E4C3A9B2}"
$thumbShellex = "{e357fccd-a995-4576-b01f-234630154e96}"   # IThumbnailProvider
$prevShellex  = "{8895b1c6-b41f-4c1c-a562-0d564250836f}"   # IPreviewHandler
$prevHostAppId = "{534A1E02-D58F-44f0-B58B-36CBED287C7C}"  # prevhost.exe surrogate
$classes = "HKCU:\Software\Classes"

if ($Unregister) {
    foreach ($k in @(
        "$classes\CLSID\$thumbClsid", "$classes\CLSID\$prevClsid",
        "$classes\PDF Document\shellex", "$classes\.pdf\shellex",
        "$classes\Markdown Document\shellex", "$classes\.md\shellex", "$classes\.markdown\shellex"
    )) { Remove-Item $k -Recurse -Force -ErrorAction SilentlyContinue }
    Remove-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\PreviewHandlers" `
        -Name $prevClsid -ErrorAction SilentlyContinue
    Write-Host "unregistered"
    exit 0
}

if (-not (Test-Path $dll)) { throw "$dll not found — build shellext and copy it there first" }

# COM classes
foreach ($pair in @(@($thumbClsid, "PDF-EZ Thumbnail Provider"), @($prevClsid, "PDF-EZ Preview Handler"))) {
    $clsid, $name = $pair
    New-Item "$classes\CLSID\$clsid\InprocServer32" -Force | Out-Null
    Set-ItemProperty "$classes\CLSID\$clsid" -Name "(default)" -Value $name
    Set-ItemProperty "$classes\CLSID\$clsid\InprocServer32" -Name "(default)" -Value $dll
    Set-ItemProperty "$classes\CLSID\$clsid\InprocServer32" -Name "ThreadingModel" -Value "Apartment"
}
# Preview handler runs in the prevhost surrogate
Set-ItemProperty "$classes\CLSID\$prevClsid" -Name "AppID" -Value $prevHostAppId
Set-ItemProperty "$classes\CLSID\$prevClsid" -Name "DisplayName" -Value "PDF-EZ Preview"

# Hook both each format's PDF-EZ ProgID and its bare extension, so previews
# work even while another app still owns the default for that extension. Both
# document types share the same two CLSIDs — the DLL content-sniffs the
# stream (md_engine::looks_like_pdf) to pick the right renderer.
foreach ($base in @(
    "$classes\PDF Document", "$classes\.pdf",
    "$classes\Markdown Document", "$classes\.md", "$classes\.markdown"
)) {
    New-Item "$base\shellex\$thumbShellex" -Force | Out-Null
    Set-ItemProperty "$base\shellex\$thumbShellex" -Name "(default)" -Value $thumbClsid
    New-Item "$base\shellex\$prevShellex" -Force | Out-Null
    Set-ItemProperty "$base\shellex\$prevShellex" -Name "(default)" -Value $prevClsid
}

New-Item "HKCU:\Software\Microsoft\Windows\CurrentVersion\PreviewHandlers" -Force | Out-Null
Set-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\PreviewHandlers" `
    -Name $prevClsid -Value "PDF-EZ PDF Preview"

Write-Host "registered $dll"
