#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repository_root"

for script in installer/*.sh scripts/*.sh; do
    bash -n "$script"
done
installer/test-install-hub.sh
shellcheck scripts/check.sh scripts/fmt.sh scripts/fix.sh scripts/generate-api.sh scripts/build-binaries.sh scripts/upload-binaries.sh
cargo deny check
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
taplo fmt --check

for directory in \
    crates/libs/nexigon-api \
    crates/libs/nexigon-agent-api \
    crates/libs/nexigon-agent-protocol \
    crates/apps/nexigon-cli \
    crates/apps/nexigon-agent; do
    (
        cd -- "$directory"
        sidex fmt --check
        sidex check
    )
done
