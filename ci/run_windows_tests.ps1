$env:ErrorActionPreference='Stop'

# List of all the crates we want to build
# We need to do this manually to avoid trying
# to build the ones which can't work on windows
[string[]] $crates = @(
    # Same as default-members in Cargo.toml
    "tutorial",
    "version-helper",
    "audio/audiofx",
    "audio/claxon",
    "audio/lewton",
    "generic/file",
    "generic/fmp4",
    "generic/threadshare",
    "net/onvif",
    "net/raptorq",
    "net/reqwest",
    "net/aws",
    "utils/fallbackswitch",
    "utils/togglerecord",
    "utils/tracers",
    "utils/uriplaylistbin",
    "video/cdg",
    "video/ffv1",
    "video/flavors",
    "video/gif",
    "video/rav1e",
    "video/rspng",
    "video/hsv",
    "text/ahead",
    "text/wrap",
    "text/json",
    "text/regex",

    # Extra crates that can be built
    # "audio/csound",
    "audio/spotify",
    "generic/sodium",
    "net/hlssink3",
    "video/closedcaption",
    "video/dav1d",
    "video/gtk4",
    "video/videofx"
    # "video/webp",
)

foreach($crate in $crates)
{
    Write-Host "Building crate: $crate"
    Write-Host "Features: $env:FEATURES"
    $env:LocalFeatures = $env:FEATURES

    Write-Host "with features: $env:LocalFeatures"
    cargo build --color=always --manifest-path $crate/Cargo.toml --all-targets $env:LocalFeatures

    if (!$?) {
        Write-Host "Failed to build crate: $crate"
        Exit 1
    }

    $env:G_DEBUG="fatal_warnings"
    cargo test --no-fail-fast --color=always --manifest-path $crate/Cargo.toml --all-targets $env:LocalFeatures

    if (!$?) {
        Write-Host "Tests failed to for crate: $crate"
        Exit 1
    }
}
