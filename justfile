default:
    @just --list

build:
    cargo build --release --locked

test:
    cargo nextest run --locked

lint:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings
