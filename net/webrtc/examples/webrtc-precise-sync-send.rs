use anyhow::{bail, Context};
use futures::prelude::*;
use gst::prelude::*;
use gst_rtp::prelude::*;
use tracing::{debug, error, info};
use url::Url;

const VIDEO_PATTERNS: [&str; 3] = ["ball", "smpte", "snow"];

#[derive(Debug, Default, Clone, clap::Parser)]
struct Args {
    #[clap(
        long,
        help = "Number of audio streams. Use 0 to disable audio",
        default_value = "1"
    )]
    pub audio_streams: usize,

    #[clap(
        long,
        help = "Number of video streams. Use 0 to disable video",
        default_value = "1"
    )]
    pub video_streams: usize,

    #[clap(long, help = "Force video caps (ex. 'video/x-h264')")]
    pub video_caps: Option<String>,

    #[clap(long, help = "Use RFC 6051 64-bit NTP timestamp RTP header extension.")]
    pub enable_rapid_sync: bool,

    #[clap(
        long,
        help = "Maximum duration in seconds to wait for clock synchronization",
        default_value = "5"
    )]
    pub clock_sync_timeout: u64,

    #[clap(long, help = "NTP server host", default_value = "pool.ntp.org")]
    pub ntp_server: String,

    #[clap(long, help = "Signalling server host", default_value = "localhost")]
    pub server: String,

    #[clap(long, help = "Signalling server port", default_value = "8443")]
    pub port: u32,

    #[clap(long, help = "use tls")]
    pub use_tls: bool,
}

impl Args {
    fn scheme(&self) -> &str {
        if self.use_tls {
            "wss"
        } else {
            "ws"
        }
    }
}

#[derive(Debug, Default)]
struct App {
    args: Args,
    pipeline: Option<gst::Pipeline>,
}

impl App {
    fn new(args: Args) -> Self {
        App {
            args,
            ..Default::default()
        }
    }

    #[inline(always)]
    fn pipeline(&self) -> &gst::Pipeline {
        self.pipeline.as_ref().expect("Set in prepare")
    }

    async fn prepare_and_run(&mut self) -> anyhow::Result<()> {
        self.prepare().await.context("Preparing")?;
        self.run().await.context("Running")?;

        Ok(())
    }

    async fn prepare(&mut self) -> anyhow::Result<()> {
        debug!("Preparing");

        debug!("Syncing to NTP clock {}", self.args.ntp_server);

        // Create the NTP clock and wait for synchronization.
        let clock = tokio::task::spawn_blocking({
            let clock =
                gst_net::NtpClock::new(None, &self.args.ntp_server, 123, gst::ClockTime::ZERO);
            let clock_sync_timeout = gst::ClockTime::from_seconds(self.args.clock_sync_timeout);
            move || -> anyhow::Result<gst_net::NtpClock> {
                clock.wait_for_sync(clock_sync_timeout)?;

                Ok(clock)
            }
        })
        .await
        .context("Syncing to NTP clock")??;

        info!("Synced to NTP clock");

        self.pipeline = Some(gst::Pipeline::new());
        self.pipeline().use_clock(Some(&clock));

        // Set the base time of the pipeline statically to zero so that running
        // time and clock time are the same and timeoverlay can be used to render
        // the clock time over the video frames.
        //
        // This is needed for no other reasons.
        self.pipeline().set_base_time(gst::ClockTime::ZERO);
        self.pipeline().set_start_time(gst::ClockTime::NONE);

        let signaller_url = Url::parse(&format!(
            "{}://{}:{}",
            self.args.scheme(),
            self.args.server,
            self.args.port,
        ))?;

        let webrtcsink = gst::ElementFactory::make("webrtcsink")
            // See:
            // * https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/issues/497
            // * https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/3301
            .property("do-fec", false)
            .build()
            .context("Creating webrtcsink")?;

        self.pipeline().add(&webrtcsink).unwrap();

        let signaller = webrtcsink.property::<gst::glib::Object>("signaller");
        signaller.set_property("uri", signaller_url.as_str());

        signaller.connect("webrtcbin-ready", false, |args| {
            let webrtcbin = args[2].get::<gst::Element>().unwrap();

            let rtpbin = webrtcbin
                .downcast_ref::<gst::Bin>()
                .unwrap()
                .by_name("rtpbin")
                .unwrap();

            // Use local pipeline clock time as RTP NTP time source instead of using
            // the local wallclock time converted to the NTP epoch.
            rtpbin.set_property_from_str("ntp-time-source", "clock-time");

            // Use the capture time instead of the send time for the RTP / NTP timestamp
            // mapping. The difference between the two options is the capture/encoder/etc.
            // latency that is introduced before sending.
            rtpbin.set_property("rtcp-sync-send-time", false);

            None
        });

        webrtcsink.connect("encoder-setup", true, |args| {
            let enc = args[3].get::<gst::Element>().unwrap();
            if enc.is::<gst_audio::AudioEncoder>() {
                // Make sure the audio encoder tracks upstream timestamps.
                enc.set_property("perfect-timestamp", false);
            }

            Some(true.to_value())
        });

        if self.args.enable_rapid_sync {
            webrtcsink.connect("payloader-setup", false, |args| {
                let payloader = args[3].get::<gst::Element>().unwrap();

                // Add RFC6051 64-bit NTP timestamp RTP header extension.
                let hdr_ext = gst_rtp::RTPHeaderExtension::create_from_uri(
                    "urn:ietf:params:rtp-hdrext:ntp-64",
                )
                .expect("Creating NTP 64-bit RTP header extension");
                hdr_ext.set_id(1);
                payloader.emit_by_name::<()>("add-extension", &[&hdr_ext]);

                Some(true.into())
            });
        }

        for idx in 0..self.args.audio_streams {
            let audiosrc = gst::ElementFactory::make("audiotestsrc")
                .property("is-live", true)
                .property("freq", (idx + 1) as f64 * 440.0)
                .property("volume", 0.2f64)
                .build()
                .context("Creating audiotestsrc")?;
            self.pipeline().add(&audiosrc).context("Adding audiosrc")?;

            audiosrc
                .link_pads(None, &webrtcsink, Some("audio_%u"))
                .context("Linking audiosrc")?;
        }

        for idx in 0..self.args.video_streams {
            let videosrc = gst::ElementFactory::make("videotestsrc")
                .property("is-live", true)
                .property_from_str("pattern", VIDEO_PATTERNS[idx % VIDEO_PATTERNS.len()])
                .build()
                .context("Creating videotestsrc")?;
            let video_overlay = gst::ElementFactory::make("timeoverlay")
                .property_from_str("time-mode", "running-time")
                .build()
                .context("Creating timeoverlay")?;

            self.pipeline()
                .add_many([&videosrc, &video_overlay])
                .expect("adding video elements");

            videosrc
                .link_filtered(
                    &video_overlay,
                    &gst::Caps::builder("video/x-raw")
                        .field("width", 800i32)
                        .field("height", 600i32)
                        .build(),
                )
                .context("Linking videosrc to timeoverlay")?;

            video_overlay
                .link_pads(None, &webrtcsink, Some("video_%u"))
                .context("Linking video overlay")?;
        }

        if let Some(ref video_caps) = self.args.video_caps {
            webrtcsink.set_property("video-caps", &gst::Caps::builder(video_caps).build());
        }

        Ok(())
    }

