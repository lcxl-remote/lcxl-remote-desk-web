#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "Building Rust server (lcxl-remote-desk-server)..."
cargo build -p lcxl-remote-desk-server

echo "Building Vite project..."
pushd "$SCRIPT_DIR/vite-project" > /dev/null
npm install
npm run ci
popd > /dev/null

echo "Copying static files to target directory..."
TARGET_DIR="$SCRIPT_DIR/target/debug"
STATIC_DIR="$TARGET_DIR/static"

rm -rf "$STATIC_DIR"
mkdir -p "$STATIC_DIR"
cp -r dist/* "$STATIC_DIR/"

echo "Build and copy complete. Executable and static/ are in $TARGET_DIR"
