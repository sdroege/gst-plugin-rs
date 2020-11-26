# gst-plugins-rs [![crates.io](https://img.shields.io/crates/v/gst-plugin.svg)](https://crates.io/crates/gst-plugin) [![pipeline status](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/badges/master/pipeline.svg)](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/commits/master)

Repository containing various [GStreamer](https://gstreamer.freedesktop.org/)
plugins and elements written in the [Rust programming
language](https://www.rust-lang.org/).

The plugins build upon the [GStreamer Rust bindings](https://gitlab.freedesktop.org/gstreamer/gstreamer-rs).
Check the README.md of that repository also for details about how to set-up
your development environment.

## Plugins

You will find the following plugins in this repository:

  * `generic`
    - `file`: A Rust implementation of the standard `filesrc` and `filesink`
      elements

    - `sodium`: Elements to perform encryption and decryption using
      [libsodium](https://libsodium.org).

    - `threadshare`: Some popular threaded elements reimplemented using common
      thread-sharing infrastructure.

  * `net`
    - `reqwest`: An HTTP source element based on the
      [reqwest](https://github.com/seanmonstar/reqwest) library.

    - `rusoto`: A source and sink plugin to talk to the Amazon S3 object
      storage system using the [Rusoto](https://rusoto.org) library.

  * `audio`
    - `audiofx`: Plugins to apply audio effects to a stream (such as adding
      echo/reverb, [removing noise](https://jmvalin.ca/demo/rnnoise/) or normalization).

    - `claxon`: A FLAC decoder based on the
      [Claxon](https://github.com/ruuda/claxon) library.

    - `csound`: A plugin to implement audio effects using the
      [Csound](https://csound.com/) library.

    - `lewton`: A Vorbis decoder based on the
      [lewton](https://github.com/RustAudio/lewton) library.

  * `video`
    - `cdg`: A parser and renderer for
      [CD+G karaoke data](https://docs.rs/cdg/0.1.0/cdg/).

    - `closedcaption`: Plugins to deal with several closed caption formats
      (MCC, SCC, EIA-608/CEA-608 and timed text).

    - `dav1d`: AV1 decoder based on the
      [dav1d](https://code.videolan.org/videolan/dav1d) library.

    - `flavors`: FLV demuxer based on the
      [flavors](https://github.com/rust-av/flavors) library.

    - `gif`: A GIF encoder based on the
      [gif](https://github.com/image-rs/image-gif) library.

    - `rav1e`: AV1 encoder based on the [rav1e](https://github.com/xiph/rav1e)
      library.

  * `utils`
    - `fallbackswitch`: Aggregator element that allows falling back to a
      different sink pad after a timeout.

    - `togglerecord`: Element to enable starting and stopping multiple
      streams together.

## Building

gst-plugins-rs relies on [cargo-c](https://github.com/lu-zero/cargo-c/) to
generate shared and static C libraries. It can be installed using:

```
$ cargo install cargo-c
```

Then you can easily build and test a specific plugin:

```
$ cargo cbuild --manifest-path video/cdg/Cargo.toml
$ GST_PLUGIN_PATH=$GST_PLUGIN_PATH:target/debug gst-inspect-1.0 cdgdec
```

The plugin can also be installed system-wide:

```
$ cargo cbuild --manifest-path video/cdg/Cargo.toml --prefix=/usr
$ cargo cinstall --manifest-path video/cdg/Cargo.toml --prefix=/usr
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
