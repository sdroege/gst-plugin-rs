# Gtk 4 Sink & Paintable

GTK 4 provides `gtk::Video` & `gtk::Picture` for rendering media such as videos. As the default `gtk::Video` widget doesn't
offer the possibility to use a custom `gst::Pipeline`. The plugin provides a `gst_video::VideoSink` along with a `gdk::Paintable` that's capable of rendering the sink's frames.

The Sink can generate GL Textures if the system is capable of it, but it needs to be compiled
with either `wayland`, `x11glx` or `x11egl` cargo features.

# Flatpak Integration

To build and include the plugin in a Flatpak manifest, you can add the following snippet to your json manifest:

```json
{
    "sdk-extensions": [
        "org.freedesktop.Sdk.Extension.rust-stable"
    ],
    "build-options": {
        "env": {
            "CARGO_HOME": "/run/build/cargo-c/cargo"
        },
        "append-path": "/usr/lib/sdk/rust-stable/bin",
    },
    "modules": [
        {
            "name": "cargo-c",
            "buildsystem": "simple",
            "build-commands": [
                "cargo install cargo-c --root /app"
            ],
            "build-options": {
                "build-args": [
                    "--share=network"
                ]
            },
            "cleanup": [
                "*"
            ]
        },
        {
            "name": "gst-plugins-rs",
            "buildsystem": "simple",
            "sources": [
                {
                    "type": "git",
                    "url": "https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs",
                    "branch": "0.10"
                }
            ],
            "build-options": {
                "build-args": [
                    "--share=network"
                ]
            },
            "build-commands": [
                "cargo cinstall -p gst-plugin-gtk4 --prefix=/app"
            ]
        }
    ]
}
```
