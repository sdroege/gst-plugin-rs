# gst-plugins-rs [![crates.io](https://img.shields.io/crates/v/gst-plugin.svg)](https://crates.io/crates/gst-plugin) [![pipeline status](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/badges/main/pipeline.svg)](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/commits/main)

Repository containing various [GStreamer](https://gstreamer.freedesktop.org/)
plugins and elements written in the [Rust programming
language](https://www.rust-lang.org/).

The plugins build upon the [GStreamer Rust bindings](https://gitlab.freedesktop.org/gstreamer/gstreamer-rs).
Check the README.md of that repository also for details about how to set-up
your development environment.

## Plugins

You will find the following plugins in this repository:

  * `generic`
    - `file`: A Rust implementation of the standard `filesrc` and `filesink` elements

    - `fmp4`: A fragmented MP4/ISOBMFF/CMAF muxer for generating e.g. DASH/HLS media fragments.

    - `sodium`: Elements to perform encryption and decryption using [libsodium](https://libsodium.org).

    - `threadshare`: Some popular threaded elements reimplemented using common thread-sharing infrastructure.

  * `net`
    - `hlssink3`: An element for generating MPEG-TS HLS streams.

    - `reqwest`: An HTTP source element based on the [reqwest](https://github.com/seanmonstar/reqwest) library.

    - `rusoto`: Various elements for Amazon AWS services using the [Rusoto](https://rusoto.org) library
      - `s3src`/`s3sink`: A source and sink element to talk to the Amazon S3 object storage system.
      - `aws_transcriber`: an element wrapping the AWS Transcriber service.

  * `audio`
    - `audiofx`: Elements to apply audio effects to a stream
      - `rsaudioecho`: a simple echo/reverb filter.
      - `rsaudioloudnorm`: [audio normalization](http://k.ylo.ph/2016/04/04/loudnorm.html) filter.
      - `audiornnoise`: Filter for [removing noise](https://jmvalin.ca/demo/rnnoise/).
      - `ebur128level`: Filter for measuring audio loudness according to EBU R-128.
      - `hrtfrender`: Filter for rendering audio according to a [head-related transfer
        function](https://en.wikipedia.org/wiki/Head-related_transfer_function).

    - `claxon`: A FLAC decoder based on the [Claxon](https://github.com/ruuda/claxon) library.

    - `csound`: A plugin to implement audio effects using the [Csound](https://csound.com/) library.

    - `lewton`: A Vorbis decoder based on the [lewton](https://github.com/RustAudio/lewton) library.

    - `spotify`: A plugin to access content from [Spotify](https://www.spotify.com/) based on the [librespot](https://github.com/librespot-org/) library.

  * `video`
    - `cdg`: A parser and renderer for [CD+G karaoke data](https://docs.rs/cdg/0.1.0/cdg/).

    - `closedcaption`: Plugin to deal with closed caption streams
      - `ccdetect`: Detects if a stream contains active Closed Captions.
      - `cea608overlay`: Overlay CEA-608 / EIA-608 closed captions over a
        video stream.
      - `cea608tojson`: Convert CEA-608 / EIA-608 closed captions to a JSON
        stream.
      - `cea608tott`: Convert CEA-608 / EIA-608 closed captions to timed text.
      - `jsontovtt`: Convert JSON to timed text.
      - `mccenc`: Convert CEA-608 / EIA-608 and CEA-708 / EIA-708 closed captions to the MCC format.
      - `mccparse`: Parse CEA-608 / EIA-608 and CEA-708 / EIA-708 closed captions from the MCC format.
      - `sccenc`: Convert CEA-608 / EIA-608 closed captions to the MCC format.
      - `sccparse`: Parse CEA-608 / EIA-608 closed captions from the MCC format.
      - `transcriberbin`: Convenience bin around transcriber elements like `aws_transcriber`.
      - `tttocea608`: Convert timed text to CEA-608 / EIA-608 closed captions.
      - `tttojson`: Convert timed text to JSON.

    - `dav1d`: AV1 decoder based on the [dav1d](https://code.videolan.org/videolan/dav1d) library.

    - `ffv1`: FFV1 decoder based on the [ffv1](https://github.com/rust-av/ffv1) library.

    - `flavors`: FLV demuxer based on the [flavors](https://github.com/rust-av/flavors) library.

    - `gif`: A GIF encoder based on the [gif](https://github.com/image-rs/image-gif) library.

    - `gtk4`: A [GTK4](https://www.gtk.org) video sink that provides a `GdkPaintable` for UI integration.

    - `hsv`: Plugin with various elements to work with video data in hue, saturation, value format
       - `hsvdetector`: Mark pixels that are close to a configured color in HSV format.
       - `hsvfilter`: Apply various transformations in the HSV colorspace.

    - `png`: PNG encoder based on the [png](https://github.com/image-rs/image-png) library.

    - `rav1e`: AV1 encoder based on the [rav1e](https://github.com/xiph/rav1e) library.

    - `videofx`: Plugin with various video filters.
      - `roundedcorners`: Element to make the corners of a video rounded via the alpha channel.

    - `webp`: WebP decoder based on the [libwebp-sys-2](https://github.com/qnighy/libwebp-sys2-rs) library.

  * `text`
    - `ahead`: A plugin to display upcoming text buffers ahead.

    - `json`: A plugin to convert a stream of JSON objects to a higher level wrapped NDJSON output.

    - `regex`: A regular expression text filter plugin.

    - `wrap`: A plugin to perform text wrapping with hyphenation.

  * `utils`
    - `fallbackswitch`:
      - `fallbackswitch`: An element that allows falling back to different
        sink pads after a timeout based on the sink pads' priorities.
      - `fallbacksrc`: Element similar to `urisourcebin` that allows
        configuring a fallback audio/video if there are problems with the main
        source.

    - `togglerecord`: Element to enable starting and stopping multiple streams together.

## Building

gst-plugins-rs relies on [cargo-c](https://github.com/lu-zero/cargo-c/) to
generate shared and static C libraries. It can be installed using:

```
$ cargo install cargo-c
```

Then you can easily build and test a specific plugin:

```
$ cargo cbuild -p gst-plugin-cdg
$ GST_PLUGIN_PATH="target/x86_64-unknown-linux-gnu/debug:$GST_PLUGIN_PATH" gst-inspect-1.0 cdgdec
```

Replace `x86_64-unknown-linux-gnu` with your system's Rust target triple (`rustc -vV`).

The plugin can also be installed system-wide:

```
$ cargo cbuild -p gst-plugin-cdg --prefix=/usr
$ cargo cinstall -p gst-plugin-cdg --prefix=/usr
```

This will install the plugin to `/usr/lib/gstreamer-1.0`.
You can use `--libdir` to pass a custom `lib` directory
such as `/usr/lib/x86_64-linux-gnu` for example.

Note that you can also just use `cargo` directly to build Rust static libraries
and shared C libraries. `cargo-c` is mostly useful to build static C libraries
and generate `pkg-config` files.

## LICENSE

gst-plugins-rs and all crates contained in here that are not listed below are
licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   http://opensource.org/licenses/MIT)

at your option.

gst-plugin-togglerecord is licensed under the Lesser General Public License
([LICENSE-LGPLv2](LICENSE-LGPLv2)) version 2.1 or (at your option) any later
version.

gst-plugin-csound is licensed under the Lesser General Public License
([LICENSE-LGPLv2](LICENSE-LGPLv2)) version 2.1 or (at your option) any later
version.

GStreamer itself is licensed under the Lesser General Public License version
2.1 or (at your option) any later version:
https://www.gnu.org/licenses/lgpl-2.1.html

## Contribution

Any kinds of contributions are welcome as a pull request.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in gst-plugins-rs by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
