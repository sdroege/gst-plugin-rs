/// rtpbin2-precise-sync-recv
///
/// This example renders raw audio streams received RTP with clock signalling ([RFC7273]).
/// The SDP is expected on `stdin`.
///
/// Use `rtpbin2-precise-sync-send` as a sender.
///
/// Note that this receiver expects muxed RTP & RTCP. If this is not a requirement,
/// separate `udpsrc2`s should be used for incoming RTCP streams.
///
/// ## Examples
///
/// ### Usage
///
/// ````
/// cargo r --example rtpbin2-precise-sync-recv -- --help
/// ````
///
/// ### Render the audio stream(s) advertised in the provided SDP
///
/// ````
/// cargo r --example rtpbin2-precise-sync-recv -- < combined_audio_streams.sdp
/// ````
///
/// ### Declaring the local (system) clock as being synced to a ref. clock
///
/// If the system clock is known to be sync to a reference clock, it is more accurate
/// to sync to this clock instead of using a GStreamer Ntp / Ptp clock.
///
/// ````
/// cargo r --example rtpbin2-precise-sync-recv -- \
///     --local-refclk ntp=pool.ntp.org \
///         < combined_audio_streams.sdp
/// ````
///
/// or
///
/// ````
/// cargo r --example rtpbin2-precise-sync-recv -- \
///     --local-refclk ptp=IEEE1588-2008:00-00-00-00-00-00-00-2A:1 \
///         < combined_audio_streams.sdp
/// ````
///
/// [RFC7273]: https://www.rfc-editor.org/rfc/rfc7273.html
// SPDX-License-Identifier: MPL-2.0
use anyhow::{Context, bail};
use clap::Parser;
use futures::prelude::*;
use gst::glib;
use gst::prelude::*;
use itertools::Itertools as _;
use sdp_types::{MediaClockSource, ReferenceClock};

use std::collections::BTreeMap;
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const RTP_ID: &str = "rtp-id";

const DEFAULT_NTP_PORT: u16 = 123;

#[derive(clap::Parser, Debug)]
#[command(
    version,
    about = "Receives audio streams from RTP with clock signalling (RFC7273). Use `rtpbin2-precise-sync-send` as a sender"
)]
pub struct Args {
    #[clap(long, help = "RTP jitterbuffer latency (ms)", default_value = "40")]
    pub rtp_latency: u32,

    #[clap(long, help = "Disable audio visualizer")]
    pub disable_visualizer: bool,

    #[clap(
        long,
        help = "Which clock the local (system) clock is synchronized to. E.g. 'ntp=pool.ntp.org'"
    )]
    pub local_refclk: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Clock {
    refclk: ReferenceClock,
    mediaclk: Option<MediaClockSource>,
    ssrc: Option<u32>,
}

impl Clock {
    // Builds a `Clock` with the specified RFC7273 description
    fn new(refclk: ReferenceClock) -> Clock {
        Clock {
            refclk,
            mediaclk: None,
            ssrc: None,
        }
    }

    // Builds a `Clock` with the specified RFC7273 description targeting the specified SSRC
    fn with_ssrc(refclk: ReferenceClock, ssrc: u32) -> Clock {
        Clock {
            refclk,
            mediaclk: None,
            ssrc: Some(ssrc),
        }
    }

    fn set_mediaclk(&mut self, mediaclk: MediaClockSource) {
        self.mediaclk = Some(mediaclk);
    }
}

