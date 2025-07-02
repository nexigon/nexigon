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