    async fn run(&mut self) -> anyhow::Result<()> {
        debug!("Running");

        let bus = self.pipeline().bus().context("Getting the pipeline bus")?;
        let mut bus_stream = bus.stream();

        self.pipeline()
            .call_async_future(|pipeline| pipeline.set_state(gst::State::Playing))
            .await
            .context("Setting pipeline to Playing")?;

        while let Some(bus_msg) = bus_stream.next().await {
            use gst::MessageView::*;

            match bus_msg.view() {
                Error(msg) => {
                    let err = msg.error();
                    let src_name = msg.src().map(|src| src.name());

                    bail!(
                        "Element {} error message: {err:#}",
                        src_name.as_deref().unwrap_or("UNKNOWN"),
                    );
                }
                Latency(msg) => {
                    info!(
                        "Latency requirements have changed for element {}",
                        msg.src()
                            .map(|src| src.name())
                            .as_deref()
                            .unwrap_or("UNKNOWN"),
                    );
                    if let Err(err) = self.pipeline().recalculate_latency() {
                        error!(%err, "Error recalculating latency");
                    }
                }
                _ => (),
            }
        }

        Ok(())
    }

    /// Tears this `App` down and deallocates all its resources by consuming `self`.
    async fn teardown(mut self) {
        debug!("Tearing down");

        if let Some(pipeline) = self.pipeline.take() {
            let _ = pipeline
                .call_async_future(|pipeline| pipeline.set_state(gst::State::Null))
                .await;
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use clap::Parser;
    use tracing_subscriber::prelude::*;

    let args = Args::parse();

    tracing_log::LogTracer::init().context("Setting logger")?;
    let env_filter = tracing_subscriber::EnvFilter::try_from_env("WEBRTC_PRECISE_SYNC_SEND_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_target(true)
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::NEW
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        );
    let subscriber = tracing_subscriber::Registry::default()
        .with(env_filter)
        .with(fmt_layer);
    tracing::subscriber::set_global_default(subscriber).context("Setting tracing subscriber")?;

    gst::init()?;
    gstrswebrtc::plugin_register_static()?;
    gstrsrtp::plugin_register_static()?;

    debug!("Starting");

    let mut res = Ok(());
    let mut app = App::new(args);

    {
        let ctrl_c = tokio::signal::ctrl_c().fuse();
        tokio::pin!(ctrl_c);

        let prepare_and_run = app.prepare_and_run().fuse();
        tokio::pin!(prepare_and_run);

        futures::select! {
            _ctrl_c_res = ctrl_c => {
                info!("Shutting down due to user request");
            }
            app_res = prepare_and_run => {
                if let Err(ref err) = app_res {
                    error!("Shutting down due to application error: {err:#}");
                } else {
                    info!("Shutting down due to application termination");
                }

                res = app_res;
            }
        }
    }

    app.teardown().await;

    debug!("Quitting");

    unsafe {
        // Needed for certain tracers to write data
        gst::deinit();
    }

    res
}
