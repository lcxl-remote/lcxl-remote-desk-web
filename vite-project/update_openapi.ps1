$ErrorActionPreference = "Stop"
# 检测 8081 端口
if (-not (Test-NetConnection -ComputerName localhost -Port 8081 -InformationLevel Quiet)) {
    Write-Host "Error: Could not connect to localhost:8081. Please make sure desk server is running." -ForegroundColor Red
    exit 1
}

Write-Host "Fetching OpenAPI JSON..."
Invoke-WebRequest -Uri "http://localhost:8081/openapi.json" -OutFile "openapi.json"
Write-Host "Generating Kubb clients..."
npx kubb generate
