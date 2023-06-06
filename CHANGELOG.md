# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](http://semver.org/spec/v2.0.0.html),
specifically the [variant used by Rust](http://doc.crates.io/manifest.html#the-version-field).

## [0.10.8] - 2023-06-07
### Fixed
- fmp4mux: Use updated start PTS when checking if a stream is filled instead
  of a stale one.
- fmp4mux: Fix various issues with stream gaps, especially in the beginning.
- fmp4mux: Fix waiting in live pipelines.
- uriplaylistbin: Prevent deadlocks during property notifications.
- webrtcsink: Fix panics during `twcc-stats` callback and related issues.
- awstranscriber: Handle stream disconts correctly.
- roundedcorners: Fix caps negotiation to not use I420 if a border radius is
  configured.
- whipsink: Use the correct pad template to request pads from the internal
  webrtcbin.
- fallbacksrc: Don't apply fallback audio caps to the main stream.
- webrtcsrc: Fix caps handling during transceiver creation.

### Changed
- rtpgccbwe: Improve packet handling.

## [0.10.7] - 2023-05-09
### Fixed
- ffv1dec: Drop rank until the implementation is feature-complete.
- spotifyaudiosrc: Check cached credentials before use and fix usage of
  credentials cache.
- tttocea608: Specify raw CEA608 field.
- gtk4paintablesink: Fix compilation on non-Linux UNIX systems.
- webrtcsrc: Don't set stun-server to the empty string if none was set.
- webrtcsink: Abort statistics collection before stopping the signaller.
- rtpgccbwe: Don't process empty lists.

### Changed
- ndi: Update to libloading 0.8.
- aws: Update to AWS SDK 0.55/0.27.
- webrtcsink: Order pads by serial number.
- Update to async-tungstenite 0.22.

### Added
- webrtcsink/webrtcsrc: Add `request-encoded-filter` signal to add support for
  inserting custom filters between encoder/payloader or depayloader/decoder.
  This allows interacting with the "insertable streams" API from Chrome.

## [0.10.6] - 2023-04-06
### Fixed
- webrtcsink: Fix max/min-bitrate property blurb/nick.
- uriplaylistbin: Add missing queues to example.
- tttocea608: Fix pushing of caps events that sometimes contained unfixed caps.
- tttocea608: Fix disappearing text after special character in non-popon mode.
- transcriberbin: Fix deadlock on construction.
- transcriberbin: Fix initial bin setup.
- fallbacksrc: Handle incompatible downstream caps without panicking.
- ndisrc: Fix copying of raw video frames with different NDI/GStreamer strides.
- livesync: Correctly assume zero upstream latency if latency query fails.

### Added
- webrtcsink: Add `ice-transport-policy` property that proxies the same
  `webrtcbin` property.

## [0.10.5] - 2023-03-19
### Fixed
- gtk4: Fix build with OpenGL support on macOS.
- threadshare: Fix symbol conflicts when statically linking the plugin.

## [0.10.4] - 2023-03-14
### Fixed
- fmp4mux: Return a running time from `AggregatorImpl::next_time()` to fix
  waiting in live pipelines.
- fmp4mux: Fix `hls_live` example to set properties on the right element.
- uriplaylistbin: Reset element when switching back to `NULL` state.
- livesync: Handle variable framerates correctly in fallback buffer duration
  calculation.
- meson: Fix GStreamer version feature detection.

### Added
- webrtc: New `webrtc` element.

## [0.10.3] - 2023-03-02
### Added
- tracers: `queue_levels` tracer now also supports printing the `appsrc` levels.
- webrtc: `webrtcsink` can use `nvvidconv` if `nvvideoconvert` does not exist
  on an NVIDIA platform.

### Fixed
- gtk4: Set the sync point on the video frame after mapping it as otherwise
  the frame might not be ready yet for further usage.
- livesync: Correctly calculate the fallback buffer duration from the video
  framerate.
- ndi: Handle caps changes correctly in `ndisinkcombiner`.

### Changed
- webrtc: Minor cleanup.

## [0.10.2] - 2023-02-23
### Fixed
- hlssink3: Allow signal handlers to return `None`
- gtk4: Make GL context sharing more reliable in pipelines with multiple
  `gtk4paintablesinks`
- gtk4: Attach channel receiver to the main context from the correct thread to
  make it possible to start the sink from a different thread than the main
  thread without having retrieved the paintable from the main thread before.
- fmp4mux/mp4mux: Ignore caps changes if only the framerate changes.

### Changed
- gtk4: Simplify and refactor GL context sharing. Apart from being more
  reliable this reduces GL resource usage.

## [0.10.1] - 2023-02-13
### Fixed
- rtpav1pay: Fix calculation of Leb128 size size to work correctly with
  streams from certain encoders.

## [0.10.0] - 2023-02-10
### Fixed
- audiornnoise: Use correct value range for the samples
- awss3sink: Treat stopping without EOS as an error for multipart upload
- awss3hlssink: Fix the name of the hlssink child element
- awss3hlssink: Fix deadlock on EOS
- dav1d: Various fixes to improve performance, to handle decoding errors more
  gracefully and to make sure all frames are output in the end
- fmp4mux: Various fixes to fragment splitting behaviour, output formatting
  and header generation
- gtk4: Various stability and rendering fixes
- meson: Various fixes and improvements to the meson-based build system
- ndi: provide non-Linux/macOS UNIX fallback for the soname
- ndisrc: Use default channel mask for audio output to allow >2 channels to
  work better
- rav1e: Correctly enable threading support
- rtpav1: Various fixes to the payloader and depayloader to handle streams
  more correctly and to handle errors more cleanly
- rtpav1depay: Set caps on the source pad
- spotify: fix "start a runtime from within a runtime" with static link
- textahead: fix previous buffers
- textwrap: Don't panic on empty buffers
- tttocea608: Don't fail if a GAP event contains no duration
- webrtchttp: whipsink: construct TURN URL correctly
- webrtcsink: fix panic on pre-bwe request error
- whipsink: Send ICE candidates together with the offer
- whipsink: Various cleanups and minor fixes

### Added
- audiornnoise: Add voice detection threshold property
- awss3hlssink: Add `stats` property
- awss3sink: Add properties to set Content-Type and Content-Disposition
- fmp4mux: add 'offset-to-zero' property
- fmp4mux/mp4mux: add support for muxing Opus, VP8, VP9 and AV1 streams
- fmp4mux/mp4mux: Make media/track timescales configurable
- fmp4mux: Add support for CMAF-style chunking, e.g. low-latency / LL HLS and DASH
- gtk4: Support for rendering GL textures on X11/EGL, X11/GLX, Wayland and macOS
- hlssink3: Allow generating i-frame-only playlist
- livesync: New element that allows maintaining a contiguous live stream
  without gaps from a potentially unstable source.
- mp4mux: New non-fragmented MP4 muxer element
- spotifyaudiosrc: Support configurable bitrate
- textahead: add settings to display previous buffers
- threadshare: Introduce new ts-audiotestsrc
- webrtcsink: Support nvv4l2vp9enc
- whepsource: Add a WebRTC WHEP source element

### Changed
- audiofx: Derive from AudioFilter where possible
- dav1ddec: Lower rank to primary to allow usage of hardware decoders with
  higher ranks
- fmp4mux: Only push `fragment_offset` if `write-mfra` is true to reduce memory usage
- webrtcsink: Make the `turn-server` property a `turn-servers` list
- webrtcsink: Move from async-std to tokio

[Unreleased]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.8...HEAD
[0.10.8]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.7...0.10.8
[0.10.7]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.6...0.10.7
[0.10.6]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.5...0.10.6
[0.10.5]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.4...0.10.5
[0.10.4]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.3...0.10.4
[0.10.3]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.2...0.10.3
[0.10.2]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.1...0.10.2
[0.10.1]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.0...0.10.1
[0.10.0]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.9.0...0.10.0
