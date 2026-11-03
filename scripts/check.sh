#!/usr/bin/env sh
set -eu
cd "$(dirname "$0")/.."
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --locked
node tests/parity.cjs

node tests/upstream-stream.cjs
