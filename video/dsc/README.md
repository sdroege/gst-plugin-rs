# GStreamer DSC Plugin

A GStreamer plugin for Digitally Signed Content (DSC) that provides cryptographic signing and verification for encoded video data using Rust RSA and certificate parsing libraries.

Mechanisms for trustworthy authentication and verification of video content were recently developed by JVET for inclusion in video coding standards. This is realized by three supplemental enhancement information (SEI) messages that attach cryptographic signatures to flexible chunks of a video stream at the network abstraction layer (NAL) unit level.

The following are the references used for this implementation:
* Paper: https://www.hhi.fraunhofer.de/fileadmin/Events/2025/IBC_2025/IBC2025PaperAuthentication_HHI.pdf
* JVET Specs: https://www.jvet-experts.org/doc_end_user/documents/40_Geneva/wg11/JVET-AN1019-v1.zip
* DSC implementation in VVC VTM: https://vcgit.hhi.fraunhofer.de/jvet/VVCSoftware_VTM/-/releases/VTM-23.13

## Elements

- **DSC Signer**: Signs data packets based on the specified hash method
- **DSC Verifier**: Verifies the signed data

## Substream boundaries and EOS

`dscsigner` creates a signature when the current substream reaches
`substream-length` buffers. If the stream ends before the current substream
is complete, the final buffers are forwarded without a signature:
`dscsigner` posts a warning at EOS and `dscverifier` reports the final buffers
as unverified. Make the stream length a multiple of `substream-length` if every
buffer must be signed, or use `substream-length=1` to sign every buffer.

If a stream has no DSC initialization metadata at all, `dscverifier` forwards
it and posts a one-time warning plus a `dsc-verification-result` message that
the data is unverified. This keeps unsigned streams usable while making the
lack of verification visible.

## Buffering limit

In its default buffering mode, `dscverifier` holds a substream until its
verification metadata arrives. `max-buffering-time` (default 5 seconds, `0` for
unlimited) bounds how much media it holds. If the limit is exceeded, the
buffered data and the current buffer are forwarded, the substream is reported
as failed with `verified=false` and `error="max-buffering-time exceeded"`, and
the remaining data of that substream passes through until the next
initialization metadata starts a new one. Setting `buffer=false` disables
buffering entirely.

## Crypto stack

- Runtime implementation (`dscsigner` / `dscverifier`) uses pure-Rust crypto crates (`rsa`, `sha1`, `sha2`, `x509-parser`, `rustls-pemfile`).
- Test helper code is also OpenSSL-free and uses `rsa` + `rcgen`.
- OpenSSL is not required as a dependency for this plugin runtime or test helper logic.

## Generate certificate samples
The plugin accepts standard PEM key/certificate files. You can generate them with any tooling.

For tests and development, see the helper implementation in `tests/dsc.rs` (`create_test_keys()`), which generates RSA keys and self-signed certificates with Rust crates only.

If you prefer manual CLI generation, the following OpenSSL commands are optional:

### Create CA
```bash
openssl genrsa -out example_ca.key 4096
openssl genrsa -aes256 -out example_ca.key 4096
openssl req -x509 -new -nodes -key example_ca.key -sha256 -days 1826 -out example_ca.crt
openssl x509 -in example_ca.crt -noout -pubkey -out example_ca.pub
```

### Create Content Provider certificate
```bash
openssl genrsa -out example_content.key 4096
openssl req -new -key example_content.key -out example_content.csr
openssl x509 -req -in example_content.csr -CA example_ca.crt -CAkey example_ca.key -out example_content.crt -days 730 -sha256
openssl x509 -in example_content.crt -noout -pubkey -out example_content.pub
```

## Example
```bash
GST_DEBUG=dscverifier:4 \
gst-launch-1.0 videotestsrc pattern=ball num-buffers=30 ! "video/x-raw,framerate=30/1" ! videoconvert ! x265enc key-int-max=5 ! dscsigner private-key-path=./example_content.key public-key-uri=./example_content.crt substream-length=5 ! dscverifier key-store-path="$(pwd)" ! h265parse ! avdec_h265 ! videoconvert ! autovideosink
```
