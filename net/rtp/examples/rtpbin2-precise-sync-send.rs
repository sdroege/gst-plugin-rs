/// rtpbin2-precise-sync-send
///
/// This example sends a raw audio stream via RTP with clock signalling ([RFC7273]).
/// The SDP is produced on `stdout`.
///
/// Use `rtpbin2-precise-sync-recv` as a receiver.
///
/// Note that this sender muxes RTP & RTCP. If this is not a requirement,
/// this can be simplified by removing the relevant funnel & signalling and
/// adding a dedicated `udpsink`.
///
/// ## Examples
///
/// ### Usage
///
/// ````
/// cargo r --example rtpbin2-precise-sync-send -- --help
/// ````
///
/// ### Produce an audio stream with an NTP ref clock
///
/// Note: since the actual NTP server / port arguments are not specified,
/// defaults apply: pool.ntp.org:123
///
/// ````
/// cargo r --example rtpbin2-precise-sync-send -- ntp > audio_stream_ntp.sdp
/// ````
///
/// ### Combine previous stream with a stream with a local ref clock
///
/// ````
/// cargo r --example rtpbin2-precise-sync-send -- --freq 550 --expect-sdp local \
///         < audio_stream_ntp.sdp > combined_audio_streams.sdp
/// ````
///
/// ### Declaring the local (system) clock as being synced to a ref. clock
///
/// If the system clock is known to be sync to a reference clock, it is more accurate
/// to sync to this clock instead of using a GStreamer Ntp / Ptp clock.
///
/// ````
/// cargo r --example rtpbin2-precise-sync-send -- \
///     local --refclk ntp=pool.ntp.org \
///         > audio_stream_ntp.sdp
/// ````
///
/// or
///
/// ````
/// cargo r --example rtpbin2-precise-sync-send -- \
///     local --refclk ptp=IEEE1588-2008:00-00-00-00-00-00-00-2A:1 \
///         > audio_stream_ntp.sdp
/// ````
///
/// [RFC7273]: https://www.rfc-editor.org/rfc/rfc7273.html
// SPDX-License-Identifier: MPL-2.0
use anyhow::{Context, bail};
use clap::Parser;
use futures::prelude::*;
use gst::glib;
use gst::prelude::*;

use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::mpsc;

pub use sdp_types::{MediaClockSource, ReferenceClock};

const RTP_ID: &str = "rtp-id";

const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u32 = 2;

const DEFAULT_NTP_PORT: u16 = 123;

#[derive(clap::Parser, Debug)]
#[command(
    version,
    about = "Sends a raw audio stream via RTP with clock signalling (RFC7273). Use `rtpbin2-precise-sync-recv` as a receiver"
)]
pub struct Args {
    #[command(subcommand)]
    pub clock: Clock,

    #[clap(long, help = "Session id", default_value_t = 1234)]
    pub session_id: u32,

    #[clap(long, help = "Payload Type", default_value_t = 96)]
    pub pt: u8,

    #[clap(long, help = "Encoding Name (L8, L16, L24)", default_value = "L24")]
    pub encoding_name: String,

    #[clap(
        long,
        help = "Audio stream main tick frequency",
        default_value_t = 440.0
    )]
    pub freq: f64,

    #[clap(long, help = "Audio buffer duration (ms)", default_value_t = 10)]
    pub audio_buffer_dur: u32,

    #[clap(
        long,
        help = "Address of the sender to be used in SDP",
        default_value = "127.0.0.1"
    )]
    pub sender_addr: String,

    #[clap(long, help = "Sender inbound RTCP port", default_value_t = 5_001)]
    pub inbound_rtcp_port: u16,

    #[clap(
        long,
        help = "Address to which the muxed RTP/RTCP stream is sent",
        default_value = "127.0.0.1"
    )]
    pub outbound_addr: String,

    #[clap(
        long,
        help = "Port to which the muxed RTP/RTCP stream is sent",
        default_value_t = 5_000
    )]
    pub outbound_port: u16,

    #[clap(long, help = "Disable RTCP")]
    pub disable_rtcp: bool,

    #[clap(
        long,
        help = "Align the RTP timestamps so the mediaclk direct offset is 0"
    )]
    pub align_for_offset_0: bool,

    #[clap(long, help = "Expect SDP on stdin (and append new media)")]
    pub expect_sdp: bool,
}

