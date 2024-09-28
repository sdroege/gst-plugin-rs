# Changelog
All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com/en/1.0.0/)
and this project adheres to [Semantic Versioning](http://semver.org/spec/v2.0.0.html),
specifically the [variant used by Rust](http://doc.crates.io/manifest.html#the-version-field).

## [0.13.2] - 2024-09-28
### Fixed
- cea608overlay: Avoid overflow when deciding which lines to retain.
- cea708mux: Actually push gap events downstream.
- cea708mux: Stop with EOS once all pads are EOS.
- cea708mux: Fix off-by-one when deciding if a buffer belongs to this or the
  next frame.
- mpegtslivesrc: Various timestamp tracking fixes.
- onvifmetadatapay: Set output caps earlier.
- transcriberbin: Fix passthrough state change.
- webrtcsink: Fix setting of RFC7273 attributes in the SDP.

### Added
- dav1ddec: Add properties for film grain synthesis and in-loop filters.
- mpegtslivesrc: Handle PCR discontinuities.
- rtpav1depay: Add wait-for-keyframe and request-keyframe properties.
- webrtcsrc: Expose msid property on source pads.

### Changed
- spotify: Reduce dependencies.

## [0.13.1] - 2024-08-27
### Fixed
- transcriberbin: Fix gst-inspect with missing elements.
- gtk4paintablesink: Move dmabuf cfg to the correct bracket level.
- webrtcsrc: Don't hold the state lock while removing sessions.
- rtpbasepay: Various fixes to payloader base class.
- webrtcsink: Fix various assertions when finalizing.
- webrtcsrc: Make sure to always call end_session() without state lock.
- mpegtslivesrc: Handle PCR discontinuities as errors.
- ndisrc: Calculate timestamps for metadata buffers too.
- Various new clippy warnings.
- webrtcsink: Fix segment format mismatch when using a remote offer.
- awstranscriber: Fix sanity check in transcribe loop.
- whepsrc: Fix incorrect default caps.

### Changed
- gtk4paintablesink: Enable `gtk::GraphicsOffload::black-background` when
  building with GTK 4.16 or newer.
- gstwebrtc-api: Always include index file in dist for convenience.
- rtpbasepay: Negotiate SSRC/PT with downstream via caps for backwards
  compatibility.
- hlssink3: Use more accurate fragment duration from splitmuxsink if
  available.

### Added
- gtk4paintablesink: Add `window-width` and `window-height` properties.
- gtk4paintablesink: Add custom widget for automatically updating window size.
- fmp4mux / mp4mux: Add image orientation tag support.
- webrtcsink: Add nvv4l2av1enc support.
- cmafmux: Add Opus support.

## [0.13.0] - 2024-07-16

### Added
- rtp: New RTP payloader and depayloader base classes, in addition to new
  payloader and depayloaders for: PCMA, PCMU, AC-3, AV1 (ported to the new
  base classes), MPEG-TS, VP8, VP9, MP4A, MP4G, JPEG, Opus, KLV.
- originalbuffer: New pair of elements that allows to save a buffer, perform
  transformations on it and then restore the original buffer but keeping any
  new analytics and other metadata on it.
- gopbuffer: New element for buffering an entire group-of-pictures.
- tttocea708: New element for converting timed text to CEA-708 closed captions.
- cea708mux: New element for muxing multiple CEA-708 services together.
- transcriberbin: Add support for generating CEA-708 closed captions and
  CEA-608-in-708.
- cea708overlay: New overlay element for CEA-708 and CEA-608 closed captions.
- dav1ddec: Signal colorimetry in the caps.
- webrtc: Add support for RFC7273 clock signalling and synchronization to
  webrtcsrc and webrtcsink.
- tracers: Add a new pad push durations tracer.
- transcriberbin: Add support for a secondary audio stream.
- quinn: New plugin with a QUIC source and sink element.
- rtpgccbwe: New mode based on linear regression instead of a kalman filter.
- rtp: New rtpsend and rtprecv elements that provide a new implementation of
  the rtpbin element with a separate send and receive side.
- rtpsrc2: Add support for new rtpsend / rtprecv elements instead of rtpbin.
- webrtcsrc: Add multi-producer support.
- livesync: Add sync property for enabling/disabling syncing of the output
  buffers to the clock.
- mpegtslivesrc: New element for receiving an MPEG-TS stream, e.g. over SRT or
  UDP, and exposing the remote PCR clock as a local GStreamer clock.
- gtk4paintablesink: Add support for rotations / flipping.
- gtk4paintablesink: Add support for RGBx formats in non-GL mode.

### Fixed
- livesync: Queue up to latency buffers instead of requiring a queue of the
  same size in front of livesync.
- livesync: Synchronize the first buffer to the clock too.
- livesync: Use correct duration for deciding whether a filler has to be
  inserted or not.
- audioloudnorm: Fix possible off-by-one in the limiter when handling the very
  last buffer.
- webrtcsink: Fix property types for rav1enc.

### Changed
- sccparse, mccparse: Port from nom to winnow.
- uriplaylistbin: Rely on uridecodebin3 gapless logic instead of
  re-implementing it.
- webrtc: Refactor of JavaScript API.
- janusvrwebrtcsink: New use-string-ids property to distinguish between
  integer and string room IDs, instead of always using strings and guessing
  what the server expects.
- janusvrwebrtcsink: Handle more events and expose some via signals.
- dav1ddec: Require dav1d 1.3.0.
- closedcaption: Drop libcaption C code and switch to a pure Rust
  implementation.

## [0.12.7] - 2024-06-19
### Fixed
- aws, spotifyaudiosrc, reqwesthttpsrc, webrtchttp: Fix race condition when unlocking
- rtp: Allow any payload type for the AV1 RTP payloader/depayloader
- rtp: Various fixes to the AV1 RTP payloader/depayloader to work correctly
  with Chrome and Pion
- meson: Various fixes to the meson-based build system around cargo
- webrtcsink: Use correct property names for configuring `av1enc`
- webrtcsink: Avoid lock poisoning when setting encoder properties

### Added
- ndi: Support for NDI SDK v6
- webrtcsink: Support for AV1 via `nvav1enc`, `av1enc` or `rav1enc`

### Changed
- Update to async-tungstenite 0.26

## [0.12.6] - 2024-05-23
### Fixed
- Various Rust 1.78 clippy warnings.
- gtk4paintablesink: Fix plugin description.

### Added
- fmp4mux / mp4mux: Add support for adding AV1 header OBUs into the MP4
  headers.
- fmp4mux / mp4mux: Take track language from the tags if provided.
- gtk4paintablesink: Add GST_GTK4_WINDOW_FULLSCREEN environment variable to
  create a fullscreen window for debugging purposes.
- gtk4paintablesink: Also create a window automatically when called from
  gst-play-1.0.
- webrtc: Add support for insecure TLS connections.
- webrtcsink: Add VP9 parser after the encoder.

### Changed
- webrtcsink: Improve error when no discovery pipeline runs.
- rtpgccbwe: Improve debug output in various places.

## [0.12.5] - 2024-04-29
### Fixed
- hrtfrender: Use a bitmask instead of an int in the caps for the channel-mask.
- rtpgccbwe: Don't log an error when pushing a buffer list fails while stopping.
- webrtcsink: Don't panic in bitrate handling with unsupported encoders.
- webrtcsink: Don't panic if unsupported input caps are used.
- webrtcsrc: Allow a `None` producer-id in `request-encoded-filter` signal.

### Added
- aws: New property to support path-style addressing.
- fmp4mux / mp4mux: Support FLAC instead (f)MP4.
- gtk4: Support directly importing dmabufs with GTK 4.14.
- gtk4: Add force-aspect-ratio property similar to other video sinks.

## [0.12.4] - 2024-04-08
### Fixed
- aws: Use fixed behaviour version to ensure that updates to the AWS SDK don't
  change any defaults configurations in unexpected ways.
- onvifmetadataparse: Fix possible deadlock on shutdown.
- webrtcsink: Set `perfect-timestamp=true` on audio encoders to work around
  bugs in Chrome's audio decoders.
- Various clippy warnings.

### Changed
- reqwest: Update to reqwest 0.12.
- webrtchttp: Update to reqwest 0.12.

## [0.12.3] - 2024-03-21
### Fixed
- gtk4paintablesink: Fix scaling of texture position.
- janusvrwebrtcsink: Handle 64 bit numerical room ids.
- janusvrwebrtcsink: Don't include deprecated audio/video fields in publish
  messages.
- janusvrwebrtcsink: Handle various other messages to avoid printing errors.
- livekitwebrtc: Fix shutdown behaviour.
- rtpgccbwe: Don't forward buffer lists with buffers from different SSRCs to
  avoid breaking assumptions in rtpsession.
- sccparse: Ignore invalid timecodes during seeking.
- webrtcsink: Don't try parsing audio caps as video caps.

### Changed
- webrtc: Allow resolution and framerate changes.
- webrtcsrc: Make producer-peer-id optional.

### Added
- livekitwebrtcsrc: Add new LiveKit source element.
- regex: Add support for configuring regex behaviour.
- spotifyaudiosrc: Document how to use with non-Facebook accounts.
- webrtcsrc: Add `do-retransmission` property.

## [0.12.2] - 2024-02-26
### Fixed
- rtpgccbwe: Don't reset PTS/DTS to `None` as otherwise `rtpsession` won't be
  able to generate valid RTCP.
- webrtcsink: Fix usage with 1.22.

### Added
- janusvrwebrtcsink: Add `secret-key` property.
- janusvrwebrtcsink: Allow for string room ids and add `string-ids` property.
- textwrap: Don't split on all whitespaces, especially not on non-breaking
  whitespace.

## [0.12.1] - 2024-02-13
### Added
- gtk4: Create a window for testing purposes when running in `gst-launch-1.0`
  or if `GST_GTK4_WINDOW=1` is set.
- webrtcsink: Add `msid` property.

## [0.12.0] - 2024-02-08
### Changed
- ndi: `ndisrc` passes received data downstream without an additional copy, if
  possible.
- webrtc: Cleanups to webrtcsrc/sink default signalling protocol, JavaScript
  implementation and server implementation.
- webrtc: `whipwebrtcsink` is renamed to `whipclientsink` and deprecate old
  `whipsink`.

### Fixed
- gtk4: Fix Windows build when using EGL.
- gtk4: Fix ARGB pre-multiplication with GTK 4.14. This requires building with
  the `gtk_v4_10` or even better `gtk_v4_14` feature.
- gtk4: Fix segfault if GTK3 is used in the same process.
- gtk4: Always draw background behind the video frame and not only when
  borders have to be added to avoid glitches.
- livekitwebrtcsink: Add high-quality layer for video streams.
- webrtc: Fix potential hang and fd leak in signalling server.
- webrtc: Fix closing of WebSockets.
- webrtchttp: Allow setting `None` for audio/video caps for WHEP.

### Added
- New `awss3putobjectsink` that works similar to `awss3sink` but with a
  different upload strategy.
- New `hlscmafsink` element for writing HLS streams with CMAF/ISOBMFF
  fragments.
- New `inter` plugin with `intersink` / `intersrc` elements that allow to
  connect different pipelines in the same process.
- New `janusvrwebrtcsink` element for the Janus VideoRoom API.
- New `rtspsrc2` element.
- New `whipserversrc` element.
- gtk4: New `background-color` property for setting the color of the
  background of the frame and the borders, if any.
- gtk4: New `scale-filter` property for defining how to scale the frames.
- livesync: Add support for image formats.
- ndi: Closed Caption support in `ndisrc` / `ndisink`.
- textwrap: Add support for gaps.
- tracers: Optionally only show late buffers in `buffer-lateness` tracer.
- webrtc: Add support for custom headers.
- webrtcsink: New `payloader-setup` signal to configure payloader elements.
- webrtcsrc: Support for navigation events.

## [0.11.3] - 2023-12-18
### Fixed
- ndi: Mark a private type as such and remove a wrong `Clone` impl of internal types.
- uriplaylistbin: Fix a minor clippy warning.
- fallbacksrc: Fix error during badly timed timeout scheduling.
- webrtcsink: Fail gracefully if webrtcbin pads can't be requested instead of
  panicking.
- threadshare: Fix deadlock in `ts-udpsrc` `notify::used-socket` signal
  emission.

### Changed
- Update to AWS SDK 1.0.
- Update to windows-sys 0.52.
- Update to async-tungstenite 0.24.
- Update to bitstream-io 2.0.
- tttocea608: De-duplicate some functions.
- gtk4: Use async-channel instead of deprecated GLib main context channel.

## [0.11.2] - 2023-11-11
### Fixed
- filesink / s3sink: Set `sync=false` to allow processing faster than
  real-time.
- hlssink3: Various minor bugfixes and cleanups.
- livesync: Various minor bugfixes and cleanups that should make the element
  work more reliable.
- s3sink: Fix handling of non-ASCII characters in URIs and keys.
- sccparse: Parse SCC files that are incorrectly created by CCExtractor.
- ndisrc: Assume > 8 channels are unpositioned.
- rtpav1depay: Skip unexpected leading fragments instead of repeatedly warning
  about the stream possibly being corrupted.
- rtpav1depay: Don't push stale temporal delimiters downstream but wait until
  a complete OBU is collected.
- whipwebrtcsink: Use correct URL during redirects.
- webrtcsink: Make sure to not miss any ICE candidates.
- webrtcsink: Fix deadlock when calling `set-local-description`.
- webrtcsrc: Fix reference cycles that prevented the element from being freed.
- webrtcsrc: Define signaller property as `CONSTRUCT_ONLY` to make it actually
  possible to set different signallers.
- webrtc: Update livekit signaller to livekit 0.2.
- meson: Various fixes to the meson-based build system.

### Added
- audiornnoise: Attach audio level meta to output buffers.
- hlssink3: Allow adding `EXT-X-PROGRAM-DATE-TIME` tag to the manifest.
- webrtcsrc: Add `turn-servers` property.

### Changed
- aws/webrtc: Update to AWS SDK 0.57/0.35.

## [0.11.1] - 2023-10-04
### Fixed
- fallbackswitch: Fix various deadlocks.
- webrtcsink: Gracefully fail if adding the TWCC RTP header extension fails.
- webrtcsink: Fix codec selection discovery.
- webrtcsink: Add support for D3D11 memory and qsvh264enc.
- onvifmetadataparse: Skip metadata frames with unrepresentable UTC times.
- gtk4paintablesink: Pre-multiply alpha when creating GL textures with alpha.
- gtk4paintablesink: Only support RGBA/RGB in the GL code path.
- webrtchttp: Respect HTTP redirects.
- fmp4mux: Specify unit of fragment-duration property.

### Changed
- threadshare: Port to polling 3.1.

## [0.11.0] - 2023-08-10
### Changed
- Updated MSRV to 1.70.
- Compatible with gtk-rs 0.18 and gstreamer-rs 0.21.
- awstranscriber: Move to HTTP2-based API via the aws-sdk-transcribestreaming
  crate instead of our own implementation around the WebSocket API.

### Added
- webrtcsink: Add AWS KVS signaller and corresponding aws-kvs-webrtcsink
  element.
- awstranscriber / transcriberbin: Add support for translations and outputting
  transcriptions from a single audio stream in multiple languages at once.
- gstwebrtc-api: JavaScript API for interacting with the default signalling
  protocol used by webrtcsink / webrtcsrc.
- cea608to708: New element for converting CEA608 to CEA708 closed captions.
- webrtcsink: Expose the signaller as property and allow implementing a
  custom signaller by connecting signal handlers to the default signaller.
- webrtcsink: Add support for pre-encoded streams.
- togglerecord: Add support for non-live input streams.
- webrtcsink: New whipwebrtcsink that implements WHIP around webrtcsink.
  The existing whipsink still exists but will sooner or later be deprecated.
- webrtcsink: Add LiveKit signaller and corresponding livekitwebrtcsink
  element.

## [0.10.11] - 2023-07-20
### Fixed
- fallbackswitch: Fix pad health calculation and notifies.
- fallbackswitch: Change the threshold for trailing buffers.
- webrtcsink: Fix pipeline when input caps contain a max-framerate field.
- webrtcsink: Set VP8/VP9 payloader properties based on payloader element
  factory name.
- webrtcsink: Set config-interval=-1 and aggregate-mode=zero-latency for
  H264/5 payloaders.
- webrtcsink: Translate force-keyunit events to custom force-IDR API of NVIDIA
  encoders.
- webrtcsink: Configure only 4 threads instead of 12 for x264enc for Chrome
  compatibility.
- fmp4mux: Fix draining in chunk mode if keyframes are after the desired
  fragment end.

## [0.10.10] - 2023-07-05
### Fixed
- livesync: Improve EOS handling to be in sync with `queue`'s behaviour.
- livesync: Wait for the end timestamp of the previous buffer before looking
  at queue to actually make use of the available latency.
- webrtcsink: Avoid panic on unprepare from an async tokio context.
- webrtc/signalling: Fix race condition in message ordering.
- webrtcsink: Use the correct property types when configuring `nvvideoconvert`.
- videofx: Minimize dependencies of the image crate.
- togglerecord: Fix segment clipping to actually work as intended.

### Added
- gtk4paintablesink: Support for WGL/EGL on Windows.
- gtk4paintablesink: Add Python example application to the repository.

## [0.10.9] - 2023-06-19
### Fixed
- mp4mux/fmp4mux: Fix byte order in Opus extension box.
- webrtcsrc: Add twcc extension to the codec-preferences when present.
- webrtcsink: Don't try using cudaconvert if it is not present.
- mccparse: Don't offset the first timecode to a zero PTS.
- Correctly use MPL as license specifier instead of MPL-2 for plugins that
  compile with GStreamer < 1.20.

### Added
- fallbackswitch: Add `stop-on-eos` property.

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

[Unreleased]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.13.2...HEAD
[0.13.2]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.13.1...0.13.2
[0.13.1]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.13.0...0.13.1
[0.13.0]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.12.7...0.13.0
[0.12.7]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.12.6...0.12.7
[0.12.6]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.12.5...0.12.6
[0.12.5]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.12.4...0.12.5
[0.12.4]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.12.3...0.12.4
[0.12.3]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.12.2...0.12.3
[0.12.2]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.12.1...0.12.2
[0.12.1]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.12.0...0.12.1
[0.12.0]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.11.3...0.12.0
[0.11.3]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.11.2...0.11.3
[0.11.2]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.11.1...0.11.2
[0.11.1]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.11.0...0.11.1
[0.11.0]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.11...0.11.0
[0.10.11]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.10...0.10.11
[0.10.10]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.9...0.10.10
[0.10.9]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.8...0.10.9
[0.10.8]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.7...0.10.8
[0.10.7]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.6...0.10.7
[0.10.6]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.5...0.10.6
[0.10.5]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.4...0.10.5
[0.10.4]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.3...0.10.4
[0.10.3]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.2...0.10.3
[0.10.2]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.1...0.10.2
[0.10.1]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.10.0...0.10.1
[0.10.0]: https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/compare/0.9.0...0.10.0
