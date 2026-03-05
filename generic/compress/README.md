# Compress

The Compress plugin provides general-purpose lossless compression elements for GStreamer streams. It works best with raw data streams and includes the following element pairs:

- `zlibcompress` / `zlibdecompress` — zlib compression (deflate with checksum)
- `deflatecompress` / `deflatedecompress` — raw deflate compression (no checksum)
- `brotlicompress` / `brotlidecompress` — Brotli compression

## Concepts

### With GStreamer Data Protocol (GDP)

Use GDP when you need to preserve metadata (caps, buffer metadata, events) with your compressed data. GDP encapsulates the data before compression, so everything is available after decompression without additional configuration. This approach is ideal for file storage.

### Without GStreamer Data Protocol

Use this approach when metadata is handled separately, such as when multiplexing compressed streams into a container format. The compressed data contains only buffer content, making it lightweight but requiring you to specify caps manually during decompression.

## Usage Examples

### Using GStreamer Data Protocol (GDP)

**Compress first, then encapsulate:**

GDP wraps the compressed data and provides framing for decompression. This keeps caps and metadata separate from the compressed stream. The decompressor can read the metadata directly from GDP without needing the compressed data itself.

Compression pipeline:

```
gst-launch-1.0 videotestsrc num-buffers=100 ! zlibcompress ! gdppay ! filesink location=myfile
```

Decompression pipeline:

```
gst-launch-1.0 filesrc location=myfile ! gdpdepay ! zlibdecompress ! videoconvert ! autovideosink
```

**Encapsulate first, then compress:**

GDP wraps the data before compression, so all caps and metadata are compressed along with the buffer content. After decompression, all information is recovered automatically. This approach compresses all data and does not require any out-of-band data.

Compression pipeline:

```
gst-launch-1.0 videotestsrc num-buffers=100 ! gdppay ! zlibcompress ! filesink location=myfile
```

Decompression pipeline:

```
gst-launch-1.0 filesrc location=myfile ! zlibdecompress ! gdpdepay  ! videoconvert ! autovideosink
```

### Without GStreamer Data Protocol

The compressed file contains only buffer content. Metadata and events are not compressed. This approach is useful when compressing for container formats or multiplexing, but not ideal for standalone file storage (you must specify caps manually during decompression).

Direct compress + decompress:

```
gst-launch-1.0 videotestsrc num-buffers=300 ! zlibcompress ! zlibdecompress  ! videoconvert ! autovideosink
```

Compression pipeline:

```
gst-launch-1.0 videotestsrc num-buffers=300 ! "video/x-raw, format=RGB, width=320, height=240, framerate=30/1" ! zlibcompress ! filesink location=myfile
```

Decompression pipeline:
You must specify caps manually, since this information is not part of the compressed file.

```
gst-launch-1.0 filesrc location=myfile ! zlibdecompress ! "video/x-raw, format=RGB, width=320, height=240, framerate=30/1" ! videoconvert ! autovideosink
```

