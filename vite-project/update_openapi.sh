#!/bin/bash
set -e
echo "Fetching OpenAPI JSON..."
curl http://localhost:8081/openapi.json -o openapi.json
echo "Generating Kubb clients..."
npx kubb generate
