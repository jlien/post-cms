#!/usr/bin/env bash
# Build the Rust workspace and render dist/.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --workspace
cargo run -p build
echo "build done → dist/"