impl Clock {
    // Gets a synchronized `gst::Clock` matching this RFC7273 described `Clock`
    fn get_synced(&self) -> anyhow::Result<gst::Clock> {
        use sdp_types::{Ntp, NtpServerAddr, Ptp, PtpDomain, PtpServer};

        let clock = match &self.refclk {
            ReferenceClock::Local => glib::Object::builder::<gst::SystemClock>()
                // produce time relative to UNIX epoch
                .property("clock-type", gst::ClockType::Realtime)
                .build()
                .upcast::<gst::Clock>(),
            ReferenceClock::Ntp(Ntp {
                server: NtpServerAddr::HostPort { hostname, port },
            }) => gst_net::NtpClock::new(
                None,
                hostname,
                port.unwrap_or(DEFAULT_NTP_PORT) as _,
                gst::ClockTime::ZERO,
            )
            .upcast::<gst::Clock>(),
            ReferenceClock::Ptp(Ptp {
                version: _,
                server:
                    PtpServer::GmidDomain {
                        gmid: _,
                        domain: Some(domain),
                    },
            }) => {
                let (name, number) = match domain {
                    PtpDomain::DomainName { name } => (Some(name.as_str()), 0),
                    PtpDomain::DomainNumber(number) => (None, *number),
                };
                gst_net::PtpClock::init(None, &[])?;
                gst_net::PtpClock::new(name, number as _)?.upcast()
            }
            other => panic!("Unsupported clock {other:?}"),
        };

        clock
            .wait_for_sync(5.seconds())
            .with_context(|| format!("Syncing to {}", self.refclk))?;

        let now = clock.time();
        eprintln!(
            "Synced to {}: now {now} ({}.{})",
            self.refclk,
            now.seconds(),
            now.nseconds() % *gst::ClockTime::SECOND
        );

        Ok(clock)
    }

    /// Builds & synchronises a `gst::Clock` matching `self`
    /// and populates the `clock_map` & `pt_caps` accordingly.
    ///
    /// If the `local-refclk` CLI argument is not defined, the clock
    /// is obtained and synced using [`Self::get_synced`].
    ///
    /// If the `local-refclk` CLI argument is defined and matches `self`,
    /// a `SystemClock` (with `RealTime` 'clock-type') is used instead.
    ///
    /// The `clock_map` & `pt_caps` are populated as follows:
    ///
    /// * `clock_map`: add the synchronised clock, regardless of the level
    ///   at which it applies. The field name is the value of the `ts-refclk`
    ///   attribute from the SDP message (e.g. 'ntp=traceable') and the field
    ///   value is the synchronised clock itself.
    /// * `pt_map`: add reference clock attributes:
    ///   * if `self.ssrc` is not defined, we are dealing with a session or
    ///     media level clock definition. So the attributes are added with
    ///     the `a-` prefix, which is the usual pt-map convention for additional
    ///     SDP attributes.
    ///   * if `self.ssrc` is defined, this means we are dealing with a SSRC
    ///     specific clock definition. So the attributes are added with the
    ///     `a-ssrc-{srrc}-` prefix, which is the usual pt-map convention for
    ///     SSRC specific SDP attributes.
    fn sync_and_add(
        self,
        args: &Args,
        clock_map: &mut gst::Structure,
        pt_caps_builder: gst::caps::Builder<gst::caps::NoFeature>,
    ) -> anyhow::Result<gst::caps::Builder<gst::caps::NoFeature>> {
        let refclk_str = self.refclk.to_string();
        if !clock_map.has_field(&refclk_str) {
            if args
                .local_refclk
                .as_ref()
                .is_some_and(|local_refclk| local_refclk == &refclk_str)
            {
                // system clock is synchronised to this refclk
                // => use it instead of a GStreamer Ntp or Ptp clock which is likely less accurate
                eprintln!("Using local (system) clock for {}", self.refclk);
                clock_map.set(
                    &refclk_str,
                    glib::Object::builder::<gst::SystemClock>()
                        .property("clock-type", gst::ClockType::Realtime)
                        .build()
                        .upcast::<gst::Clock>(),
                );
            } else {
                clock_map.set(&refclk_str, self.get_synced()?);
            }
        }
        let mediaclk_str_opt = self.mediaclk.map(|mediaclk| mediaclk.to_string());
        let caps_builder = if let Some(ssrc) = self.ssrc {
            pt_caps_builder
                .field(format!("a-ssrc-{ssrc}-ts-refclk"), refclk_str)
                .field_if_some(format!("a-ssrc-{ssrc}-mediaclk"), mediaclk_str_opt)
        } else {
            pt_caps_builder
                .field("a-ts-refclk", refclk_str)
                .field_if_some("a-mediaclk", mediaclk_str_opt)
        };

        Ok(caps_builder)
    }
}

