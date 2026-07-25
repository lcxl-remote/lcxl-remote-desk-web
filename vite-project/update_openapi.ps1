$ErrorActionPreference = "Stop"
# Regenerate the typed frontend client from the desk-server OpenAPI spec.
#
# The spec is dumped offline via the `dump-openapi` subcommand — it is built
# purely from the route registration (`configure_api_surface`), so no running
# server / DB / Redis / HTTP is required. Works locally and in CI.
#
# Fix the working directory to this script's location so the spec and Kubb
# config resolve correctly regardless of where the script is invoked from.
Set-Location $PSScriptRoot

$specPath = Join-Path ([System.IO.Path]::GetTempPath()) (
    "lcxl-desk-openapi-{0}.json" -f [System.Guid]::NewGuid()
)
$previousInputPath = $env:KUBB_OPENAPI_PATH

try {
    # $ErrorActionPreference = "Stop" does not catch native (exe) non-zero
    # exits, so check $LASTEXITCODE after each command.
    Write-Host "Dumping OpenAPI spec (offline)..."
    cargo run -q -p lcxl-remote-desk-server -- dump-openapi --out $specPath
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    $env:KUBB_OPENAPI_PATH = $specPath
    Write-Host "Generating Kubb clients..."
    npx kubb generate
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
finally {
    if ($null -eq $previousInputPath) {
        Remove-Item Env:KUBB_OPENAPI_PATH -ErrorAction SilentlyContinue
    }
    else {
        $env:KUBB_OPENAPI_PATH = $previousInputPath
    }
    Remove-Item -LiteralPath $specPath -Force -ErrorAction SilentlyContinue
}
