source ./ci/env.sh

set -e
export CARGO_HOME='/usr/local/cargo'

# Install 0.9.4 until Rust 1.56 is the minimum supported version here
cargo install cargo-c --version 0.9.4+cargo-0.56
