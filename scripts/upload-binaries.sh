#!/usr/bin/env bash

set -euo pipefail

cd -- "$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

run_cli() {
    if [[ -n "${NEXIGON_CLI:-}" ]]; then
        "$NEXIGON_CLI" "$@"
    else
        mise run run-cli -- "$@"
    fi
}

shopt -s nullglob
archives=(build/binaries/*.tar.gz)
if [[ ${#archives[@]} -eq 0 ]]; then
    echo "No binary archives found in build/binaries. Build them before uploading." >&2
    exit 1
fi

TIMESTAMP=$(date +"%Y%m%d%H%M%S")

GIT_COMMIT=$(git rev-parse --short HEAD)
GIT_VERSION="git-$GIT_COMMIT"

BUILD_VERSION=${BUILD_VERSION:-"build-$TIMESTAMP-$GIT_COMMIT"}

DESCRIBED_VERSION=$(git describe --tags --always)


function upload_binaries() {
    local build_version_info version_id filename target asset_info asset_id archive uploaded

    local crate=$1
    # Check whether the build version already exists. Otherwise, create it.
    build_version_info=$(run_cli repositories versions resolve "nexigon-downloads/$crate/$BUILD_VERSION")
    if [ "$(echo "$build_version_info" | jq -r '.result')" == "Found" ]; then
        echo "Build version already exists, updating it."
        version_id=$(echo "$build_version_info" | jq -er '.versionId')
    else
        echo "Build version does not exist, creating it."
        version_id=$(run_cli repositories versions create "nexigon-downloads/$crate" --tag "$BUILD_VERSION,locked" | jq -er '.versionId')
    fi

    echo "VERSION_ID=$version_id"
    echo "DESCRIBED_VERSION=$DESCRIBED_VERSION"

    for archive in "${archives[@]}"; do
        uploaded=false
        filename=$(basename "$archive")
        target=${filename%.tar.gz}
        mkdir -p "build/binaries/$target"
        if tar -xzf "$archive" -C "build/binaries/$target" "$crate" 2>/dev/null; then
            uploaded=true
            asset_info=$(run_cli repositories assets upload nexigon-downloads "build/binaries/$target/$crate")
            asset_id=$(echo "$asset_info" | jq -er '.assetId')
            run_cli repositories versions assets add "$version_id" "$asset_id" "$target/$crate"
        fi
        if tar -xzf "$archive" -C "build/binaries/$target" "$crate.exe" 2>/dev/null; then
            uploaded=true
            asset_info=$(run_cli repositories assets upload nexigon-downloads "build/binaries/$target/$crate.exe")
            asset_id=$(echo "$asset_info" | jq -er '.assetId')
            run_cli repositories versions assets add "$version_id" "$asset_id" "$target/$crate.exe"
        fi
        if [[ "$uploaded" != true ]]; then
            echo "Archive $archive contains neither $crate nor $crate.exe." >&2
            exit 1
        fi
    done

    local tag_args=(
        --tag "$GIT_VERSION,reassign"
        --tag "$DESCRIBED_VERSION,reassign"
    )

    # For proper releases (e.g., v0.5.0), also reassign v$major and v$major.$minor tags.
    if [[ "$DESCRIBED_VERSION" =~ ^v([0-9]+)\.([0-9]+)\.[0-9]+$ ]]; then
        tag_args+=(--tag "latest,reassign")
        tag_args+=(--tag "v${BASH_REMATCH[1]},reassign")
        tag_args+=(--tag "v${BASH_REMATCH[1]}.${BASH_REMATCH[2]},reassign")
    fi

    run_cli repositories versions tag "$version_id" "${tag_args[@]}"
}

upload_binaries "nexigon-agent"
upload_binaries "nexigon-cli"
