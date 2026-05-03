$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
Set-Location $ScriptDir

Write-Host "Building Rust server (lcxl-remote-desk-server)..."
cargo build -p lcxl-remote-desk-server

Write-Host "Building Vite project..."
Push-Location "$ScriptDir\vite-project"
# npm ci
npm run build
Pop-Location

Write-Host "Copying static files to target directory..."
$TargetDir = "$ScriptDir\target\debug"
$StaticDir = "$TargetDir\static"

if (Test-Path $StaticDir) {
    Remove-Item -Recurse -Force $StaticDir
}
New-Item -ItemType Directory -Force -Path $StaticDir | Out-Null
Copy-Item -Path "$ScriptDir\vite-project\dist\*" -Destination $StaticDir -Recurse -Force

Write-Host "Build and copy complete. Executable and static/ are in $TargetDir"