#[derive(Debug, Clone)]
pub struct TimeReference {
    /// Clock selected by the command line argument
    clock: gst::Clock,
    /// Time offset between this clock and the target reference clock
    /// (zero execept for local (system) clock representing a different clock)
    offset: gst::ClockTime,
}

pub trait Rfc7273Clock {
    fn get_time_reference(&self) -> anyhow::Result<TimeReference>;

    fn get_clock_signalling(
        &self,
        // RFC7273 § 5.2:
        // > The offset indicates the RTP timestamp value at the epoch (time of
        // > origin) of the reference clock
        rtptimestamp_offset: u32,
    ) -> anyhow::Result<(sdp_types::ReferenceClock, sdp_types::MediaClockSource)>;
}

#[derive(clap::Subcommand, Clone, Debug)]
pub enum Clock {
    #[clap(about = "Use an NTP clock")]
    Ntp(NtpClock),
    #[clap(about = "Use a PTP clock")]
    Ptp(PtpClock),
    #[clap(about = "Use the local (system) clock")]
    Local(LocalClock),
}

impl Rfc7273Clock for Clock {
    fn get_time_reference(&self) -> anyhow::Result<TimeReference> {
        match self {
            Self::Local(system) => system.get_time_reference(),
            Self::Ntp(ntp) => ntp.get_time_reference(),
            Self::Ptp(ptp) => ptp.get_time_reference(),
        }
    }

    fn get_clock_signalling(
        &self,
        rtptimestamp_offset: u32,
    ) -> anyhow::Result<(ReferenceClock, MediaClockSource)> {
        match self {
            Self::Local(system) => system.get_clock_signalling(rtptimestamp_offset),
            Self::Ntp(ntp) => ntp.get_clock_signalling(rtptimestamp_offset),
            Self::Ptp(ptp) => ptp.get_clock_signalling(rtptimestamp_offset),
        }
    }
}

#[derive(clap::Parser, Clone, Debug, Default)]
#[clap(about = "NTP Clock")]
pub struct NtpClock {
    #[clap(long, help = "NTP server host", default_value = "pool.ntp.org")]
    pub ntp_server: String,

    #[clap(long, help = "NTP server port")]
    pub ntp_port: Option<u16>,
}

impl Rfc7273Clock for NtpClock {
    fn get_time_reference(&self) -> anyhow::Result<TimeReference> {
        let clock = gst_net::NtpClock::new(
            None,
            &self.ntp_server,
            self.ntp_port.unwrap_or(DEFAULT_NTP_PORT) as _,
            gst::ClockTime::ZERO,
        );

        clock
            .wait_for_sync(5.seconds())
            .with_context(|| format!("Syncing to {self:?}"))?;

        let now = clock.time();
        eprintln!(
            "Synced to {self:?}: now {now} ({}.{})",
            now.seconds(),
            now.nseconds() % *gst::ClockTime::SECOND
        );

        Ok(TimeReference {
            clock: clock.upcast(),
            offset: gst::ClockTime::ZERO,
        })
    }

    fn get_clock_signalling(
        &self,
        rtptimestamp_offset: u32,
    ) -> anyhow::Result<(ReferenceClock, MediaClockSource)> {
        Ok((
            sdp_types::NtpServerAddr::builder(&self.ntp_server)
                .port_if_some(self.ntp_port)
                .build()
                .into(),
            sdp_types::Direct::with_offset(rtptimestamp_offset).into(),
        ))
    }
}

#[derive(clap::Args, Clone, Debug, Default)]
#[clap(about = "PTP Clock")]
pub struct PtpClock {
    #[clap(long, help = "PTP domain", default_value = "0")]
    pub ptp_domain: u8,
}

impl Rfc7273Clock for PtpClock {
    fn get_time_reference(&self) -> anyhow::Result<TimeReference> {
        gst_net::PtpClock::init(None, &[])?;
        let clock = gst_net::PtpClock::new(None, self.ptp_domain as _)?;

        clock
            .wait_for_sync(5.seconds())
            .with_context(|| format!("Syncing to {self:?}"))?;

        Ok(TimeReference {
            clock: clock.upcast(),
            offset: gst::ClockTime::ZERO,
        })
    }

