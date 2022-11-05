source ./ci/env.sh

set -e
export CARGO_HOME='/usr/local/cargo'

cargo install cargo-c --version 0.9.14+cargo-0.66