fn prepare_expected_medias(
    args: &Args,
    pipeline: &gst::Pipeline,
    inbound_funnel: &gst::Element,
    rtprecv: &gst::Element,
    outbound_rtcp_tee: &gst::Element,
    sdp: &sdp_types::Session,
    session_id: u32,
) -> anyhow::Result<()> {
    use sdp_types::{Rtcp, RtpMap, Ssrc, SsrcAttribute};

    let Some(ref connection) = sdp.connection else {
        bail!("No connections for session {session_id}");
    };
    let connection_address = connection.connection_address.as_str();

    // We need to inform rtprecv of details provided by the SDP
    // To do so, we will use two `gst::Structure`s:
    //
    // * x-rtp2-clock-map: this is a collection of all the reference clocks
    //   declared in the SDP, regardless of the level at which they apply.
    // * x-rtp2-pt-map: this is the usual media-level Caps collection containing
    //   the matching RTP Map. User should also add reference clock attributes
    //   which are relevant to current session, this particular payload type,
    //   or any specific SSRC.
    //
    // See `Clock::sync_and_add()` documentation above for more details.
    let mut clock_map = gst::Structure::new_empty("application/x-rtp2-clock-map");
    let mut pt_map = gst::Structure::new_empty("application/x-rtp2-pt-map");
    let mut has_rtcp_sink = false;

    // TODO handle multiple refclks at each level?
    // see https://www.rfc-editor.org/rfc/rfc7273.html#section-4.2

    let session_level_clock =
        if let Some(refclk_res) = sdp.get_first_attribute_typed::<ReferenceClock>() {
            let mut clock = Clock::new(refclk_res.context("session-level")?);
            if let Some(mediaclk_res) = sdp.get_first_attribute_typed::<MediaClockSource>() {
                clock.set_mediaclk(mediaclk_res.context("session-level")?);
            }
            Some(clock)
        } else {
            None
        };

    for media in &sdp.medias {
        let Some(rtpmap_res) = media.attributes_typed::<RtpMap>().next() else {
            eprintln!("Skipping media due to missing rtpmap: {media:?}");
            continue;
        };
        let rtpmap = rtpmap_res.context("media-level")?;
        let media_level_ctx = || format!("media-level {} {rtpmap}", media.media);

        let mut pt_caps_builder = gst::Caps::builder("application/x-rtp")
            .field("media", &media.media)
            .field("payload", rtpmap.payload_type as i32)
            .field("clock-rate", rtpmap.clock_rate as i32)
            .field("encoding-name", &rtpmap.encoding_name)
            .field_if_some("encoding-params", rtpmap.encoding_params.as_ref());

        let inbound_src = get_or_add_inbound_rtp_src(
            pipeline,
            inbound_funnel,
            session_id,
            connection_address,
            media.port,
        )?;

        // handle rtcp attribute, if any
        if let Some(rtcp_attr_res) = media.get_first_attribute_typed::<Rtcp>() {
            let rtcp_attr = rtcp_attr_res.with_context(media_level_ctx)?;
            maybe_add_outbound_rtcp_sink(
                pipeline,
                outbound_rtcp_tee,
                &inbound_src,
                session_id,
                &mut has_rtcp_sink,
                &rtcp_attr.connection_address,
                rtcp_attr.port,
            )?;
        }

        // handle media-level clock attributes, if any
        let media_level_clk =
            if let Some(refclk_res) = media.get_first_attribute_typed::<ReferenceClock>() {
                let mut clock = Clock::new(refclk_res.with_context(media_level_ctx)?);
                if let Some(mediaclk_res) = media.get_first_attribute_typed::<MediaClockSource>() {
                    clock.set_mediaclk(mediaclk_res.with_context(media_level_ctx)?);
                }
                Some(clock)
            } else {
                None
            };
        // a media-level clock takes precedence over a session-level clock
        if let Some(clock) = media_level_clk.or(session_level_clock.clone()) {
            pt_caps_builder = clock.sync_and_add(args, &mut clock_map, pt_caps_builder)?;
        }

        // a source-specific clock takes precedence over a media-level clock
        // and a source-specific clock will be added to the pt-map using `ssrc-{ssrc}-ts-refclk`,
        // so sources can select their clock accordingly

        // let's look for matching ts-refclk & mediaclk for a given ssrc
        // TODO we currently expect ts-refclk before mediaclk, this could be improved if needed
        let mut ssrc_clocks = BTreeMap::new();
        for ssrc_attr in media.attributes_typed::<Ssrc>().flatten() {
            let ssrc_level_ctx = || format!("ssrc-level {}", ssrc_attr.ssrc_id);

            match &ssrc_attr.attribute {
                SsrcAttribute::ReferenceClock => {
                    let refclk = ssrc_attr
                        .get_typed::<ReferenceClock>()
                        .with_context(ssrc_level_ctx)?;
                    ssrc_clocks.insert(
                        ssrc_attr.ssrc_id,
                        Clock::with_ssrc(refclk, ssrc_attr.ssrc_id),
                    );
                }
                SsrcAttribute::MediaClockSource => {
                    let mediaclk = ssrc_attr
                        .get_typed::<MediaClockSource>()
                        .with_context(ssrc_level_ctx)?;
                    if let Some(ssrc_clock) = ssrc_clocks.get_mut(&ssrc_attr.ssrc_id) {
                        ssrc_clock.set_mediaclk(mediaclk);
                    } else {
                        eprintln!(
                            "missing matching ts-refclk for ssrc {} mediaclk => ignoring: media {rtpmap}",
                            ssrc_attr.ssrc_id
                        );
                    }
                }
                SsrcAttribute::Rtcp => {
                    let rtcp_attr = ssrc_attr.get_typed::<Rtcp>().with_context(ssrc_level_ctx)?;
                    maybe_add_outbound_rtcp_sink(
                        pipeline,
                        outbound_rtcp_tee,
                        &inbound_src,
                        session_id,
                        &mut has_rtcp_sink,
                        &rtcp_attr.connection_address,
                        rtcp_attr.port,
                    )?;
                }
                _ => (),
            }
        }

        for ssrc_clock in ssrc_clocks.into_values() {
            pt_caps_builder = ssrc_clock.sync_and_add(args, &mut clock_map, pt_caps_builder)?;
        }

        pt_map.set(rtpmap.payload_type.to_string(), pt_caps_builder.build());
    }

    if !has_rtcp_sink {
        eprintln!("No outbound RTCP sinks for this session");
        // redirect to a fakesink to avoid warnings
        let rtcp_fakesink = gst::ElementFactory::make("fakesink")
            .name("rtcp-fakesink")
            .property("async", false)
            .property("enable-last-sample", false)
            .build()
            .unwrap();
        pipeline.add(&rtcp_fakesink).unwrap();
        outbound_rtcp_tee
            .link_pads(Some("src_%u"), &rtcp_fakesink, Some("sink"))
            .unwrap();
    }

    let session = rtprecv.emit_by_name::<gst::glib::Object>("get-session", &[&session_id]);

    session.set_property("clock-map", clock_map);
    session.set_property("pt-map", pt_map);

    eprintln!(
        "clock-map {:#?}",
        session.property::<gst::Structure>("clock-map")
    );
    eprintln!(
        "\npt-map {:#?}",
        session.property::<gst::Structure>("pt-map")
    );

    Ok(())
}

