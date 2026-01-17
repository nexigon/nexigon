# spellchecker:ignore clippy extglob taplo elif shopt

# Pass on positional arguments.
set positional-arguments

# Default Rust target.
export DEFAULT_TARGET := `rustc --version --verbose | grep -o "host:.*" | awk '{print $2}'`

# List the available recipes.
[private]
default:
    @just --list

# Linting and check formatting.
check:
    cargo deny check
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    taplo fmt --check

# Format all files.
fmt:
    cargo fmt
    taplo fmt

# Generate documentation.
doc:
    cargo +nightly doc --all-features --document-private-items

# Generate code.
generate:
    cd crates/libs/nexigon-api && sidex generate rust src/types/generated
    cd crates/libs/nexigon-api && sidex generate json-schema ../../../build/api-json-schema
    cd crates/apps/nexigon-cli && sidex generate rust src/config/generated
    cd crates/apps/nexigon-cli && sidex generate json-schema ../../../build/cli-json-schema
    cd crates/apps/nexigon-agent && sidex generate rust src/config/generated
    cd crates/apps/nexigon-agent && sidex generate json-schema ../../../build/agent-json-schema
    mv build/api-json-schema/schema-defs.json crates/utils/nexigon-gen-openapi/schemas.json
    mv build/cli-json-schema/nexigon_cli.config.Config.schema.json schemas/nexigon-cli.schema.json
    mv build/agent-json-schema/nexigon_agent.config.Config.schema.json schemas/nexigon-agent.schema.json
    cargo run --bin nexigon-gen-openapi >api/openapi.json
    just fmt

# Run tests.
test:
    cargo test

# Build binaries.
build TARGET=DEFAULT_TARGET CROSS="false":
    #!/usr/bin/env bash
    TARGET="{{ TARGET }}"
    CROSS="{{ CROSS }}"
    if [ "$CROSS" == "true" ]; then
        cross build --locked --release --bins --target "$TARGET"
    else
        cargo build --locked --release --bins --target "$TARGET"
    fi
    mkdir -p build/binaries
    shopt -s extglob
    cd "target/{{ TARGET }}/release"
    tar -czf "../../../build/binaries/{{ TARGET }}.tar.gz" nexigon-+([a-z])?(.exe)

# Run Nexigon CLI.
run-cli *args:
    cargo run --bin nexigon-cli -- "$@"

# Run Nexigon Agent.
run-agent *args:
    cargo run --bin nexigon-agent -- "$@"
