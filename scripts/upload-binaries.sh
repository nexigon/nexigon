#!/usr/bin/env bash

set -euo pipefail

NEXIGON_CLI=${NEXIGON_CLI:-"just run-cli"}

TIMESTAMP=$(date +"%Y%m%d%H%M%S")

GIT_COMMIT=$(git rev-parse --short HEAD)
GIT_VERSION="git-$GIT_COMMIT"

BUILD_VERSION=${BUILD_VERSION:-"build-$TIMESTAMP-$GIT_COMMIT"}

CARGO_METADATA=$(cargo metadata --no-deps --format-version 1)


function upload_binaries() {
    local build_version_info version_id filename target asset_info asset_id \
        crate_version crate_version_minor crate_version_major

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

    crate_version=$(echo "$CARGO_METADATA" | jq -r ".packages[] | select(.name == \"$crate\") | .version")
    crate_version_major="${crate_version%%.*}"
    crate_version_minor="${crate_version%.*}"

    echo "VERSION_ID=$version_id"
    echo "CRATE_VERSION=$crate_version"

    for archive in build/binaries/*.tar.gz; do
        filename=$(basename "$archive")
        target=${filename%.tar.gz}
        mkdir -p "build/binaries/$target"
        tar -xzf "$archive" -C "build/binaries/$target" "$crate"
        asset_info=$($NEXIGON_CLI repositories assets upload nexigon-downloads "build/binaries/$target/$crate")
        asset_id=$(echo "$asset_info" | jq -r '.assetId')
        $NEXIGON_CLI repositories versions assets add "$version_id" "$asset_id" "$target/$crate"
    done

    $NEXIGON_CLI repositories versions tag "$version_id" \
        --tag "$GIT_VERSION,reassign" \
        --tag "v$crate_version,reassign" \
        --tag "v$crate_version_major,reassign" \
        --tag "v$crate_version_minor,reassign"
}

upload_binaries "nexigon-agent"
upload_binaries "nexigon-cli"