/// Gets or adds a UDP src listening to inbound RTP packets on (connection_address, port)
/// and link it to the inbound_funnel
///
/// * rtprecv will demultiplex packets coming from different sources
/// * the socket will be shared with the RTCP sink (see `add_outbound_rtcp_sink()`)
fn get_or_add_inbound_rtp_src(
    pipeline: &gst::Pipeline,
    inbound_funnel: &gst::Element,
    session_id: u32,
    connection_address: &str,
    port: u16,
) -> anyhow::Result<gst::Element> {
    let inbound_src_name = format!("inbound-udpsrc-{session_id}-{}", port);

    if let Some(inbound_src) = pipeline.by_name(&inbound_src_name) {
        return Ok(inbound_src);
    }

    eprintln!("Adding inbound RTP src {inbound_src_name}");
    let inbound_src = gst::ElementFactory::make("udpsrc2")
        .name(inbound_src_name)
        .property("address", connection_address)
        .property("port", port as u32)
        .build()
        .context("configuring inbound src")?;

    // Up to GStreamer 1.28 included, udpsrc/2 used the default basesrc behaviour
    // that triggers an Allocation query when a Reconfigure event is sent by a branch.
    // This is solved for newer udpsrc/2 versions, but the workaround
    // is kept here as an illustration & to ensure proper behaviour for all.
    inbound_src.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::QUERY_DOWNSTREAM | gst::PadProbeType::PUSH,
        |_pad, info| {
            let Some(gst::PadProbeData::Query(query)) = info.data.as_ref() else {
                unreachable!();
            };
            if query.type_() == gst::QueryType::Allocation {
                return gst::PadProbeReturn::Drop;
            }
            gst::PadProbeReturn::Ok
        },
    );

    pipeline.add(&inbound_src).unwrap();
    inbound_src
        .link_pads(Some("src"), inbound_funnel, Some("sink_%u"))
        .unwrap();

    Ok(inbound_src)
}

