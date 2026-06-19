#!/bin/bash
set -e
# Regenerate the typed frontend client from the desk-server OpenAPI spec.
#
# The spec is dumped offline via the `dump-openapi` subcommand — it is built
# purely from the route registration (`configure_api_surface`), so no running
# server / DB / Redis / HTTP is required. Works locally and in CI.
#
# Fix the working directory to this script's location so the spec and Kubb
# config resolve correctly regardless of where the script is invoked from.
cd "$(dirname "$0")"

echo "Dumping OpenAPI spec (offline)..."
cargo run -q -p lcxl-remote-desk-server -- dump-openapi --out openapi.json
echo "Generating Kubb clients..."
npx kubb generate
