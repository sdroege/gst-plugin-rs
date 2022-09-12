$env:ErrorActionPreference='Stop'

[string[]] $exclude_crates = @(
    "--exclude",
    "gst-plugin-csound",
    "--exclude",
    "gst-plugin-webp"
)

[string[]] $features_matrix = @(
    "--no-default-features",
    "",
    "--all-features"
)

function Run-Tests {
    param (
        $Features
    )

    Write-Host "Features: $Features"
    Write-Host "Exclude string: $exclude_crates"

    cargo build --color=always --workspace $exclude_crates --all-targets $Features

    if (!$?) {
        Write-Host "Build failed"
        Exit 1
    }

    $env:G_DEBUG="fatal_warnings"
    cargo test --no-fail-fast --color=always --workspace $exclude_crates --all-targets $Features

    if (!$?) {
        Write-Host "Tests failed"
        Exit 1
    }
}

foreach($feature in $features_matrix) {
    Run-Tests -Features $feature
}
