#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repository_root"

cargo fmt
taplo fmt
for directory in \
    crates/libs/nexigon-api \
    crates/libs/nexigon-agent-api \
    crates/libs/nexigon-agent-protocol \
    crates/apps/nexigon-cli \
    crates/apps/nexigon-agent; do
    (
        cd -- "$directory"
        sidex fmt
    )
done
