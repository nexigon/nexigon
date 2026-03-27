#!/usr/bin/env bash

set -euo pipefail

NEXIGON_CLI=${NEXIGON_CLI:-"just run-cli"}

TIMESTAMP=$(date +"%Y%m%d%H%M%S")

GIT_COMMIT=$(git rev-parse --short HEAD)
GIT_VERSION="git-$GIT_COMMIT"

BUILD_VERSION=${BUILD_VERSION:-"build-$TIMESTAMP-$GIT_COMMIT"}

DESCRIBED_VERSION=$(git describe --tags --always)


function upload_binaries() {
    local build_version_info version_id filename target asset_info asset_id

    local crate=$1
    # Check whether the build version already exists. Otherwise, create it.
    build_version_info=$($NEXIGON_CLI repositories versions resolve "nexigon-downloads/$crate/$BUILD_VERSION")
    if [ "$(echo "$build_version_info" | jq -r '.result')" == "Found" ]; then
        echo "Build version already exists, updating it."
        version_id=$(echo "$build_version_info" | jq -r '.versionId')
    else
        echo "Build version does not exist, creating it."
        version_id=$($NEXIGON_CLI repositories versions create "nexigon-downloads/$crate" --tag "$BUILD_VERSION,locked" | jq -r '.versionId')
    fi

    echo "VERSION_ID=$version_id"
    echo "DESCRIBED_VERSION=$DESCRIBED_VERSION"

    for archive in build/binaries/*.tar.gz; do
        filename=$(basename "$archive")
        target=${filename%.tar.gz}
        mkdir -p "build/binaries/$target"
        if tar -xzf "$archive" -C "build/binaries/$target" "$crate"; then
            asset_info=$($NEXIGON_CLI repositories assets upload nexigon-downloads "build/binaries/$target/$crate")
            asset_id=$(echo "$asset_info" | jq -r '.assetId')
            $NEXIGON_CLI repositories versions assets add "$version_id" "$asset_id" "$target/$crate"
        fi
        if tar -xzf "$archive" -C "build/binaries/$target" "$crate.exe"; then
            asset_info=$($NEXIGON_CLI repositories assets upload nexigon-downloads "build/binaries/$target/$crate.exe")
            asset_id=$(echo "$asset_info" | jq -r '.assetId')
            $NEXIGON_CLI repositories versions assets add "$version_id" "$asset_id" "$target/$crate.exe"
        fi
    done

    local tag_args=(
        --tag "$GIT_VERSION,reassign"
        --tag "$DESCRIBED_VERSION,reassign"
    )

    # For proper releases (e.g., v0.5.0), also reassign v$major and v$major.$minor tags.
    if [[ "$DESCRIBED_VERSION" =~ ^v([0-9]+)\.([0-9]+)\.[0-9]+$ ]]; then
        tag_args+=(--tag "v${BASH_REMATCH[1]},reassign")
        tag_args+=(--tag "v${BASH_REMATCH[1]}.${BASH_REMATCH[2]},reassign")
    fi

    $NEXIGON_CLI repositories versions tag "$version_id" "${tag_args[@]}"
}

upload_binaries "nexigon-agent"
upload_binaries "nexigon-cli"
