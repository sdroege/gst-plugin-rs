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
    $local_exclude = $exclude_crates;

    # In this case the plugin will pull x11/wayland features
    # which will fail to build on windows.
    if (($Features -eq '--all-features') -or ($Features -eq '')) {
        $local_exclude += @("--exclude", "gst-plugin-gtk4")
    }

    Write-Host "Features: $Features"
    Write-Host "Exclude string: $local_exclude"

    cargo build --color=always --workspace $local_exclude --all-targets $Features

    if (!$?) {
        Write-Host "Build failed"
        Exit 1
    }

    $env:G_DEBUG="fatal_warnings"
    $env:RUST_BACKTRACE="1"
    cargo test --no-fail-fast --color=always --workspace $local_exclude --all-targets $Features

    if (!$?) {
        Write-Host "Tests failed"
        Exit 1
    }
}

foreach($feature in $features_matrix) {
    Run-Tests -Features $feature
}
