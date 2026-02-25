#!/bin/bash

# Default image name
IMAGE_NAME="lcxl/lcxl-remote-desk-web"
TAG="latest"
PUSH=false
MIRROR=false

# Parse arguments
while [[ "$#" -gt 0 ]]; do
    case $1 in
        --push) PUSH=true ;;
        --mirror) MIRROR=true ;;
        *) 
            # If it doesn't start with --, assume it's the tag or full image name
            if [[ "$1" == *":"* ]]; then
                FULL_IMAGE_NAME="$1"
            else
                TAG="$1"
            fi
            ;;
    esac
    shift
done

# Resolve full image name
if [ -z "$FULL_IMAGE_NAME" ]; then
    FULL_IMAGE_NAME="${IMAGE_NAME}:${TAG}"
fi

echo "Building Docker image: ${FULL_IMAGE_NAME}"

# Enable Docker BuildKit
export DOCKER_BUILDKIT=1

BUILD_ARGS=()
if [ "$MIRROR" = true ]; then
    echo "Using Cargo mirror (aliyun)"
    BUILD_ARGS+=(--build-arg ENABLE_MIRROR=true)
fi

# Build the image
docker build "${BUILD_ARGS[@]}" -t "${FULL_IMAGE_NAME}" .

# Check build status
if [ $? -eq 0 ]; then
    echo "Build successful: ${FULL_IMAGE_NAME}"
    
    # Push if requested
    if [ "$PUSH" = true ]; then
        echo "Pushing image: ${FULL_IMAGE_NAME}"
        docker push "${FULL_IMAGE_NAME}"
    fi
else
    echo "Build failed!"
    exit 1
fi