/// Adds a UDP sink sending RTCP packets to (connection_address, port)
/// and link it to the is_first_outbound_rtcp_sink, if it wasn't added yet
///
/// If it's the first RTCP sink, also share the socket with the inbound_src.
fn maybe_add_outbound_rtcp_sink(
    pipeline: &gst::Pipeline,
    outbound_rtcp_tee: &gst::Element,
    inbound_src: &gst::Element,
    session_id: u32,
    has_rtcp_sink: &mut bool,
    connection_address: &str,
    port: u16,
) -> anyhow::Result<()> {
    let outbound_rtcp_udpsink_name = format!(
        "outbound-rtcp-funnel-{session_id}-{}-{}",
        connection_address, port,
    );

    if pipeline.by_name(&outbound_rtcp_udpsink_name).is_some() {
        // already added
        return Ok(());
    }

    eprintln!("Adding outbound RTCP sink {outbound_rtcp_udpsink_name}");
    let rtcp_sink = gst::ElementFactory::make("udpsink")
        .name(&outbound_rtcp_udpsink_name)
        .property("sync", false)
        .property("async", false)
        .property("port", port as i32)
        .property("host", connection_address)
        .build()
        .context("configuring outbound RTCP sink")?;
    pipeline.add(&rtcp_sink).unwrap();
    outbound_rtcp_tee
        .link_pads(Some("src_%u"), &rtcp_sink, Some("sink"))
        .unwrap();

    if !*has_rtcp_sink {
        // Share the same socket in source and sink (only doing that for the first sink)
        inbound_src.set_state(gst::State::Ready).unwrap();
        let socket = inbound_src.property::<glib::Object>("used-socket");
        rtcp_sink.set_property("socket", socket);
        *has_rtcp_sink = true;
    }

    Ok(())
}

