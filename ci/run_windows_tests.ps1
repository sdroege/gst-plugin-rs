$env:ErrorActionPreference='Stop'

$exclude_crates = @(
    "--exclude",
    "gst-plugin-csound",
    "--exclude",
    "gst-plugin-webp"
)

Write-Host "Features: $env:CI_CARGO_FEATURES"
Write-Host "Exlcude string: $exclude_crates"

cargo build --color=always --workspace $exclude_crates --all-targets $env:CI_CARGO_FEATURES

if (!$?) {
    Write-Host "Build failed"
    Exit 1
}

$env:G_DEBUG="fatal_warnings"
cargo test --no-fail-fast --color=always --workspace $exclude_crates --all-targets $env:CI_CARGO_FEATURES

if (!$?) {
    Write-Host "Tests failed"
    Exit 1
}
