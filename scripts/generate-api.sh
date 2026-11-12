#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$repository_root"

generated_directory="$(mktemp -d)"
trap 'rm -rf -- "$generated_directory"' EXIT

generate_schema() {
    local crate="$1"
    local output="$2"
    (
        cd -- "$crate"
        sidex generate json-schema "$output"
    )
}

generate_schema crates/libs/nexigon-api "$generated_directory/api"
generate_schema crates/apps/nexigon-cli "$generated_directory/cli"
generate_schema crates/apps/nexigon-agent "$generated_directory/agent"

mv -- "$generated_directory/api/schema-defs.json" crates/utils/nexigon-gen-openapi/schemas.json
mv -- "$generated_directory/cli/nexigon_cli.config.Config.schema.json" schemas/nexigon-cli.schema.json
mv -- "$generated_directory/agent/nexigon_agent.config.Config.schema.json" schemas/nexigon-agent.schema.json

cargo run --bin nexigon-gen-openapi > "$generated_directory/openapi.json"
mv -- "$generated_directory/openapi.json" api/openapi.json
exec bash scripts/fmt.sh
