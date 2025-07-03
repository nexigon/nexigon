set positional-arguments

check:
    cargo deny check
    cargo +nightly fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    taplo fmt --check

fmt:
    cargo +nightly fmt
    taplo fmt

doc:
    cargo +nightly doc --all-features --document-private-items

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
    cd api && npx @redocly/cli build-docs openapi.json
    just fmt

run-cli *args:
    cargo run --bin nexigon-cli -- "$@"

run-agent *args:
    cargo run --bin nexigon-agent -- "$@"