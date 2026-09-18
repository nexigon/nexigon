#!/usr/bin/env bash

set -euo pipefail

REPOSITORY_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
OUTPUT_DIR=${1:-"$REPOSITORY_ROOT/build/release"}

if ! cargo cyclonedx --version >/dev/null 2>&1; then
    echo "cargo-cyclonedx is required to prepare release assets." >&2
    exit 1
fi

mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR=$(cd "$OUTPUT_DIR" && pwd)

mapfile -t existing_sboms < <(
    find "$REPOSITORY_ROOT/crates" -type f -name '*.cdx.json' -print
)
if ((${#existing_sboms[@]} > 0)); then
    echo "Refusing to overwrite existing CycloneDX files in the source tree." >&2
    exit 1
fi

generated_sboms=()
cleanup() {
    mapfile -t generated_sboms < <(
        find "$REPOSITORY_ROOT/crates" -type f -name '*.cdx.json' -print
    )
    if ((${#generated_sboms[@]} > 0)); then
        rm -f -- "${generated_sboms[@]}"
    fi
}
trap cleanup EXIT

export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$REPOSITORY_ROOT" log -1 --format=%ct)}

cd "$REPOSITORY_ROOT"
cargo cyclonedx \
    --format json \
    --all \
    --target all \
    --spec-version 1.5

for binary in nexigon-agent nexigon-cli; do
    source_file="$REPOSITORY_ROOT/crates/apps/$binary/$binary.cdx.json"
    if [[ ! -f "$source_file" ]]; then
        echo "cargo-cyclonedx did not produce $source_file." >&2
        exit 1
    fi
    install -m 0644 "$source_file" "$OUTPUT_DIR/$binary.cdx.json"
done

mapfile -t release_assets < <(
    find "$OUTPUT_DIR" -maxdepth 1 -type f ! -name SHA256SUMS -printf '%f\n' |
        LC_ALL=C sort
)
if ((${#release_assets[@]} == 0)); then
    echo "No release assets found in $OUTPUT_DIR." >&2
    exit 1
fi

(
    cd "$OUTPUT_DIR"
    sha256sum -- "${release_assets[@]}"
) >"$OUTPUT_DIR/SHA256SUMS"
