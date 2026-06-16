<#
Package a standalone release build into dist\v<version>\.

The Rust binary is self-contained (egui, fonts, and all deps are linked in); the
only thing not bundled by default is the MSVC C runtime, so we statically link it
(+crt-static) to make the exe run on any Windows 10+ machine without a Visual C++
redistributable. Output is a single MusicPlayer.exe plus a run note, also zipped.

Usage:  pwsh -File scripts\package.ps1
#>
$ErrorActionPreference = "Stop"
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

# Statically link the CRT (scoped to this build only, not the dev profile).
$env:RUSTFLAGS = "-C target-feature=+crt-static"

$version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$dist = Join-Path $root "dist\v$version"

Write-Host "Building release (static CRT)..." -ForegroundColor Cyan
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }

if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory -Force -Path $dist | Out-Null

Copy-Item "target\release\music_player.exe" (Join-Path $dist "MusicPlayer.exe") -Force
Copy-Item "README.md" (Join-Path $dist "README.md") -Force

@"
MusicPlayer (v$version) - early preview build

Double-click MusicPlayer.exe to run. You can also drag an audio file onto it,
or run from a terminal:  MusicPlayer.exe "path\to\track.mp3"

Supported formats: mp3, wav, flac, m4a, aac
This is a single self-contained executable - no install, no DLLs required.
"@ | Set-Content (Join-Path $dist "HOW-TO-RUN.txt") -Encoding UTF8

$zip = Join-Path $root "dist\MusicPlayer-v$version.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path "$dist\*" -DestinationPath $zip

$exe = Get-Item (Join-Path $dist "MusicPlayer.exe")
Write-Host ("Packaged -> {0}" -f $dist) -ForegroundColor Green
Write-Host ("  MusicPlayer.exe  {0:N1} MB" -f ($exe.Length / 1MB))
Write-Host ("  zip -> {0}" -f $zip)
