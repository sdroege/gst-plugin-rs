# gst-plugins-rs [![crates.io](https://img.shields.io/crates/v/gst-plugin.svg)](https://crates.io/crates/gst-plugin) [![pipeline status](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/badges/main/pipeline.svg)](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/commits/main)

Repository containing various [GStreamer](https://gstreamer.freedesktop.org/)
plugins and elements written in the [Rust programming
language](https://www.rust-lang.org/).

The plugins build upon the [GStreamer Rust bindings](https://gitlab.freedesktop.org/gstreamer/gstreamer-rs).
Check the README.md of that repository also for details about how to set-up
your development environment.

## Releases and git tags

Releases from this repository are done from the versioned release branches,
i.e. `0.14` right now, and there's a tag for each release, e.g. `0.14.2`.
Plugins that are ready for general usage are also published to [crates.io](https://crates.io).

Distributors should make use of these versions.

In addition there are `gstreamer-X.Y.Z` tags in this repository. These tags
are for internal use by [cerbero](https://gitlab.freedesktop.org/gstreamer/cerbero) and are
used for the binary releases published by the GStreamer project. These tags
are not supposed to be used by distributors.

## Why is this separate from the GStreamer monorepo with an independent release schedule?

The GStreamer Rust bindings (`gstreamer-rs`) and the GStreamer plugins written
in Rust (`gst-plugins-rs`) use separate git repositories and an independent
release schedule, unlike for example the Python bindings which are part of the
main GStreamer repository. This will not change in the foreseeable future.

This separation does not mean that `gstreamer-rs` and `gst-plugins-rs` are not
part of the GStreamer project or second-class citizens in GStreamer. The separation
is due to technical reasons.

  * **Release cycle alignment**: Both repositories depend on
    [`gtk-rs`](https://gtk-rs.org) for GLib and GObject bindings. Since
    `gtk-rs` follows GNOME's six-monthly release cycle, `gstreamer-rs` and
    `gst-plugins-rs` are aligned to this release cycle as well. This ensures
    that the APIs stay synchronized as breaking API changes happen every 6, 12
    or 18 months. The crates follow `cargo`'s [semantic versioning
    rules](https://doc.rust-lang.org/cargo/reference/semver.html) to make
    these breaking API changes easier to handle for users.

  * **cargo ecosystem integration**: Cargo-based Rust projects expect to be able to pull in
    functionality via crates, and to facilitate that, each plugin in `gst-plugins-rs`
    is published as a crate. When a project wants to use the git version of a plugin,
    or the bindings, forcing them to pull in the entire GStreamer mono repository is excessive.
    Doing so would introduce unnecessary overhead in network bandwidth, disk usage and build
    times.

Note that depending on plugins as cargo dependency is explicitly supported and
widely used for Rust projects to statically link plugins into applications.
For this purpose all plugins are also published to crates.io but depending on
git versions is supported as well.

## Plugins

You will find the following plugins in this repository:

  * `analytics`

    - `analytics`:
      - `analyticscombiner`: Analytics combiner / batcher element
      - `analyticssplitter`: Analytics batch splitter element
      - `handdetectiontensordec`: Tensor decoder for hand detection tensors.
      - `onvifmeta2relationmeta`: Convert ONVIF metadata to relation metas
      - `relationmeta2onvifmeta`: Convert relation metadata to ONVIF metas
      - `yoloxtensordec`: Tensor decoder for YOLOX tensors.

    - `burn`:
      - `burn-yoloxinference`: Object detection inference element based on YOLOX.


  * `audio`

    - `audiofx`: Elements to apply audio effects to a stream
      - `agingradio`: Filter to add age to audio input using various kinds of distortion.
      - `audioloudnorm`: [audio normalization](http://k.ylo.ph/2016/04/04/loudnorm.html) filter.
      - `audiornnoise`: Filter for [removing noise](https://jmvalin.ca/demo/rnnoise/).
      - `ebur128level`: Filter for measuring audio loudness according to EBU R-128.
      - `rsaudioecho`: a simple echo/reverb filter.

    - `audioparsers`: Audio parser elements
      - `ac4parse`: Parses AC4 audio streams.
      - `s302mparse`: Parser for SMPTE S302M audio elementary streams.

    - `claxon`: A FLAC decoder based on the [Claxon](https://github.com/ruuda/claxon) library.
      - `claxondec`: Claxon FLAC decoder.

    - `csound`: A plugin to implement audio effects using the [Csound](https://csound.com/) library.
      - `csoundfilter`: Implement an audio filter/effects using Csound.

    - `demucs`: An audio source separation plugin using [demucs](https://github.com/adefossez/demucs).

    - `elevenlabs`:
      - `elevenlabssynthesizer`: Generate audio speech from text using the [ElevenLabs](https://elevenlabs.io) API/service.
      - `elevenlabsvoicecloner`: ElevenLabs Voice Cloner.

    - `hrtf`: Filters for rendering audio according to a [head-related transferfunction](https://en.wikipedia.org/wiki/Head-related_transfer_function).
      - `hrtfrender`: Read and render filters from IRCAM binary files.
      - `sofalizer`:  Read and render filters from SOFA files.

    - `lewton`: A Vorbis decoder based on the [lewton](https://github.com/RustAudio/lewton) library.
      - `lewtondec`: lewton Vorbis decoder.

    - `speechmatics`:
      - `speechmaticstranscriber`: Speech to text transcription using [Speechmatics](https://www.speechmatics.com/speech-to-text)

    - `spotify`: A plugin to access content from [Spotify](https://www.spotify.com/) based on the [librespot](https://github.com/librespot-org/) library.
      - `spotifyaudiosrc`: Spotify source.
      - `spotifylyricssrc`: Spotify lyrics source.

    - `whisper`:
      - `whispertranscriber`: Speech to text transcription using [whisper-cpp](github.com/ggml-org/whisper.cpp/)


  * `generic`

    - `compress`: General purpose lossless compression plugin.
      - `brotlicompress`: Compress data using Brotli.
      - `brotlidecompress`: Decompress data using Brotli.
      - `deflatecompress`: Compress data using deflate (no checksum).
      - `deflatedecompress`: Decompress deflate-compressed data.
      - `zlibcompress`: Compress data using zlib (with checksum).
      - `zlibdecompress`: Decompress zlib-compressed data.

    - `file`: A Rust implementation of the standard `filesrc` and `filesink` elements
      - `rsfilesink`: Write stream to a file.
      - `rsfilesrc`: Read stream from a file.

    - `gopbuffer`: Stores a minimum duration of data delimited by discrete GOPs (Group of Picture).

    - `inter`: 1:N wormhole for sending data from one pipeline to another within the same process using the [`StreamProducer` API](https://docs.rs/gstreamer-utils/latest/gstreamer_utils/struct.StreamProducer.html).
      - `intersink`: send data to one or more `intersrc` within the same process.
      - `intersrc`: receive data from an `intersink` in the same process.

    - `originalbuffer`:
      - `originalbufferrestore`: Restores the original buffer previously saved by `originalbuffersave`.
      - `originalbuffersave`: Saves a reference to the buffer in a meta so it can later be restored again after transformations such as downscaling before inference.

    - `sodium`: Elements to perform encryption and decryption using [libsodium](https://libsodium.org).
      - `sodiumdecrypter`: libsodium-based file decrypter.
      - `sodiumencrypter`: libsodium-based file encrypter.

    - `streamgrouper`: Filter element that makes all the incoming streams use the same group-id.

    - `threadshare`: Some popular threaded elements reimplemented using common thread-sharing infrastructure.
      - `ts-appsrc`: Thread-sharing app source.
      - `ts-audiotestsrc`: Thread-sharing audio test source.
      - `ts-blocking-adapter`: Converts a blocking downstream branch into an async backpressure.
      - `ts-clocksync`: Asynchronously synchronizes buffers to the clock.
      - `ts-input-selector`: Simple input selector element.
      - `ts-intersink`: Thread-sharing inter-pipelines sink.
      - `ts-intersrc`: Thread-sharing inter-pipelines source.
      - `ts-jitterbuffer`: Simple jitterbuffer.
      - `ts-proxysink`: Thread-sharing proxy sink.
      - `ts-proxysrc`: Thread-sharing proxy source.
      - `ts-queue`: Simple data queue.
      - `ts-rtpdtmfsrc`: Thread-sharing RTP DTMF packet (RFC2833) source.
      - `ts-tcpclientsrc`: Receives data over the network via TCP.
      - `ts-udpsink`: Thread-sharing UDP sink.
      - `ts-udpsrc`: Receives data over the network via UDP.


  * `mux`

    - `flavors`: FLV demuxer based on the [flavors](https://github.com/rust-av/flavors) library.
      - `rsflvdemux`: Demuxes FLV Streams.

    - `isobmff`: A MP4/ISOBMFF/CMAF muxer for generating fragmented (e.g. DASH/HLS media) and non-fragmented (MP4) files.
      - `cmafmux`: CMAF fragmented MP4 muxer.
      - `dashmp4mux`: DASH fragmented MP4 muxer.
      - `isofmp4mux`: ISO fragmented MP4 muxer.
      - `isomp4mux`: ISO MP4 muxer.
      - `onviffmp4mux`: ONVIF fragmented MP4 muxer.
      - `onvifmp4mux`: ONVIF MP4 muxer.


  * `net`

    - `aws`: Various elements for Amazon AWS services using the [AWS SDK](https://awslabs.github.io/aws-sdk-rust/) library
      - `awspolly`: Text to Speech filter, using AWS polly.
      - `awss3hlssink`: Streams HLS data to S3.
      - `awss3putobjectsink`: Writes an object to Amazon S3 using PutObject (mostly useful for small files).
      - `awss3sink`: Writes an object to Amazon S3.
      - `awss3src`: Reads an object from Amazon S3.
      - `awstranscribeparse`: an element parsing the packets of the AWS Transcriber service.
      - `awstranscriber`: an element wrapping the AWS Transcriber service.
      - `awstranscriber2`: Speech to Text filter, using AWS transcribe.
      - `awstranslate`: Translates text.

    - `dashsink2`: An element for generating MPEG-DASH streams.

    - `deepgram`: Wrapper elements to talk to the [Deepgram API](https://developers.deepgram.com/home)
      - `deepgramtranscriber`: an element wrapping the Deepgram Speech-to-Text service

    - `hlsmultivariantsink`: Create multi-variant HLS playlists with alternate renditions and variant streams.

    - `hlssink3`: An element for generating MPEG-TS HLS streams.
      - `hlscmafsink`: HTTP Live Streaming CMAF Sink.
      - `hlswebvttsink`: HTTP Live Streaming WebVTT Sink.

    - `icecast`:
      - `icecastsink`: shout2send-like element to send audio to an Icecast server

    - `mpegtslive`:
      - `mpegtslivesrc`: Wraps MPEG-TS sources such as `udpsrc` or `srtsrc` and provides a live clock based on the stream's PCR.

    - `ndi`: An [NDI](https://www.newtek.com/ndi/) plugin containing a source, sink and device provider.
      - `ndisink`: NewTek NDI Sink.
      - `ndisinkcombiner`: NewTek NDI sink audio/video combiner.
      - `ndisrc`: NewTek NDI Source.
      - `ndisrcdemux`: NewTek NDI source demuxer.

    - `onvif`: Various elements for parsing, RTP (de)payloading, overlaying of ONVIF timed metadata.
      - `onvifmetadatacombiner`: ONVIF metadata combiner.
      - `onvifmetadataextractor`: Extract the ONVIF GstMeta into a separate stream.
      - `onvifmetadataoverlay`: Renders ONVIF analytics meta over raw video frames.
      - `onvifmetadataparse`: Parses ONVIF Timed XML Metadata.
      - `rtponvifmetadatadepay`: ONVIF metadata RTP depayloader.
      - `rtponvifmetadatapay`: ONVIF metadata RTP payloader.

    - `quinn`: Transfer data over the network using QUIC
      - `quinnquicdemux`: Demultiplexes multiple streams and datagram for QUIC
      - `quinnquicmux`: Multiplexes multiple streams and datagram for QUIC
      - `quinnquicsink`: Send data over the network via QUIC
      - `quinnquicsrc`: Receive data over the network via QUIC
      - `quinnroqdemux`: Demultiplexes multiple RTP streams over QUIC
      - `quinnroqmux`: Multiplexes multiple RTP streams over QUIC
      - `quinnwtsink`: Send data over the network via WebTransport
      - `quinnwtsrc`: Receive data over the network via WebTransport

    - `raptorq`: Encoder/decoder element for RaptorQ RTP FEC mechanism.
      - `raptorqdec`: Performs FEC using RaptorQ (RFC6681, RFC6682).
      - `raptorqenc`: Performs FEC using RaptorQ (RFC6681, RFC6682).

    - `reqwest`: An HTTP source element based on the [reqwest](https://github.com/seanmonstar/reqwest) library.
      - `reqwesthttpsrc`: Read stream from an HTTP/HTTPS location.

    - `rtp`:
      - `rtpL16depay2`: Depayload 16-bit raw audio (L16) from RTP packets.
      - `rtpL16pay2`: Payload 16-bit raw audio (L16) into RTP packets (RFC 3551).
      - `rtpL20depay`: Depayload 20-bit raw audio (L20) from RTP packets.
      - `rtpL20pay`: Payload 20-bit raw audio (L20) into RTP packets (RFC 3551).
      - `rtpL24depay2`: Depayload 24-bit raw audio (L24) from RTP packets.
      - `rtpL24pay2`: Payload 24-bit raw audio (L24) into RTP packets (RFC 3551).
      - `rtpL8depay2`: Depayload 8-bit raw audio (L8) from RTP packets.
      - `rtpL8pay2`: Payload 8-bit raw audio (L8) into RTP packets (RFC 3551).
      - `rtpac3depay2`: Depayload an AC-3 Audio Stream from RTP packets (RFC 4184).
      - `rtpac3pay2`: Payload an AC-3 Audio Elementary Stream into RTP packets (RFC 4184).
      - `rtpamrdepay2`: Depayload an AMR audio stream from RTP packets (RFC 3267).
      - `rtpamrpay2`: Payload an AMR audio stream into RTP packets (RFC 3267).
      - `rtpav1depay`: Depayload AV1 from RTP packets.
      - `rtpav1pay`: Payload AV1 as RTP packets.
      - `rtpgccbwe`: RTP bandwidth estimator based on the Google Congestion Control algorithm.
      - `rtpjpegdepay2`: Depayload a JPEG Video stream from RTP packets (RFC 2435).
      - `rtpjpegpay2`: Payload a JPEG Video stream to RTP packets (RFC 2435).
      - `rtpklvdepay2`: Depayload an SMPTE ST 336 KLV metadata stream from RTP packets (RFC 6597).
      - `rtpklvpay2`: Payload an SMPTE ST 336 KLV metadata stream into RTP packets (RFC 6597).
      - `rtpmp2tdepay2`: Depayload an MPEG Transport Stream from RTP packets (RFC 2250).
      - `rtpmp2tpay2`: Payload an MPEG Transport Stream into RTP packets (RFC 2250).
      - `rtpmp4adepay2`: Depayload an MPEG-4 Audio bitstream (e.g. AAC) from RTP packets (RFC 3016).
      - `rtpmp4apay2`: Payload an MPEG-4 Audio bitstream (e.g. AAC) into RTP packets (RFC 3016).
      - `rtpmp4gdepay2`: Depayload MPEG-4 Generic elementary streams from RTP packets (RFC 3640).
      - `rtpmp4gpay2`: Payload an MPEG-4 Generic elementary stream into RTP packets (RFC 3640).
      - `rtpmpadepay2`: Depayload an MPEG Audio Elementary Stream from RTP packets (RFC 2038, RFC 2250).
      - `rtpmpapay2`: Payload an MPEG Audio Elementary Stream into RTP packets (RFC 2038, RFC 2250).
      - `rtpmparobustdepay2`: Depayload MPEG Audio Robust elementary streams from RTP packets (RFC 5219).
      - `rtpmpvdepay2`: Depayload an MPEG-1 or MPEG-2 Elementary Stream from RTP packets (RFC 2250).
      - `rtpmpvpay2`: Payload an MPEG-1 or MPEG-2 Elementary Stream into RTP packets (RFC 2250).
      - `rtpopusdepay2`: Depayload an Opus audio stream from RTP packets (RFC 7587).
      - `rtpopuspay2`: Payload an Opus audio stream into RTP packets (RFC 7587).
      - `rtppcmadepay2`: Depayload A-law from RTP packets (RFC 3551).
      - `rtppcmapay2`: Payload A-law Audio into RTP packets (RFC 3551).
      - `rtppcmudepay2`: Depayload µ-law from RTP packets (RFC 3551).
      - `rtppcmupay2`: Payload µ-law Audio into RTP packets (RFC 3551).
      - `rtprecv`: RTP sessions management (receiver).
      - `rtpsend`: RTP session management (sender).
      - `rtpsmpte291depay`: Depayload an SMPTE ST291-1 ANC stream from RTP packets (RFC 8331).
      - `rtpsmpte291pay`: Payload an SMPTE ST291-1 ANC stream into RTP packets (RFC 8331).
      - `rtpvp8depay2`: Depayload VP8 from RTP packets.
      - `rtpvp8pay2`: Payload VP8 as RTP packets.
      - `rtpvp9depay2`: Depayload VP9 from RTP packets.
      - `rtpvp9pay2`: Payload VP9 as RTP packets.
      - `rtpvrawdepay2`: Depayload a Raw Uncompressed Video Stream from RTP packets (RFC 4175).
      - `rtpvrawpay2`: Payload a Raw Uncompressed Video Stream into RTP packets (RFC 4175).

    - `rtsp`:
      - `rtspsrc2`: New Rust implementation of a Real Time Streaming Protocol (RTSP) (RFC 2326, 7826) source element.

    - `udp`:
      - `udpsrc2`: New version of the `udpsrc` with a lot better performance.

    - `webrtc`: WebRTC elements, with batteries included Sink elements for specific signalling protocols.
      - `awskvswebrtcsink`: WebRTC sink with kinesis video streams signaller.
      - `janusvrwebrtcsink`: WebRTC sink with Janus Video Room signaller.
      - `janusvrwebrtcsrc`: WebRTC source with Janus Video Room signaller.
      - `livekitwebrtcsink`: WebRTC sink with LiveKit signaller.
      - `livekitwebrtcsrc`: WebRTC source with LiveKit signaller.
      - `uepswebrtcsink`: WebRTC sink for signalling on an Unreal Engine Pixelstreaming compliant Signalling Server.
      - `webrtcsink`: WebRTC sink with custom protocol signaller.
      - `webrtcsrc`: WebRTC src.
      - `whepclientsrc`: WebRTC source element using WHEP Client as the signaller.
      - `whepserversink`: WebRTC sink with WHEP server signaller.
      - `whipclientsink`: WebRTC sink with WHIP client signaller.
      - `whipserversrc`: WebRTC source element using WHIP Server as the signaller.

    - `webrtcbin2`: new WebRTC elements with less threads based on `rtpsend`/`rtprecv`.
      - `webrtcrecv`: Receive streams using WebRTC.
      - `webrtcsend`: Send half of a WebRTC session.

    - `webrtchttp`: Simple WebRTC HTTP elements (WHIP/WHEP).
      - `whepsrc`: A bin to stream media using the WebRTC HTTP Egress Protocol (WHEP).
      - `whipsink`: A bin to stream RTP media using the WebRTC HTTP Ingestion Protocol (WHIP).


  * `text`

    - `accumulate`: A plugin for segmenting text, designed to work in a live context
      - `textaccumulate`: Accumulates text.

    - `ahead`: A plugin to display upcoming text buffers ahead.
      - `textahead`: Display upcoming text buffers ahead.

    - `json`: A plugin to convert a stream of JSON objects to a higher level wrapped NDJSON output.
      - `jsongstenc`: Wraps buffers containing any valid top-level JSON structures into higher level JSON objects, and outputs those as ndjson.
      - `jsongstparse`: Parses ndjson as output by jsongstenc.

    - `regex`: A regular expression text filter plugin.

    - `wrap`: A plugin to perform text wrapping with hyphenation.
      - `textwrap`: Breaks text into fixed-size lines, with optional hyphenation.


  * `utils`

    - `debugseimetainserter`: Element to insert SEI metadata into a video stream
      for debugging purposes.

    - `fallbackswitch`:
      - `fallbacksrc`: Element similar to `urisourcebin` that allows
        configuring a fallback audio/video if there are problems with the main
        source.
      - `fallbackswitch`: An element that allows falling back to different
        sink pads after a timeout based on the sink pads' priorities.

    - `livesync`: Element to maintain a continuous live stream from a
      potentially unstable source.

    - `togglerecord`: Element to enable starting and stopping multiple streams together.

    - `tracers`: Plugin with multiple tracers:
      - `buffer-lateness`: Records lateness of buffers and the reported
        latency for each pad in a CSV file. Contains a script for
        visualization.
      - `chrometracing`: GStreamer tracer that outputs events in Chrome JSON tracing format.
        The generated trace files can be opened in [perfetto](https://ui.perfetto.dev/).
      - `fmttracing`: GStreamer tracer that uses the tracing-subscriber fmt formatter.
        This tracer provides human-readable output using `tracing_subscriber::fmt`.
      - `memory-tracer`: Memory tracer.
      - `pad-push-timings`: Records push timings for pads.
      - `pcap-writer`: Writes network packets to a pcap file.
      - `perfetto`: GStreamer tracer that outputs events in Perfetto native format.
        The generated trace files can be opened in [perfetto](https://ui.perfetto.dev/).
      - `pipeline-snapshot`: Creates a .dot file of all pipelines in the
        application whenever requested.
      - `queue-levels`: Records queue levels for each queue in a CSV file.
        Contains a script for visualization.
      - `rusttracing`: GStreamer tracer that integrates with the Rust tracing ecosystem.
        This tracer provides spans for GStreamer pad operations, allowing
        for integration with the Rust tracing ecosystem.

    - `uriplaylistbin`: Helper bin to gaplessly play a list of URIs.


  * `video`

    - `cdg`: A parser and renderer for [CD+G karaoke data](https://docs.rs/cdg/0.1.0/cdg/).
      - `cdgdec`: CDG decoder.
      - `cdgparse`: CDG parser.

    - `closedcaption`: Plugin to deal with closed caption streams
      - `ccdetect`: Detects if a stream contains active Closed Captions.
      - `cctost2038anc`: Convert closed captions to ST-2038 ANC.
      - `cdpserviceinject`: Add or update service description data in a CDP.
      - `cea608overlay`: Overlay CEA-608 / EIA-608 closed captions over a
        video stream.
      - `cea608tocea708`: Upconvert a CEA-608 / EIA-608 caption stream to the equivalant
        CEA-708 caption stream.
      - `cea608tojson`: Convert CEA-608 / EIA-608 closed captions to a JSON
        stream.
      - `cea608tott`: Convert CEA-608 / EIA-608 closed captions to timed text.
      - `cea708mux`: Mux multiple CTA-708 / CEA-708 services together.
      - `cea708overlay`: Overlay CTA-708 / CEA-708 closed captions over a video
        stream.
      - `jsontovtt`: Convert JSON to timed text.
      - `mccenc`: Convert CEA-608 / EIA-608 and CEA-708 / EIA-708 closed captions to the MCC format.
      - `mccparse`: Parse CEA-608 / EIA-608 and CEA-708 / EIA-708 closed captions from the MCC format.
      - `sccenc`: Convert CEA-608 / EIA-608 closed captions to the MCC format.
      - `sccparse`: Parse CEA-608 / EIA-608 closed captions from the MCC format.
      - `st2038ancdemux`: Split individual ancillary streams from a ST-2038
        stream.
      - `st2038ancmux`: Mux togehter multiple ancillary ST-2038 streams.
      - `st2038anctocc`: Converts ST-2038 ANC to Closed Captions.
      - `st2038combiner`: Combines video input stream and ST2038 stream in GstAncillaryMeta.
      - `st2038extractor`: Extracts ST2038 stream in GstAncillaryMeta from video input stream.
      - `transcriberbin`: Convenience bin around transcriber elements like `aws_transcriber`.
      - `translationbin`: Convenience bin around transcriber and translation
        elements.
      - `tttocea608`: Convert timed text to CEA-608 / EIA-608 closed captions.
      - `tttocea708`: Convert timed text to CTA-708 / CEA-708 closed captions.
      - `tttojson`: Convert timed text to JSON.

    - `colorlut`: CPU and D3D12-based element for applying Adobe Cube LUTs to
      RGB video streams.

    - `dav1d`: AV1 decoder based on the [dav1d](https://code.videolan.org/videolan/dav1d) library.
      - `dav1ddec`: Decode AV1 video streams with dav1d.

    - `ffv1`: FFV1 decoder based on the [ffv1](https://github.com/rust-av/ffv1) library.
      - `ffv1dec`: Decode FFV1 video streams.

    - `gif`: A GIF encoder based on the [gif](https://github.com/image-rs/image-gif) library.
      - `gifdec`: Decodes GIF images.
      - `gifenc`: GIF encoder.

    - `gtk4`: A [GTK4](https://www.gtk.org) video sink that provides a `GdkPaintable` for UI integration.
      - `gtk4paintablesink`: A GTK 4 Paintable sink.

    - `hsv`: Plugin with various elements to work with video data in hue, saturation, value format
      - `hsvdetector`: Mark pixels that are close to a configured color in HSV format.
      - `hsvfilter`: Apply various transformations in the HSV colorspace.

    - `imagers`: multi-format plugin based on the [image](https://github.com/image-rs/image) crate
       - `imagersoverlay`: Overlays an image on top of a video stream. It is a reimplementation of `gdkpixbufoverlay` entirely in Rust.

    - `png`: PNG encoder based on the [png](https://github.com/image-rs/image-png) library.
      - `rspngenc`: PNG encoder.

    - `rav1e`: AV1 encoder based on the [rav1e](https://github.com/xiph/rav1e) library.
      - `rav1enc`: rav1e AV1 encoder.

    - `skia`:
      - `skiacompositor`: Video compositor based on [Skia](https://skia.org) graphics library.

    - `videofx`: Plugin with various video filters.
      - `colordetect`: A pass-through filter able to detect the dominant color(s) on incoming frames, using [color-thief](https://github.com/RazrFalcon/color-thief-rs).
      - `roundedcorners`: Element to make the corners of a video rounded via the alpha channel.
      - `videocompare`: Compare similarity of video frames. The element can use different hashing algorithms like [Blockhash](https://github.com/commonsmachinery/blockhash-rfc), [DSSIM](https://kornel.ski/dssim), and others.

    - `viuer`: Terminal-based video sink making use of the [viuer](https://github.com/atanunq/viuer) crate.
      - `viuersink`: Renders video to the terminal using viuer.

    - `vvdec`: VVC/H.266 decoder based on [VVdeC](https://github.com/fraunhoferhhi/vvdec), the Fraunhofer Versatile Video Decoder.

    - `webp`: WebP decoder based on the [libwebp-sys-2](https://github.com/qnighy/libwebp-sys2-rs) library.
      - `rswebpdec`: Decodes potentially animated WebP images.



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
$ cargo cinstall -p gst-plugin-cdg --prefix=/usr
```

This will install the plugin to `/usr/lib/gstreamer-1.0`.
You can use `--libdir` to pass a custom `lib` directory
such as `/usr/lib/x86_64-linux-gnu` for example.

Note that you can also just use `cargo` directly to build Rust static libraries
and shared C libraries. `cargo-c` is mostly useful to build static C libraries
and generate `pkg-config` files.

In case cargo complains about dependency versions after a `git pull`, `cargo update` may
be able to resolve those.

## LICENSE

gst-plugins-rs and all crates contained in here are licensed under one of the
following licenses

 * Mozilla Public License Version 2.0 ([LICENSE-MPL-2.0](LICENSE-MPL-2.0) or http://opensource.org/licenses/MPL-2.0)
 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

GStreamer itself is licensed under the Lesser General Public License version
2.1 or (at your option) any later version: https://www.gnu.org/licenses/lgpl-2.1.html

## Contribution

Any kinds of contributions are welcome as a merge request.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in gst-plugins-rs by you shall be licensed under the license of
the plugin it is added to.

For new plugins the MPL-2 license is preferred.
