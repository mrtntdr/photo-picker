# Builds dist\Photo Picker.exe (release build, tuned for this PC's CPU, icon embedded).
# Usage: powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1 [-Shortcut]
#   -Shortcut  also adds a "Photo Picker" entry to the Start menu.
param([switch]$Shortcut)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path -Parent $PSScriptRoot)

if (-not (Test-Path assets\icon.ico)) { & "$PSScriptRoot\make-ico.ps1" }

cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force dist | Out-Null
$exe = Join-Path (Resolve-Path dist) 'Photo Picker.exe'
Copy-Item target\release\photo-picker.exe $exe -Force

if ($Shortcut) {
    $lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'Photo Picker.lnk'
    $sh = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk)
    $sh.TargetPath = $exe
    $sh.WorkingDirectory = Split-Path $exe
    $sh.Save()
    Write-Host "Start menu shortcut: $lnk"
}
Write-Host "Built $exe"
