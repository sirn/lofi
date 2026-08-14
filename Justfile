default:
    @just --list

check: fmt lint test

fmt:
    cargo fmt --all --check

lint:
    cargo clippy --workspace --all-targets --offline -- -D warnings

test:
    cargo test --workspace --offline

e2e:
    cargo test -p lofi --test e2e --offline
