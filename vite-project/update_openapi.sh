#!/bin/bash
set -e
# 检测 8081 端口
if ! curl -s -f http://localhost:8081/openapi.json > /dev/null; then
    echo "Error: Could not connect to localhost:8081. Please make sure desk server is running."
    exit 1
fi

echo "Fetching OpenAPI JSON..."
curl http://localhost:8081/openapi.json -o openapi.json
echo "Generating Kubb clients..."
npx kubb generate
