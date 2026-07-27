# Stage the pinned Gyan FFmpeg essentials build for the Windows installer.
# Essentials includes librubberband (required by Dubora voiceover tempo).
# Mirror: https://github.com/GyanD/codexffmpeg
# Catalog: https://www.gyan.dev/ffmpeg/builds/

$ErrorActionPreference = "Stop"

$Version = "8.1.2"
$ArchiveName = "ffmpeg-$Version-essentials_build.zip"
$Url = "https://github.com/GyanD/codexffmpeg/releases/download/$Version/$ArchiveName"
$ExpectedSha256 = "db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec"
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) "dubora-ffmpeg-$([guid]::NewGuid())"
$Archive = Join-Path $TempDir $ArchiveName
$Extracted = Join-Path $TempDir "extracted"
$Destination = Join-Path $PSScriptRoot "../apps/desktop/src-tauri/resources/ffmpeg"

New-Item -ItemType Directory -Force $TempDir, $Extracted, $Destination | Out-Null
Write-Host "Downloading $Url ..."
Invoke-WebRequest -Uri $Url -OutFile $Archive

$ActualSha256 = (Get-FileHash -Algorithm SHA256 $Archive).Hash.ToLowerInvariant()
if ($ActualSha256 -ne $ExpectedSha256) {
    throw "FFmpeg checksum mismatch: expected $ExpectedSha256, got $ActualSha256"
}

Expand-Archive -Path $Archive -DestinationPath $Extracted
$BuildRoot = Get-ChildItem -Path $Extracted -Directory | Select-Object -First 1
if (-not $BuildRoot) {
    throw "FFmpeg archive has no build directory"
}

Copy-Item (Join-Path $BuildRoot.FullName "bin/ffmpeg.exe") $Destination -Force
Copy-Item (Join-Path $BuildRoot.FullName "bin/ffprobe.exe") $Destination -Force
Copy-Item (Join-Path $BuildRoot.FullName "LICENSE") $Destination -Force
Copy-Item (Join-Path $BuildRoot.FullName "README.txt") $Destination -Force

# Fail early if the download ever drops rubberband.
$probe = & (Join-Path $Destination "ffmpeg.exe") -hide_banner -filters 2>&1 | Out-String
if ($probe -notmatch '(?m)^\s*\S+\s+rubberband\s+') {
    throw "Bundled FFmpeg is missing the rubberband filter"
}

Remove-Item -Recurse -Force $TempDir
Write-Host "Prepared FFmpeg $Version resources in $Destination"