    fn get_clock_signalling(
        &self,
        rtp_timestamp_offset: u32,
    ) -> anyhow::Result<(ReferenceClock, MediaClockSource)> {
        let gmid = self
            .get_time_reference()?
            .clock
            .property::<u64>("grandmaster-clock-id");

        Ok((
            sdp_types::Ptp::try_from_gmid_with_domain_number(gmid, self.ptp_domain)
                .context("ptp domain")?
                .into(),
            sdp_types::Direct::with_offset(rtp_timestamp_offset).into(),
        ))
    }
}

#[derive(clap::Args, Clone, Debug, Default)]
#[clap(about = "Local (system) Clock")]
pub struct LocalClock {
    #[clap(
        long,
        help = "Which clock the local (system) clock is synchronized to. E.g. 'ntp=pool.ntp.org'"
    )]
    pub refclk: Option<String>,
}

impl Rfc7273Clock for LocalClock {
    fn get_time_reference(&self) -> anyhow::Result<TimeReference> {
        let time_offset = if let Some(ref refclk) = self.refclk {
            match sdp_types::ReferenceClock::from_str(refclk)
                .context("local clock synchronized refclk")?
            {
                sdp_types::ReferenceClock::Local => gst::ClockTime::ZERO,
                sdp_types::ReferenceClock::Ntp(_) => gst::clock_time::UNIX_TO_NTP_TIME_OFFSET,
                sdp_types::ReferenceClock::Ptp(_) => *gst::clock_time::UNIX_TO_PTP_TIME_OFFSET,
                other => {
                    bail!("local clock declared as synchronized to unsupported refclk {other:?}")
                }
            }
        } else {
            gst::ClockTime::ZERO
        };

        let clock = glib::Object::builder::<gst::SystemClock>()
            // produce time relative to UNIX epoch
            .property("clock-type", gst::ClockType::Realtime)
            .build();

        Ok(TimeReference {
            clock: clock.upcast(),
            offset: time_offset,
        })
    }

    fn get_clock_signalling(
        &self,
        rtp_timestamp_offset: u32,
    ) -> anyhow::Result<(ReferenceClock, MediaClockSource)> {
        Ok((
            if let Some(ref refclk) = self.refclk {
                sdp_types::ReferenceClock::from_str(refclk).context("local clock refclk")?
            } else {
                sdp_types::ReferenceClock::Local
            },
            sdp_types::Direct::with_offset(rtp_timestamp_offset).into(),
        ))
    }
}

