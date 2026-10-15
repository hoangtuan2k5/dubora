# Pin the official yt-dlp release for the binary, checksums, and notices.
$ErrorActionPreference = "Stop"

$Version = "2026.08.19"
$Release = "https://github.com/yt-dlp/yt-dlp/releases/download/$Version"
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) "dubora-ytdlp-$([guid]::NewGuid())"
$Binary = Join-Path $TempDir "yt-dlp.exe"
$Checksums = Join-Path $TempDir "SHA2-256SUMS"
$Destination = Join-Path $PSScriptRoot "../apps/desktop/src-tauri/resources/yt-dlp"

New-Item -ItemType Directory -Force $TempDir, $Destination | Out-Null
Invoke-WebRequest -Uri "$Release/yt-dlp.exe" -OutFile $Binary
Invoke-WebRequest -Uri "$Release/SHA2-256SUMS" -OutFile $Checksums

$ChecksumLine = Select-String -Path $Checksums -Pattern '(^|\s)yt-dlp\.exe$' |
  Select-Object -First 1
if (-not $ChecksumLine) {
    throw "yt-dlp release checksum for yt-dlp.exe is missing"
}
$ExpectedSha256 = $ChecksumLine.Line.Split()[0].ToLowerInvariant()
$ActualSha256 = (Get-FileHash -Algorithm SHA256 $Binary).Hash.ToLowerInvariant()
if ($ActualSha256 -ne $ExpectedSha256) {
    throw "yt-dlp checksum mismatch: expected $ExpectedSha256, got $ActualSha256"
}

Copy-Item $Binary (Join-Path $Destination "yt-dlp.exe") -Force
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/yt-dlp/yt-dlp/$Version/LICENSE" -OutFile (Join-Path $Destination "LICENSE")
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/yt-dlp/yt-dlp/$Version/THIRD_PARTY_LICENSES.txt" -OutFile (Join-Path $Destination "THIRD_PARTY_LICENSES.txt")
"Version: $Version`nBinary SHA-256: $ActualSha256`nSource: https://github.com/yt-dlp/yt-dlp/tree/$Version" |
    Set-Content -Encoding utf8 (Join-Path $Destination "BUILD-INFO.txt")
Remove-Item -Recurse -Force $TempDir
Write-Host "Prepared verified yt-dlp resource in $Destination"
