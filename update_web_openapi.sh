#!/bin/bash
set -e
# This script is used to generate the OpenAPI JSON file for the web project.
# It assumes that the backend service is running on localhost:8081.
echo "Generating OpenAPI JSON file..."
cd "$(dirname "$0")/web"

curl http://localhost:8081/api/openapi.json -o config/openapi.json 
npm run openapi