#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repository_root"

if [[ $# -gt 2 ]]; then
    echo "Usage: build-binaries.sh [TARGET] [true|false]" >&2
    exit 2
fi
target="${1:-}"
use_cross="${2:-false}"
if [[ -z "$target" ]]; then
    target="$(rustc --version --verbose | awk '$1 == "host:" { print $2 }')"
fi
if [[ ! "$target" =~ ^[A-Za-z0-9_-]+$ ]]; then
    echo "Expected a Rust target triple, got: $target" >&2
    exit 2
fi
case "$use_cross" in
    true|false) ;;
    *)
        echo "The cross argument must be true or false." >&2
        exit 2
        ;;
esac

export NEXIGON_GIT_VERSION="${NEXIGON_GIT_VERSION:-$(git describe --tags --always 2>/dev/null || echo unknown)}"
if [[ "$use_cross" == true ]]; then
    cross build --locked --release --bins --target "$target"
else
    cargo build --locked --release --bins --target "$target"
fi

suffix=""
if [[ "$target" == *-windows-* ]]; then
    suffix=".exe"
fi
binaries=("nexigon-agent$suffix" "nexigon-cli$suffix")
release_directory="$repository_root/target/$target/release"
for binary in "${binaries[@]}"; do
    if [[ ! -f "$release_directory/$binary" ]]; then
        echo "Expected release binary is missing: $release_directory/$binary" >&2
        exit 1
    fi
done

mkdir -p build/binaries
archive="$(mktemp "$repository_root/build/binaries/.$target.XXXXXX")"
trap 'rm -f -- "$archive"' EXIT
tar -czf "$archive" -C "$release_directory" "${binaries[@]}"
mv -- "$archive" "build/binaries/$target.tar.gz"
