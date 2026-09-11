#! /bin/bash

set -ex

rustc --version
cargo --version
cargo clippy --version

cpus=$(nproc || sysctl -n hw.ncpu)
CARGO_FLAGS="--color=always -j${FDO_CI_CONCURRENT:-$cpus}"

no_default_excludes="\
    --exclude gst-plugin-burn \
    --exclude gst-plugin-compress \
    --exclude gst-plugin-webrtc \
    --exclude gst-plugin-reqwest \
    --exclude gst-plugin-icecast \
    --exclude gst-plugin-rtsp \
    --exclude gst-plugin-quinn \
    --exclude gst-plugin-webrtc-signalling \
    --exclude gst-plugin-uriplaylistbin"

for cfg in "" "--all-features --exclude gst-plugin-gtk4 --exclude gst-plugin-whisper --exclude gst-plugin-llamacpp" "--no-default-features $no_default_excludes"; do
    cargo clippy $CARGO_FLAGS --locked --all --all-targets $cfg -- $CLIPPY_LINTS
done
