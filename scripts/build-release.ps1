# Builds everything and stages dist\app for the installer:  scripts\build-release.ps1 [-Version 0.1.0] [-Installer]
param([string]$Version = "0.1.0", [switch]$Installer)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

# toolchain (per-user installs, see scripts/dev-env.sh)
$env:LIBCLANG_PATH = Join-Path $env:LOCALAPPDATA "Programs\llvm\clang+llvm-23.1.3-x86_64-pc-windows-msvc\bin"
$cmake = "C:\Program Files\Microsoft Visual Studio\2022\Community\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
$dotnet = Join-Path $env:LOCALAPPDATA "Microsoft\dotnet"
$env:PATH = "$cmake;$dotnet;$env:PATH"
$env:DOTNET_ROOT = $dotnet

# CPU build (also the fallback when a machine has no Vulkan runtime) + client DLL
cargo build --release -p medialake-ffi -p medialake-server
if ($LASTEXITCODE) { throw "cargo build failed" }
Copy-Item (Join-Path $root "target\release\medialake-server.exe") (Join-Path $root "target\release\medialake-server-cpu.exe") -Force

# GPU (Vulkan) build, if the Vulkan SDK is installed. It needs the VS developer environment and a short
# target directory; the cmake sub-build of vulkan-shaders-gen occasionally loses a race: retry once.
$vk = Get-ChildItem "C:\VulkanSDK" -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending | Select-Object -First 1
$gpuExe = $null
if ($vk) {
    $bat = Join-Path $env:TEMP "medialake-vkbuild.bat"
    @"
@echo off
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat" >nul
set "LIBCLANG_PATH=$env:LIBCLANG_PATH"
set "VULKAN_SDK=$($vk.FullName)"
set "PATH=$cmake;$($vk.FullName)\Bin;%PATH%"
set "CARGO_TARGET_DIR=C:\t"
cd /d $root
cargo build --release -p medialake-server --features vulkan
"@ | Set-Content $bat -Encoding ASCII
    cmd /c $bat
    if ($LASTEXITCODE) { cmd /c $bat }
    if ($LASTEXITCODE -eq 0) { $gpuExe = "C:\t\release\medialake-server.exe" } else { Write-Warning "Vulkan build failed; shipping the CPU server only" }
}

$out = Join-Path $root "dist\app"
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
# `dotnet publish` drops the compiled XAML (.xbf) and MediaLake.pri for unpackaged WinUI apps; the
# self-contained build output is complete, so stage that.
dotnet build ui\MediaLake.App\MediaLake.App.csproj -c Release -p:Platform=x64
if ($LASTEXITCODE) { throw "dotnet build failed" }
$bin = Join-Path $root "ui\MediaLake.App\bin\x64\Release\net10.0-windows10.0.19041.0\win-x64"
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item "$bin\*" $out -Recurse -Force -Exclude "*.pdb", "selftest.log"

Copy-Item (Join-Path $root "target\release\medialake_core_ffi.dll") $out -Force
Copy-Item (Join-Path $root "target\release\medialake-server-cpu.exe") $out -Force
if ($gpuExe) { Copy-Item $gpuExe (Join-Path $out "medialake-server.exe") -Force }
else { Copy-Item (Join-Path $root "target\release\medialake-server.exe") (Join-Path $out "medialake-server.exe") -Force }
# WinAppSDK ships ~190 resource folders for languages we do not translate; keep English and German
Get-ChildItem $out -Directory | Where-Object { $_.Name -match '^[a-z]{2,3}(-[A-Za-z0-9]+)+$' -and $_.Name -notin @('en-US', 'de-DE') } | Remove-Item -Recurse -Force
$mb = [math]::Round((Get-ChildItem $out -Recurse | Measure-Object Length -Sum).Sum / 1MB)
Write-Host "staged $out ($mb MB)"

if ($Installer) {
    $iscc = Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"
    & $iscc "/DAppVersion=$Version" installer\medialake.iss
    if ($LASTEXITCODE) { throw "ISCC failed" }
}
