#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repository_root"

cargo clippy --all-targets --all-features --fix --allow-dirty --allow-staged
for directory in \
    crates/libs/nexigon-api \
    crates/libs/nexigon-agent-api \
    crates/libs/nexigon-agent-protocol \
    crates/apps/nexigon-cli \
    crates/apps/nexigon-agent; do
    (
        cd -- "$directory"
        sidex check --fix
    )
done
exec bash scripts/fmt.sh
