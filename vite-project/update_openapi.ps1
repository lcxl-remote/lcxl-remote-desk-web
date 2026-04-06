$ErrorActionPreference = "Stop"
Write-Host "Fetching OpenAPI JSON..."
Invoke-WebRequest -Uri "http://localhost:8081/openapi.json" -OutFile "openapi.json"
Write-Host "Generating Kubb clients..."
npx kubb generate
