default:
    @just --list

build:
    cargo build --release --locked

test:
    cargo nextest run --locked

lint:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings

# Format check, tests and lints, as the release workflow runs them
check:
    cargo fmt --check
    cargo test --locked
    cargo clippy --locked --all-targets -- -D warnings

# Sign and publish the draft release CI created for a tag (e.g. v0.2.0)
sign-release tag:
    scripts/sign-release.sh {{tag}}