fn print_sdp(
    args: &Args,
    ref_clock_base_time: gst::ClockTime,
    mut sdp: sdp_types::Session,
    sdes: gst::Structure,
    payloader: &gst::Element,
    caps: gst::Caps,
) -> anyhow::Result<()> {
    use sdp_types::{Rtcp, RtpMap, Ssrc, SsrcAttribute, TransportProto};
    use std::{net::Ipv4Addr, str::FromStr};

    eprintln!("Got {caps:#?} for {}", payloader.name());

    let rtp_caps_s = caps.structure(0).unwrap();
    let pt = rtp_caps_s.get::<i32>("payload").unwrap();
    let clock_rate = rtp_caps_s.get::<i32>("clock-rate").unwrap() as u32;
    let ssrc = rtp_caps_s.get::<u32>("ssrc").unwrap();
    let cname = sdes.get::<&str>("cname").unwrap();
    eprintln!("ssrc {ssrc:#08x} ({ssrc}), cname {cname}");

    let rtpmap = RtpMap::builder(pt as _, &args.encoding_name, clock_rate)
        .encoding_params_if_some(
            rtp_caps_s
                .get_optional::<glib::GString>("encoding-params")
                .unwrap(),
        )
        .build();
    let rtcp_attr = Rtcp::with_ip_addr(
        args.inbound_rtcp_port,
        Ipv4Addr::from_str(&args.sender_addr).context("sender_addr")?,
    );
    let ssrc_cname = Ssrc::with_value(ssrc, SsrcAttribute::Cname, cname);

    let (refclk, mediaclk) = args
        .clock
        .get_clock_signalling(get_rtptime_at_reference_clock_epoch(
            payloader,
            ref_clock_base_time,
            clock_rate,
        ))
        .context("getting clock ref and source")?;

    let mut matching_media_rtpmap = None;
    for media in sdp.medias.iter_mut() {
        for media_rtpmap in media.attributes_typed::<RtpMap>().flatten() {
            if media_rtpmap.payload_type == args.pt {
                if media_rtpmap != rtpmap {
                    bail!(
                        "Found incompatible existing media rtpmap {media_rtpmap:?} for payload type {} => select a different pt for {rtpmap}",
                        args.pt,
                    );
                }

                matching_media_rtpmap = Some(media_rtpmap);
                break;
            }
        }

        if let Some(ref media_rtpmap) = matching_media_rtpmap {
            let media_level_ctx =
                |media: &sdp_types::Media| format!("media-level {} {media_rtpmap}", media.media);

            if !args.disable_rtcp
                && let Some(media_rtcp_attr_res) = media.get_first_attribute_typed::<Rtcp>()
            {
                let media_rtcp_attr =
                    media_rtcp_attr_res.with_context(|| media_level_ctx(media))?;
                if media_rtcp_attr == rtcp_attr {
                    bail!(
                        "Existing SDP media RTCP attribute '{media_rtcp_attr}' matches requested '{rtcp_attr}'\n\
                            => select a different sender-address / inbound-rtcp-port or use disbale-rtcp",
                    );
                }
                media.add_attribute(Ssrc::with_typed_attribute(ssrc, rtcp_attr.clone()));
            }
            media.add_attribute(ssrc_cname.clone());

            let mut must_add_ssrc_clock = true;
            if let Some(entry_refclk_res) = media.get_first_attribute_typed::<ReferenceClock>()
                && refclk == entry_refclk_res.with_context(|| media_level_ctx(media))?
                && let Some(entry_mediaclk_res) =
                    media.get_first_attribute_typed::<MediaClockSource>()
            {
                must_add_ssrc_clock =
                    mediaclk != entry_mediaclk_res.with_context(|| media_level_ctx(media))?;
            }

            if must_add_ssrc_clock {
                media.add_attribute(Ssrc::with_typed_attribute(ssrc, refclk.clone()));
                media.add_attribute(Ssrc::with_typed_attribute(ssrc, mediaclk.clone()));
            }

            break;
        }
    }

    if matching_media_rtpmap.is_none() {
        use sdp_types::{Media, MediaType};
        sdp.add_media(
            Media::builder(
                MediaType::Audio,
                args.outbound_port,
                TransportProto::RtpAvp,
                pt,
            )
            .attribute(rtpmap)
            .attribute_from_str_if("rtcp-mux", !args.disable_rtcp)
            .attribute_if(rtcp_attr, !args.disable_rtcp)
            .attribute(refclk)
            .attribute(mediaclk)
            .attribute(ssrc_cname)
            .build(),
        );
    }

    let mut ser_sdp = vec![];
    sdp.write(&mut ser_sdp).context("writing SDP")?;
    println!("{}", String::from_utf8_lossy(&ser_sdp));

    Ok(())
}

fn get_rtptime_at_reference_clock_epoch(
    payloader: &gst::Element,
    ref_clock_base_time: gst::ClockTime,
    clock_rate: u32,
) -> u32 {
    let stats = payloader.property::<gst::Structure>("stats");
    let payloader_offset = stats
        .get::<u32>("timestamp-offset")
        .expect("valid payloader stats");
    eprintln!("payloader offset {payloader_offset}");

    let base_time_rtptime = base_time_to_rtptime(ref_clock_base_time, clock_rate);
    let rtptime_at_epoch = payloader_offset.wrapping_sub(base_time_rtptime);
    eprintln!("RTP time at reference clock epoch {rtptime_at_epoch}");

    rtptime_at_epoch
}

fn base_time_to_rtptime(base_time: gst::ClockTime, clock_rate: u32) -> u32 {
    let base_time_rtptime_ext = base_time
        .nseconds()
        .mul_div_floor(clock_rate as _, *gst::ClockTime::SECOND)
        .unwrap();

    let base_time_rtptime = (base_time_rtptime_ext & (u32::MAX as u64)) as u32;

    eprintln!(
        "base time {base_time} ({}.{}), RTP time {base_time_rtptime} (ext {base_time_rtptime_ext})",
        base_time.seconds(),
        base_time.nseconds() % *gst::ClockTime::SECOND,
    );

    base_time_rtptime
}

