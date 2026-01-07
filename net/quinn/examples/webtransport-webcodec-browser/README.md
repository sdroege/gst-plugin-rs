# WebTransport + WebCodec demo

This is a demo example of sending an H.264 video stream, decoding using [WebCodec](https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API) and then rendering it in the browser. The server runs a GStreamer pipeline for sending H264 video via [WebTransport](https://w3c.github.io/webtransport/) using the `quinnwtsink` element and the browser demo page is in React.

## System prerequisites

- Rust
- pnpm
- Chrome/Chromium

Due to WebTransport running over HTTP/3 QUIC, this demo requires TLS authentication. A certificate issued by a known certificate authority is required, or alternatively, generate a self-signed certificate to run the demo locally.

## Demo explanation

- `webtransport_webcodec` demo application effectively executes the below GStreamer pipeline for sending the H264 stream via WebTransport. Check the code for the complete pipeline.

```
videotestsrc ! videorate ! videoscale ! video/x-raw,width=640,height=360,framerate=15/1 ! queue ! x264enc ! h264parse ! quinnwtsink
```

-  A simple 6-byte header is added, with information about the frame type and payload size, before serialization. The header contains (`type`: [keyframe, delta]), as required by `EncodedVideoCunk`([docs](https://developer.mozilla.org/en-US/docs/Web/API/EncodedVideoChunk)).

- React demo app uses some hard-coded values for simplicity based on the GStreamer pipeline. For example, [VideoDecoder](https://developer.mozilla.org/en-US/docs/Web/API/VideoDecoder/configure) is configured as below.

```jsx
    decoder.configure({
      codec: 'avc1.42E016', // H.264 Constrained Baseline Profile
      codedWidth: 640,
      codedHeight: 360,
      optimizeForLatency: true
    });
```

## Running with Chromium

For using a self signed certificate, see [Google Chrome Samples](https://github.com/GoogleChrome/samples/blob/gh-pages/webtransport/webtransport_server.py).

## Running the demo

Build `gst-plugin-quinn` and run Rust server application. The certificate files must be provided for running the application.

```sh
cd gst-plugins-rs
cargo build -p gst-plugin-quinn
GST_DEBUG=3 GST_PLUGIN_PATH=target/debug cargo run -p gst-plugin-quinn --example webtransport_webcodec -- --certificate-file certificate.pem --private-key-file certificate.key
```

In a separate terminal, run the React development server.

Copy the certificate files provided to the Rust server application to the `webtransport-webcodec-browser` directory. This is required, and the certificate file names are hard-coded in `vite.config.js`. Change them if you intend to use different names for the certificate and private key files.

```sh
cd gst-plugins-rs/net/quinn/examples/webtransport-webcodec-browser/
pnpm run build
pnpm run dev
```

Open `localhost:3001`.
