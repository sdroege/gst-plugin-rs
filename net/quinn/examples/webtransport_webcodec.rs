use clap::Parser;
use gst::prelude::*;
use std::io::Write;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    certificate_file: String,

    #[arg(short = 'k', long)]
    private_key_file: String,

    // Changing any of the below 3 arguments will require a
    // corresponding change in the front end application.
    #[arg(short, long, default_value = "[::]")]
    address: String,

    #[arg(short, long, default_value_t = 4433)]
    port: u16,

    #[arg(short, long, default_value_t = 600)]
    bitrate: u32,
}

// Needs to be tested with Chrome/Chromium with certificates setup.
//
// See https://github.com/GoogleChrome/samples/blob/gh-pages/webtransport/webtransport_server.py
// Reproducing the certificate information from the above link below in
// case that becomes inaccessible down the line.
//
// HTTP/3 always operates using TLS, meaning that running a WebTransport over
// HTTP/3 server requires a valid TLS certificate.  The easiest way to do this
// is to get a certificate from a real publicly trusted CA like
// <https://letsencrypt.org/>.
// https://developers.google.com/web/fundamentals/security/encrypt-in-transit/enable-https
// contains a detailed explanation of how to achieve that.
//
// As an alternative, Chromium can be instructed to trust a self-signed
// certificate using command-line flags.  Here are step-by-step instructions on
// how to do that:
//
//   1. Generate a certificate and a private key:
//         openssl req -newkey rsa:2048 -nodes -keyout certificate.key \
//                   -x509 -out certificate.pem -subj '/CN=Test Certificate' \
//                   -addext "subjectAltName = DNS:localhost"
//
//   2. Compute the fingerprint of the certificate:
//         openssl x509 -pubkey -noout -in certificate.pem |
//                   openssl rsa -pubin -outform der |
//                   openssl dgst -sha256 -binary | base64
//      The result should be a base64-encoded blob that looks like this:
//          "Gi/HIwdiMcPZo2KBjnstF5kQdLI5bPrYJ8i3Vi6Ybck="
//
//   3. Pass a flag to Chromium indicating what host and port should be allowed
//      to use the self-signed certificate.  For instance, if the host is
//      localhost, and the port is 4433, the flag would be:
//         --origin-to-force-quic-on=localhost:4433
//
//   4. Pass a flag to Chromium indicating which certificate needs to be trusted.
//      For the example above, that flag would be:
//         --ignore-certificate-errors-spki-list=Gi/HIwdiMcPZo2KBjnstF5kQdLI5bPrYJ8i3Vi6Ybck=
//
// See https://www.chromium.org/developers/how-tos/run-chromium-with-flags for
// details on how to run Chromium with flags.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    gst::init()?;

    let args = Args::parse();

    let pipeline_str = format!(
        "videotestsrc ! videorate ! videoscale ! video/x-raw,width=640,height=360,framerate=15/1 ! queue ! x264enc bitrate={} speed-preset=ultrafast tune=zerolatency key-int-max=15 ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au,profile=constrained-baseline ! queue name=enc_queue ! quinnwtsink role=server address={} port={} certificate-file={} private-key-file={}",
        args.bitrate, args.address, args.port, args.certificate_file, args.private_key_file
    );

    let pipeline = gst::parse::launch(&pipeline_str)
        .expect("Failed to create pipeline")
        .dynamic_cast::<gst::Pipeline>()
        .unwrap();

    let queue = pipeline.by_name("enc_queue").expect("Could not find queue");
    let srcpad = queue.static_pad("src").expect("Could not get src pad");

    srcpad.add_probe(gst::PadProbeType::BUFFER, |_, probe_info| {
        if let Some(gst::PadProbeData::Buffer(ref mut buffer)) = probe_info.data {
            const FRAME_TYPE_KEY: u16 = 0x0001;
            const FRAME_TYPE_DELTA: u16 = 0xFFFF;

            let frame_type = if buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                FRAME_TYPE_DELTA
            } else {
                FRAME_TYPE_KEY
            };

            let size = buffer.size() as u32;

            // Minimal custom header corresponding to `EncodedVideoChunk`.
            // See https://developer.mozilla.org/en-US/docs/Web/API/EncodedVideoChunk
            let mut header = Vec::with_capacity(6); // 2 + 4 bytes
            header.write_all(&frame_type.to_be_bytes()).unwrap();
            header.write_all(&size.to_be_bytes()).unwrap();

            let buffer = buffer.make_mut();
            buffer.prepend_memory(gst::Memory::from_mut_slice(header));
        }

        gst::PadProbeReturn::Ok
    });

    pipeline.set_state(gst::State::Playing)?;

    pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), "webtransport-webcodec-demo");

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                pipeline.debug_to_dot_file_with_ts(
                    gst::DebugGraphDetails::all(),
                    "webtransport-webcodec-demo-error",
                );
                eprintln!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                break;
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;

    Ok(())
}