fn on_rtp_stream_added(recv: &gst::Element, pad: &gst::Pad, disable_visualizer: bool) {
    let pad_name = pad.name();
    if !pad_name.starts_with("rtp_src_") {
        return;
    }
    let Some(parent) = recv.parent() else { return };
    let pipeline = parent.downcast_ref::<gst::Pipeline>().unwrap();

    let Some(caps) = pad.current_caps() else {
        eprintln!("Ignoring stream without caps: {pad_name}");
        return;
    };
    let s = caps.structure(0).unwrap();
    let Ok(encoding_name) = s.get::<String>("encoding-name") else {
        eprintln!("Ignoring stream without encoding-name: {caps}, pad {pad_name}");
        return;
    };
    let depayloader = match encoding_name.as_str() {
        "L8" => "rtpL8depay2",
        "L16" => "rtpL16depay2",
        "L24" => "rtpL24depay2",
        other => {
            eprintln!("Ignoring stream with unsupported encoding-name: {other}, pad {pad_name}");
            return;
        }
    };

    let (_prefix, _direction, session_id, pt, ssrc) = pad_name.split('_').collect_tuple().unwrap();
    let pt = pt.parse::<u8>().unwrap();

    let refclk = if let Ok(Some(media_level_refclk)) =
        s.get_optional::<&str>(&format!("a-ssrc-{ssrc}-ts-refclk"))
    {
        media_level_refclk
    } else if let Ok(Some(media_level_refclk)) = s.get_optional::<&str>("a-ts-refclk") {
        media_level_refclk
    } else {
        "local"
    };

    eprintln!("Adding {pad_name} {refclk} {caps}");
    let id = format!("{session_id}-{pt}-{ssrc}");
    let inter_stream_buffering = 500.mseconds().nseconds();
    let stream_renderer_bin = gst::parse::bin_from_description_with_name(
        &format!(
            "
            queue name=audio-rtp-queue-{id}
                max-size-buffers=1 max-size-bytes=0 max-size-time=0
            ! {depayloader} name=audio-depay-{id}
            ! tee name=audio-tee-{id}
              ! queue name=audio-renderer-queue-{id}
                    max-size-buffers=0 max-size-bytes=0 max-size-time={inter_stream_buffering}
              ! audioconvert name=audio-renderer-convert-{id}
              ! queue name=audio-renderer-sink-queue-{id}
                  max-size-buffers=1 max-size-bytes=0 max-size-time=0
              ! autoaudiosink name=audio-renderer-sink-{id}
            {optional_vis_branch}
            ",
            optional_vis_branch = if disable_visualizer {
                "".to_string()
            } else {
                format!(
                    "
            audio-tee-{id}.
              ! queue name=audio-visu-queue-{id}
              ! audioconvert name=audio-visu-convert-{id}
              ! spectrascope name=audio-visu-{id}
              ! timeoverlay name=audio-visu-overlay-{id} time-mode=reference-timestamp
                  reference-timestamp-caps={ref_ts_caps}
                  text={id} halignment=right
              ! videoconvert name=audio-visu-videoconvert-{id}
              ! queue name=audio-visu-sink-queue-{id}
                  max-size-buffers=1 max-size-bytes=0 max-size-time=0
              ! autovideosink name=audio-visu-sink-{id}
            ",
                    ref_ts_caps = match refclk.parse::<ReferenceClock>() {
                        Ok(ReferenceClock::Local) => "timestamp/x-sender",
                        Ok(ReferenceClock::Ptp(_)) => "timestamp/x-ptp",
                        _ => "timestamp/x-ntp",
                    }
                )
            }
        ),
        true,
        &format!("audio-stream-renderer-{id}"),
    )
    .unwrap();
    pipeline.add(&stream_renderer_bin).unwrap();
    pad.link(&stream_renderer_bin.static_pad("sink").unwrap())
        .unwrap();
    // log meta time references every second
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        {
            let last_log_time = Arc::new(AtomicU64::new(0));
            move |pad, info| {
                let element = pad.parent_element().expect("available in this state");
                let now = *element.clock().expect("available in this state").time();
                if now < last_log_time.load(Ordering::SeqCst) + *gst::ClockTime::SECOND {
                    return gst::PadProbeReturn::Ok;
                }
                last_log_time.store(now, Ordering::SeqCst);

                let buffer = match info.data.as_ref() {
                    Some(gst::PadProbeData::Buffer(buffer)) => buffer,
                    Some(gst::PadProbeData::BufferList(list)) => {
                        if list.is_empty() {
                            return gst::PadProbeReturn::Ok;
                        }
                        list.get(0).expect("at least one buffer")
                    }
                    _ => unreachable!("BUFFER | BUFFER_LIST pad probe"),
                };

                for meta in buffer.iter_meta::<gst::ReferenceTimestampMeta>() {
                    eprintln!(
                        "{}: ReferenceTimestampMeta: {} ({}.{}) {}",
                        pad.name(),
                        meta.timestamp(),
                        meta.timestamp().seconds(),
                        meta.timestamp().nseconds() % *gst::ClockTime::SECOND,
                        meta.reference(),
                    );
                }

                gst::PadProbeReturn::Ok
            }
        },
    );

    stream_renderer_bin.sync_state_with_parent().unwrap();
}

