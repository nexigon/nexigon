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
    mv build/api-json-schema/schema-defs.json crates/utils/nexigon-gen-openapi/schemas.json
    cargo run --bin nexigon-gen-openapi >api/openapi.json
    cd api && npx @redocly/cli build-docs openapi.json
    just fmt