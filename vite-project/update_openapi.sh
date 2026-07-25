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

spec_dir="$(mktemp -d "${TMPDIR:-/tmp}/lcxl-desk-openapi.XXXXXX")"
spec_path="$spec_dir/openapi.json"
cleanup() {
    rm -f "$spec_path"
    rmdir "$spec_dir"
}
trap cleanup EXIT

echo "Dumping OpenAPI spec (offline)..."
cargo run -q -p lcxl-remote-desk-server -- dump-openapi --out "$spec_path"
echo "Generating Kubb clients..."
KUBB_OPENAPI_PATH="$spec_path" npx kubb generate
