# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](http://semver.org/spec/v2.0.0.html),
specifically the [variant used by Rust](http://doc.crates.io/manifest.html#the-version-field).

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
- livesync: New element that alllows maintaining a contiguous live stream
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

[Unreleased]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.2...HEAD
[0.10.2]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.1...0.10.2
[0.10.1]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.0...0.10.1
[0.10.0]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.9.0...0.10.0