fn build_pipeline(
    args: &Args,
    loop_: &glib::MainLoop,
    time_reference_offset: gst::ClockTime,
    caps_sender: mpsc::Sender<(gst::Element, gst::Caps)>,
) -> anyhow::Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::new();

    // Build a ticking audio source with different ticks every 4 ticks
    eprintln!("Ticking {}Hz audio source", args.freq);
    let beat_period = 500.mseconds();
    let bar_period = 4 * beat_period;
    let samples_per_buffer = args
        .audio_buffer_dur
        .mul_div_round(AUDIO_RATE, 1_000)
        .context("audio-buffer-dur out of range")?;
    let raw_caps = gst::Caps::builder("audio/x-raw")
        .field("rate", AUDIO_RATE as i32)
        .field("channels", AUDIO_CHANNELS as i32)
        .build();
    let audio_src_bin = gst::parse::bin_from_description_with_name(
        &format!(
            "
            audiotestsrc name=audio-bar freq={bar_freq} wave=ticks is-live=true volume=0.2
                tick-interval={bar_interval} samplesperbuffer={samples_per_buffer}
                ! {raw_caps} ! audio-mixer.

            audiotestsrc name=audio-beat freq={beat_freq} wave=ticks is-live=true volume=0.2
                tick-interval={beat_interval} samplesperbuffer={samples_per_buffer}
                ! {raw_caps} ! audio-mixer.
                
            audiomixer name=audio-mixer output-buffer-duration={buf_duration_ns}
        ",
            bar_interval = bar_period.nseconds(),
            beat_interval = beat_period.nseconds(),
            bar_freq = args.freq * 4.0,
            beat_freq = args.freq,
            buf_duration_ns = args.audio_buffer_dur * 1_000_000,
        ),
        true,
        "audio-src-bin",
    )
    .context("building audio-src-bin")?;
    pipeline.add(&audio_src_bin).unwrap();

    // Align bar ticks with pipeline clock time multiples so as to ease sync check between various sources
    audio_src_bin.static_pad("src").unwrap().add_probe(
        gst::PadProbeType::EVENT_DOWNSTREAM,
        move |pad, info| {
            let Some(gst::PadProbeData::Event(ref evt)) = info.data else {
                unreachable!();
            };
            if evt.type_() != gst::EventType::StreamStart {
                return gst::PadProbeReturn::Ok;
            }

            let base_time = pad
                .parent()
                .unwrap()
                .downcast::<gst::Element>()
                .unwrap()
                .base_time()
                .expect("set at this stage");
            let ref_clock_base_time = base_time.wrapping_add(time_reference_offset);
            let offset = bar_period - ref_clock_base_time % bar_period;
            eprintln!(
                "Aligning bar periods with clock time multiples: offset {offset}, \
                 reference clock base time {ref_clock_base_time} ({}.{})",
                ref_clock_base_time.seconds(),
                ref_clock_base_time.nseconds() % *gst::ClockTime::SECOND,
            );
            pad.set_offset(offset.nseconds() as i64);

            gst::PadProbeReturn::Remove
        },
    );

    let audio_payloader_name = match args.encoding_name.as_str() {
        "L8" => "rtpL8pay2",
        "L16" => "rtpL16pay2",
        "L24" => "rtpL24pay2",
        other => bail!("unsupported audio encoding name: {other}"),
    };
    let pt = args.pt;
    let audio_payloader_bin = gst::parse::bin_from_description_with_name(
        &format!(
            "
            audioconvert name=audio-convert
            ! clocksync name=audio-sync
            ! {audio_payloader_name} name=audio-payloader pt={pt}
            ! clocksync name=audio-rtp-packet-sync
        "
        ),
        true,
        "audio-payloader-bin",
    )
    .context("building audio-payloader-bin")?;
    pipeline.add(&audio_payloader_bin).unwrap();
    audio_src_bin
        .link_pads(None, &audio_payloader_bin, None)
        .unwrap();

    // We need pt, ssrc & cname from the payloader caps for SDP generation
    let audio_payloader = audio_payloader_bin
        .by_name("audio-payloader")
        .expect("added above");
    let audio_payloyoader_src_pad = audio_payloader.static_pad("src").unwrap();
    audio_payloyoader_src_pad.connect_caps_notify(move |pad| {
        let Some(payloader_and_caps) = Option::zip(pad.parent_element(), pad.allowed_caps()) else {
            return;
        };

        let _ = caps_sender.send(payloader_and_caps);
    });

    // This is only for monitoring purposes
    audio_payloyoader_src_pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(gst::PadProbeData::Buffer(ref buf)) = info.data else {
            unreachable!();
        };
        let elem = pad.parent_element().unwrap();
        let clock = elem.clock().unwrap();
        let now = clock.time();

        let base_time = elem.base_time().expect("available at this stage");
        let evt = pad.sticky_event::<gst::event::Segment>(0).unwrap();
        let gst::EventView::Segment(evt) = evt.view() else {
            unreachable!();
        };
        let segment = evt
            .segment()
            .downcast_ref::<gst::format::Time>()
            .expect("audiotestsrc");
        let running_time = segment.to_running_time(buf.pts()).unwrap();
        let reference_time = (base_time + running_time).wrapping_add(time_reference_offset);

        let (seqnum, packet_rtptime) = {
            let buf_mapped = buf.map_readable().unwrap();
            let packet = rtp_types::RtpPacket::parse(buf_mapped.as_slice()).unwrap();
            (packet.sequence_number(), packet.timestamp())
        };

        eprintln!(
            "{now}: first RTP packet seqnum {seqnum}, RTP time {packet_rtptime}, \
             reference_time {reference_time} ({}.{}), running time {running_time} ({}.{}), \
             base_time {base_time} ({}.{})",
            reference_time.seconds(),
            reference_time.nseconds() % *gst::ClockTime::SECOND,
            running_time.seconds(),
            running_time.nseconds() % *gst::ClockTime::SECOND,
            base_time.seconds(),
            base_time.nseconds() % *gst::ClockTime::SECOND,
        );

        gst::PadProbeReturn::Remove
    });

    // Outbound RTP / RTCP (if enabled)

    let rtpsend = gst::ElementFactory::make("rtpsend")
        .name("rtpsend")
        .property("rtp-id", RTP_ID)
        .build()
        .unwrap();
    pipeline.add(&rtpsend).unwrap();
    audio_payloader_bin
        .link_pads(
            None,
            &rtpsend,
            Some(&format!("rtp_sink_{}", args.session_id)),
        )
        .unwrap();

    // mux RTP & RTCP (if enabled)
    let outbound_funnel = gst::ElementFactory::make("funnel")
        .name("outbound-funnel")
        .build()
        .unwrap();
    pipeline.add(&outbound_funnel).unwrap();
    rtpsend
        .link_pads(
            Some(&format!("rtp_src_{}", args.session_id)),
            &outbound_funnel,
            Some("sink_0"),
        )
        .unwrap();

    let outbound_sink = gst::ElementFactory::make("udpsink")
        .name("outbound-udpsink")
        // sync of the RTP packets is handled before rtpsend
        .property("sync", false)
        .property("host", &args.outbound_addr)
        .property("port", args.outbound_port as i32)
        .build()
        .context("configuring outbound sink")?;
    pipeline.add(&outbound_sink).unwrap();
    outbound_funnel.link(&outbound_sink).unwrap();

    if !args.disable_rtcp {
        rtpsend
            .link_pads(
                Some(&format!("rtcp_src_{}", args.session_id)),
                &outbound_funnel,
                Some("sink_1"),
            )
            .unwrap();

        // Inbound RTCP

        let rtcp_src = gst::ElementFactory::make("udpsrc2")
            .name("rtcp-udpsrc")
            .property("port", args.inbound_rtcp_port as u32)
            .property("caps", gst::Caps::new_empty_simple("application/x-rtcp"))
            .build()
            .context("configuring inbound RTCP src")?;
        pipeline.add(&rtcp_src).unwrap();
        let rtprecv = gst::ElementFactory::make("rtprecv")
            .property("rtp-id", RTP_ID)
            .build()
            .unwrap();
        pipeline.add(&rtprecv).unwrap();
        rtcp_src
            .link_pads(
                Some("src"),
                &rtprecv,
                Some(&format!("rtcp_sink_{}", args.session_id)),
            )
            .unwrap();

        // Share the same socket in source and sink
        outbound_sink.set_state(gst::State::Ready).unwrap();
        let socket = outbound_sink.property::<glib::Object>("used-socket");
        rtcp_src.set_property("socket", socket);
    }

    let mut bus_send_stream = pipeline.bus().unwrap().stream();
    loop_.context().spawn_local({
        let pipeline = pipeline.clone();
        let loop_ = loop_.clone();
        async move {
            while let Some(msg) = bus_send_stream.next().await {
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

    let mut input = String::new();
    if args.expect_sdp {
        eprintln!("Expecting SDP message on stdin...");
        while std::io::stdin().read_line(&mut input)? != 0 {}

        if input.is_empty() {
            eprintln!("Got empty input on stdin");
        }
    }

    use sdp_types::{Direction, Origin, Session};
    let sdp = if input.is_empty() {
        Session::builder(
            Origin::with_ip_addr(
                args.session_id,
                0,
                Ipv4Addr::from_str(&args.sender_addr).context("sender addr")?,
            ),
            "rtpbbin2-precise-sync-example",
        )
        .connection(Ipv4Addr::from_str(&args.outbound_addr).context("outbound addr")?)
        .attribute(Direction::SendOnly)
        .build()
    } else {
        Session::parse(input.as_bytes()).context("parsing input SDP")?
    };

    let main_context = glib::MainContext::default();
    let _guard = main_context.acquire().unwrap();
    let loop_ = glib::MainLoop::new(None, false);

    let time_reference = args.clock.get_time_reference()?;

    let (caps_sender, caps_receiver) = mpsc::channel();
    let pipeline = build_pipeline(&args, &loop_, time_reference.offset, caps_sender)
        .context("building pipeline")?;

    pipeline
        .set_state(gst::State::Ready)
        .context("setting pipeline to Ready")?;

    pipeline.use_clock(Some(&time_reference.clock));

    // set the base_time for the pipeline
    // including a possible time offset to match the target reference clock
    let base_time = time_reference.clock.time();
    pipeline.set_base_time(base_time);
    // tell the pipeline not to re-calculate base_time
    pipeline.set_start_time(gst::ClockTime::NONE);

    let ref_clock_base_time = base_time.wrapping_add(time_reference.offset);

    if args.align_for_offset_0 {
        let payloader = pipeline
            .by_name("audio-payloader")
            .expect("added in build_pipeline");

        // RtpBasePay2 subclasses can only set their timestamp-offset in state <= Ready
        let ref_clock_base_time_rtptime = base_time_to_rtptime(ref_clock_base_time, AUDIO_RATE);
        eprintln!(
            "Forcing timestamp-offset to {ref_clock_base_time_rtptime} for {}",
            payloader.name()
        );
        payloader.set_property("timestamp-offset", ref_clock_base_time_rtptime as i64);
    }

    // Start streaming
    pipeline
        .set_state(gst::State::Playing)
        .context("setting pipeline to Playing")?;

    eprintln!("Playing");
    eprintln!(
        "Reference clock base time {ref_clock_base_time} ({}.{})",
        ref_clock_base_time.seconds(),
        ref_clock_base_time.nseconds() % *gst::ClockTime::SECOND
    );
    if !time_reference.offset.is_zero() {
        eprintln!(
            "Actual clock base time {base_time} ({}.{})",
            base_time.seconds(),
            base_time.nseconds() % *gst::ClockTime::SECOND
        );
    }

    // we only expect one stream here, othewise we would collect them all
    let (payloader, caps) = caps_receiver.recv().expect("payloader still alive");

    let session = pipeline
        .by_name("rtpsend")
        .expect("explicitely named so")
        .emit_by_name::<gst::glib::Object>("get-session", &[&args.session_id]);
    if let Err(err) = print_sdp(
        &args,
        ref_clock_base_time,
        sdp,
        session.property::<gst::Structure>("sdes"),
        &payloader,
        caps,
    ) {
        eprintln!("\n/!\\ Failed to generate SDP:\n{err:#}");
        gst::element_error!(
            pipeline,
            gst::LibraryError::Failed,
            ("Failed to generate SDP: {err:#}")
        );
    };

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