fn build_pipeline(
    args: Args,
    loop_: &glib::MainLoop,
    sdp: sdp_types::Session,
) -> anyhow::Result<gst::Pipeline> {
    let Ok(session_id) = sdp.origin.sess_id.parse::<u32>() else {
        bail!(
            "Expected unsigned integer for session id, got {}",
            sdp.origin.sess_id
        );
    };

    let pipeline = gst::Pipeline::new();

    // Prepare RTP session handling
    let inbound_funnel = gst::ElementFactory::make("funnel")
        .name("inbound-funnel")
        .build()
        .unwrap();
    // Decouple downstream from udp source & add a bit of buffering
    let inbound_queue = gst::ElementFactory::make("queue")
        .name("inbound-queue")
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 50.mseconds())
        .property("max-size-buffers", 0u32)
        .build()
        .unwrap();
    let elems = [&inbound_funnel, &inbound_queue];
    pipeline.add_many(elems).unwrap();
    gst::Element::link_many(elems).unwrap();

    let rtprecv = gst::ElementFactory::make("rtprecv")
        .name("rtprecv")
        .property("rtp-id", RTP_ID)
        .property("latency", args.rtp_latency)
        .property("add-reference-timestamp-meta", true)
        .build()
        .unwrap();
    pipeline.add(&rtprecv).unwrap();
    inbound_queue
        .link_pads(
            Some("src"),
            &rtprecv,
            Some(&format!("rtp_sink_{session_id}")),
        )
        .unwrap();

    let rtpsend = gst::ElementFactory::make("rtpsend")
        .name("rtpsend")
        .property("rtp-id", RTP_ID)
        .build()
        .unwrap();
    let outbound_rtcp_tee = gst::ElementFactory::make("tee")
        .name("outbound-rtcp-tee")
        .build()
        .unwrap();
    let elems = [&rtpsend, &outbound_rtcp_tee];
    pipeline.add_many(elems).unwrap();
    rtpsend
        .link_pads(
            Some(&format!("rtcp_src_{session_id}")),
            &outbound_rtcp_tee,
            Some("sink"),
        )
        .unwrap();

    prepare_expected_medias(
        &args,
        &pipeline,
        &inbound_funnel,
        &rtprecv,
        &outbound_rtcp_tee,
        &sdp,
        session_id,
    )
    .context("preparing expected medias from SDP")?;

    rtprecv.connect_pad_added(move |recv, pad| {
        on_rtp_stream_added(recv, pad, args.disable_visualizer)
    });

    let mut bus_recv_stream = pipeline.bus().unwrap().stream();
    loop_.context().spawn_local({
        let pipeline = pipeline.clone();
        let loop_ = loop_.clone();
        async move {
            while let Some(msg) = bus_recv_stream.next().await {
                use gst::MessageView::*;
                match msg.view() {
                    Latency(_) => {
                        pipeline.call_async(|pipeline| {
                            let _ = pipeline.recalculate_latency();
                        });
                    }
                    Error(err) => {
                        eprintln!("\nShutting down due to: {err:?}");
                        loop_.quit();
                        break;
                    }
                    _ => (),
                }
            }
        }
    });

    Ok(pipeline)
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    gst::init().unwrap();
    gstrsrtp::plugin_register_static().unwrap();
    gstrsudp::plugin_register_static().unwrap();

    eprintln!("Waiting for SDP message on stdin...");

    let mut input = String::new();
    while io::stdin().read_line(&mut input).context("reading stdin")? != 0 {}
    if input.is_empty() {
        bail!("No SDP message on stdin");
    }

    eprintln!("Parsing SDP message:\n{input}");
    let sdp = sdp_types::Session::parse(input.as_bytes()).context("parsing input SDP")?;

    let main_context = glib::MainContext::default();
    let _guard = main_context.acquire().unwrap();
    let loop_ = glib::MainLoop::new(None, false);

    let pipeline = build_pipeline(args, &loop_, sdp).context("building pipeline")?;

    // Start streaming
    pipeline
        .set_state(gst::State::Playing)
        .context("setting pipeline to Playing")?;
    eprintln!("Playing");

    ctrlc::set_handler({
        let loop_ = loop_.clone();
        move || {
            eprintln!("\nShutting down due to user request");
            loop_.quit();
        }
    })
    .unwrap();

    loop_.run();
    // Interrupted by user or an error occured

    let _ = pipeline.set_state(gst::State::Null);
    drop(pipeline);

    // This is needed by some tracers to write their log file
    unsafe {
        gst::deinit();
    }

    Ok(())
}
