// Copyright (C) 2024-2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

/**
 * SECTION:element-udpsrc2
 * @see_also: udpsrc, udpsink, multiudpsink.
 *
 * `udpsrc2` is a network source that reads UDP packets from the network.
 * It can be combined with RTP depayloaders to implement RTP streaming.
 *
 * The `udpsrc2` element supports automatic port allocation by setting the
 * #GstUdpSrc2:port property to 0. After setting the `udpsrc2` to PAUSED, the
 * allocated port can be obtained by reading the #GstUdpSrc2:port property.
 *
 * `udpsrc2` can read from multicast groups by setting the #GstUdpSrc2:address
 * property to the IP address of the multicast group.
 *
 * Alternatively one can provide a custom socket to `udpsrc2` with the #GstUdpSrc2:socket
 * property, `udpsrc2` will then not allocate a socket itself but use the provided
 * one.
 *
 * The #GstUdpSrc2:caps property is mainly used to give a type to the UDP packet
 * so that they can be autoplugged in GStreamer pipelines. This is very useful
 * for RTP implementations where the format description of the UDP packets is transferred
 * out-of-bounds using SDP or other means.
 *
 * The #GstUdpSrc2:buffer-size property is used to change the default kernel
 * receive buffer size used for receiving packets. The buffer size may be increased for
 * high-volume connections, or may be decreased to limit the possible backlog of
 * incoming data. The system places an absolute limit on these values, on Linux,
 * for example, the default buffer size is typically 50K and can be increased to
 * maximally 100K.
 *
 * The #GstUdpSrc2:skip-first-bytes property is used to strip off an arbitrary
 * number of bytes from the start of the raw UDP packet and can be used to strip
 * off proprietary header, for example.
 *
 * `udpsrc2` is always a live source. It does however not provide a #GstClock,
 * this is left for downstream elements such as an RTP session manager or demuxer
 * (such as an MPEG TS demuxer). As with all live sources, the captured buffers
 * will have their timestamp set to the current running time of the pipeline.
 *
 * If supported, `udpsrc2` uses receive timestamps provided by the socket for timestamping
 * outgoing buffers, which is generally more accurate. Otherwise it uses the time when the
 * buffer was received by the element from the socket.
 *
 * `udpsrc2` implements a #GstURIHandler interface that handles `udp://host:port`
 * type URIs.
 *
 * If the #GstUdpSrc2:timeout property is set to a value bigger than 0, `udpsrc2`
 * will generate an element message named `GstUDPSrcTimeout` if no data was received
 * in the given timeout.
 *
 * The message's structure contains one field:
 *
 * * #guint64 `timeout`: the timeout in nanoseconds that expired when waiting for data.
 *
 * The message is typically used to detect that no UDP packet arrives in the receiver
 * because it is blocked by a firewall.
 *
 * A custom file descriptor can be configured with the #GstUdpSrc2:socket property.
 * The socket will be closed when setting the element to READY by default. This
 * behaviour can be overridden with the #GstUdpSrc2:close-socket property, in which
 * case the application is responsible for closing the file descriptor.
 *
 * ## Examples
 * |[
 * gst-launch-1.0 -v udpsrc2 ! fakesink dump=1
 * ]| A pipeline to read from the default port and dump the UDP packets.
 *
 * To actually generate UDP packets on the default port one can use the
 * `udpsink` element. When running the following pipeline in another terminal, the
 * above mentioned pipeline should dump data packets to the console.
 * |[
 * gst-launch-1.0 -v audiotestsrc ! udpsink
 * ]|
 *
 * |[
 * gst-launch-1.0 -v udpsrc port=0 ! fakesink
 * ]| read UDP packets from a free port.
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_base::{
    prelude::*,
    subclass::{base_src::CreateSuccess, prelude::*},
};

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, LazyLock, Mutex},
};

use crate::net;
use atomic_refcell::AtomicRefCell;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "udpsrc2",
        gst::DebugColorFlags::empty(),
        Some("UDP Source 2"),
    )
});

const WAKER_TOKEN: mio::Token = mio::Token(0);
const SOCKET_TOKEN: mio::Token = mio::Token(1);
const DEFAULT_MULTICAST_IFACE: Option<&str> = None;

#[derive(Debug, Clone)]
struct Settings {
    address: IpAddr,
    port: u16,
    buffer_size: u32,
    mtu: u32,
    caps: Option<gst::Caps>,
    multicast_iface: Option<String>,
    source_filter: Vec<IpAddr>,
    source_filter_exclusive: bool,
    auto_multicast: bool,
    loop_: bool,
    reuse: bool,
    batch_size: u32,
    allow_gro: bool,
    preserve_packetization: bool,
    socket: Option<net::GioSocketWrapper>,
    used_socket: Option<net::GioSocketWrapper>,
    close_socket: bool,
    skip_first_bytes: u32,
    timeout: std::time::Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 5000,
            buffer_size: 0,
            mtu: 1500,
            caps: None,
            multicast_iface: DEFAULT_MULTICAST_IFACE.map(Into::into),
            source_filter: Vec::new(),
            source_filter_exclusive: false,
            auto_multicast: true,
            loop_: true,
            reuse: true,
            batch_size: 16,
            allow_gro: false,
            preserve_packetization: true,
            socket: None,
            used_socket: None,
            close_socket: false,
            skip_first_bytes: 0,
            timeout: std::time::Duration::ZERO,
        }
    }
}

struct State {
    poll: Option<mio::Poll>,
    events: mio::Events,
    socket: Option<net::UdpSocket>,
    waker: Option<Arc<mio::Waker>>,
}

impl Default for State {
    fn default() -> Self {
        State {
            poll: None,
            events: mio::Events::with_capacity(2),
            socket: None,
            waker: None,
        }
    }
}

#[derive(Default)]
pub struct UdpSrc {
    settings: Mutex<Settings>,
    state: AtomicRefCell<State>,
    waker: Mutex<Option<Arc<mio::Waker>>>,
}

#[glib::object_subclass]
impl ObjectSubclass for UdpSrc {
    const NAME: &'static str = "GstUdpSrc2";
    type Type = super::UdpSrc;
    type ParentType = gst_base::PushSrc;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for UdpSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                /**
                 * GstUdpSrc2:address:
                 *
                 * IP address to bind to.
                 *
                 * If this is a multicast address, then the socket will actually bind to `0.0.0.0`
                 * or `::1`. Selection of the interface to use for receiving multicast packets can
                 * be done with the #GstUdpSrc2:multicast-iface property.
                 */
                glib::ParamSpecString::builder("address")
                    .nick("Address")
                    .blurb("IP Address to bind to")
                    .default_value("0.0.0.0")
                    .mutable_ready()
                    .build(),
                /**
                 * GstUdpSrc2:port:
                 *
                 * Port to bind to.
                 *
                 * If this is set to 0 then a free port will be selected and is available via this
                 * property once the element reached READY state.
                 */
                glib::ParamSpecUInt::builder("port")
                    .nick("Port")
                    .blurb("Port to bind to and receive packets from")
                    .default_value(Settings::default().port as u32)
                    .maximum(u16::MAX as u32)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("uri")
                    .nick("URI")
                    .blurb("URI in the form of udp://multicast_group:port")
                    .default_value("udp://0.0.0.0:5000")
                    .mutable_ready()
                    .build(),
                /**
                 * GstUdpSrc2:buffer-size:
                 *
                 * Socket receive buffer size.
                 *
                 * The buffer size may be increased for high-volume connections, or may be
                 * decreased to limit the possible backlog of incoming data. The system places an
                 * absolute limit on these values, on Linux, for example, the default buffer size
                 * is typically 50K and can be increased to maximally 100K.
                 *
                 * Setting this to 0 uses the default receive buffer size.
                 *
                 * On Linux the maximum can be configured via `/proc/sys/net/core/rmem_max`.
                 */
                glib::ParamSpecUInt::builder("buffer-size")
                    .nick("Buffer Size")
                    .blurb("Socket receive buffer size")
                    .default_value(Settings::default().buffer_size)
                    .mutable_ready()
                    .build(),
                /**
                 * GstUdpSrc2:mtu:
                 *
                 * Maximum expected packet size. This directly defines the allocation
                 * size of the receive buffer pool.
                 *
                 * If bigger packets are received then they are discarded.
                 */
                glib::ParamSpecUInt::builder("mtu")
                    .nick("Maximum Packet Size")
                    .blurb("Maximum expected packet size")
                    .default_value(Settings::default().mtu)
                    .minimum(1)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("caps")
                    .nick("Caps")
                    .blurb("Caps of the received stream")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("multicast-iface")
                    .nick("Multicast Interface")
                    .blurb("The network interface on which to join the multicast group. This allows multiple interfaces \
                        separated by comma. (\"eth0,eth1\")")
                    .default_value(DEFAULT_MULTICAST_IFACE)
                    .build(),
                glib::ParamSpecString::builder("source-filter")
                    .nick("Source Filter")
                    .blurb("Comma-separated list of source IP addresses or hostnames for filtering")
                    .build(),
                glib::ParamSpecBoolean::builder("source-filter-exclusive")
                    .nick("Source Filter Exclusive")
                    .blurb("If set to TRUE (exclusive mode) all addresses in source-filter will be filtered out, \
                        otherwise (inclusive mode) only the addresses in source-filter will be accepted.")
                    .default_value(Settings::default().source_filter_exclusive)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("auto-multicast")
                    .nick("Auto Multicast")
                    .blurb("Automatically join/leave multicast groups")
                    .default_value(Settings::default().auto_multicast)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("loop")
                    .nick("Loop")
                    .blurb("Loop back multicast packets")
                    .default_value(Settings::default().loop_)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("reuse")
                    .nick("Reuse")
                    .blurb("Allow port reuse")
                    .default_value(Settings::default().reuse)
                    .mutable_ready()
                    .build(),
               /**
                * GstUdpSrc2:batch-size:
                *
                * Configures how many packets are received and forwarded at once. This is a maximum
                * and if fewer packets are currently available at a time then fewer packets will be
                * processed at once.
                *
                * Note that by enabling #GstUdpSrc2:allow-gro 64 times as many packets will be
                * received if possible.
                */
                glib::ParamSpecUInt::builder("batch-size")
                    .nick("Batch Size")
                    .blurb("batch size")
                    .default_value(Settings::default().batch_size)
                    .minimum(1)
                    .maximum(1024)
                    .mutable_ready()
                    .build(),
               /**
                * GstUdpSrc2:allow-gro:
                *
                * Enable generic receive offload (GRO) if supported on the platform.
                *
                * This allows receiving multiple packets at once and can potentially improve
                * performance.
                *
                * GRO can only be effective under certain conditions, including
                *
                *   - Consecutive packets must have the same size (except for the last, which can be smaller)
                *   - IP/UDP headers must match (source / destination, etc)
                *   - IP ID field must either be all zero or incrementing by one
                *   - Packets must not be fragmented (and probably DF flag is required)
                *   - UDP checksum must be valid and not zero
                *   - NIC/driver/kernel must support it
                *
                * If not possible then a single packet will be received at once.
                *
                * Enabling this has two potential drawbacks:
                *
                *   1. With the current GStreamer design, multiple packets received via GRO need to
                *      be split into individual sub-buffers before pushing downstream. The
                *      splitting of the buffers involves additional heap allocations.
                *   2. Each receive buffer needs to be overallocated: there must be space for 64 packets
                *      of the maximum packet size in each buffer. Even if GRO is not actually used
                *      because one of the conditions above is not fulfilled, the overallocation has
                *      to happen.
                */
                glib::ParamSpecBoolean::builder("allow-gro")
                    .nick("Allow GRO")
                    .blurb("Allow GRO")
                    .default_value(Settings::default().allow_gro)
                    .mutable_ready()
                    .build(),
               /**
                * GstUdpSrc2:preserve-packetization:
                *
                * By default each buffer contains a single UDP packet, i.e. preserves
                * packetization. This is required for various protocols, e.g. RTP.
                *
                * For other formats, e.g. MPEG-TS, this is not required and multiple packets can
                * potentially be stored in a single buffer. Doing so can improve performance.
                */
                glib::ParamSpecBoolean::builder("preserve-packetization")
                    .nick("Preserve Packetization")
                    .blurb("Preserve UDP packetization instead of potentially outputting multiple packets in the same buffer")
                    .default_value(Settings::default().preserve_packetization)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("socket")
                    .nick("Socket")
                    .blurb("Socket to use for UDP reception. (None == allocate)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecObject::builder::<gio::Socket>("used-socket")
                    .nick("Used Socket")
                    .blurb("Socket currently in use for UDP reception. (None = no socket)")
                    .read_only()
                    .build(),
                glib::ParamSpecBoolean::builder("close-socket")
                    .nick("Close Socket")
                    .blurb("Close socket on state change if passed as property")
                    .default_value(Settings::default().close_socket)
                    .mutable_ready()
                    .build(),
               /**
                * GstUdpSrc2:skip-first-bytes:
                *
                * Number of bytes to skip for each UDP packet.
                *
                * This can be used to strip off proprietary headers, for example.
                */
                glib::ParamSpecUInt::builder("skip-first-bytes")
                    .nick("Skip First Bytes")
                    .blurb("Number of bytes to skip for each UDP packet")
                    .default_value(Settings::default().skip_first_bytes)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("timeout")
                    .nick("Timeout")
                    .blurb("Post a message after timeout nanoseconds (0 = disabled)")
                    .default_value(Settings::default().timeout.as_nanos() as u64)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_live(true);
        obj.set_format(gst::Format::Time);
        obj.set_element_flags(gst::ElementFlags::REQUIRE_CLOCK);
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "address" => {
                let mut settings = self.settings.lock().unwrap();
                let address = value.get::<Option<&str>>().expect("type checked upstream");

                let address = match address {
                    Some(address) => address,
                    None => {
                        gst::info!(
                            CAT,
                            imp = self,
                            "Changing address from {} to 0.0.0.0",
                            settings.address,
                        );
                        settings.address = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
                        return;
                    }
                };

                let address = match address.parse::<IpAddr>() {
                    Ok(address) => address,
                    Err(err) => {
                        gst::error!(CAT, imp = self, "Failed parsing address {address}: {err}",);
                        return;
                    }
                };

                gst::info!(
                    CAT,
                    imp = self,
                    "Changing address from {} to {address}",
                    settings.address,
                );
                settings.address = address;
            }
            "port" => {
                let mut settings = self.settings.lock().unwrap();
                let port = value.get::<u32>().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing port from {} to {port}",
                    settings.port,
                );
                settings.port = u16::try_from(port).unwrap_or(0);
            }
            "uri" => {
                let uri = value.get().expect("type checked upstream");
                if let Err(err) = self.obj().set_uri(uri) {
                    gst::warning!(CAT, imp = self, "Failed setting URI '{uri}': {err}");
                }
            }
            "buffer-size" => {
                let mut settings = self.settings.lock().unwrap();
                let buffer_size = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing buffer-size from {} to {buffer_size}",
                    settings.buffer_size,
                );
                settings.buffer_size = buffer_size;
            }
            "mtu" => {
                let mut settings = self.settings.lock().unwrap();
                let mtu = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing mtu from {} to {mtu}",
                    settings.mtu,
                );
                settings.mtu = mtu;
            }
            "caps" => {
                let mut settings = self.settings.lock().unwrap();
                let caps = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing caps from {:?} to {caps:?}",
                    settings.caps,
                );
                settings.caps = caps;
            }
            "multicast-iface" => {
                let mut settings = self.settings.lock().unwrap();
                let multicast_iface = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing multicast-iface from {:?} to {multicast_iface:?}",
                    settings.multicast_iface,
                );
                settings.multicast_iface = multicast_iface;
            }
            "source-filter" => {
                let mut settings = self.settings.lock().unwrap();
                let source_filter = value
                    .get::<Option<&str>>()
                    .expect("type checked upstream")
                    .unwrap_or("");

                match parse_source_filter(source_filter) {
                    Err(err) => {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Failed setting source-filter '{source_filter}': {err}"
                        );
                    }
                    Ok(source_filter) => {
                        gst::info!(
                            CAT,
                            imp = self,
                            "Changing source-filter from {:?} to {source_filter:?}",
                            settings.source_filter,
                        );
                        settings.source_filter = source_filter;
                    }
                }
            }
            "source-filter-exclusive" => {
                let mut settings = self.settings.lock().unwrap();
                let source_filter_exclusive = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing source-filter-exclusive from {} to {source_filter_exclusive}",
                    settings.source_filter_exclusive,
                );
                settings.source_filter_exclusive = source_filter_exclusive;
            }
            "auto-multicast" => {
                let mut settings = self.settings.lock().unwrap();
                let auto_multicast = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing auto-multicast from {:?} to {auto_multicast:?}",
                    settings.auto_multicast,
                );
                settings.auto_multicast = auto_multicast;
            }
            "loop" => {
                let mut settings = self.settings.lock().unwrap();
                let loop_ = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing loop from {:?} to {loop_:?}",
                    settings.loop_,
                );
                settings.loop_ = loop_;
            }
            "reuse" => {
                let mut settings = self.settings.lock().unwrap();
                let reuse = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing reuse from {:?} to {reuse:?}",
                    settings.reuse,
                );
                settings.reuse = reuse;
            }
            "batch-size" => {
                let mut settings = self.settings.lock().unwrap();
                let batch_size = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing batch-size from {} to {batch_size}",
                    settings.batch_size,
                );
                settings.batch_size = batch_size;
            }
            "allow-gro" => {
                let mut settings = self.settings.lock().unwrap();
                let allow_gro = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing allow-gro from {} to {allow_gro}",
                    settings.allow_gro,
                );
                settings.allow_gro = allow_gro;
            }
            "preserve-packetization" => {
                let mut settings = self.settings.lock().unwrap();
                let preserve_packetization = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing preserve-packetization from {:?} to {preserve_packetization:?}",
                    settings.preserve_packetization,
                );
                settings.preserve_packetization = preserve_packetization;
            }
            "socket" => {
                let mut settings = self.settings.lock().unwrap();
                if let Some(socket) = settings.socket.take()
                    && socket.is_external()
                    && settings.close_socket
                {
                    use gio::prelude::*;

                    let _ = socket.as_socket().close();
                }

                settings.socket = value
                    .get::<Option<gio::Socket>>()
                    .expect("type checked upstream")
                    .map(|socket| net::GioSocketWrapper::new(socket, true));
            }
            "used-socket" => {
                unreachable!();
            }
            "close-socket" => {
                let mut settings = self.settings.lock().unwrap();
                let close_socket = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing close-socket from {:?} to {close_socket:?}",
                    settings.close_socket,
                );
                settings.close_socket = close_socket;
            }
            "skip-first-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                let skip_first_bytes = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing skip-first-bytes from {} to {skip_first_bytes}",
                    settings.skip_first_bytes,
                );
                settings.skip_first_bytes = skip_first_bytes;
            }
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let timeout = std::time::Duration::from_nanos(
                    value.get::<u64>().expect("type checked upstream"),
                );
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing timeout from {:#?} to {timeout:#?}",
                    settings.timeout,
                );
                settings.timeout = timeout;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "address" => {
                let settings = self.settings.lock().unwrap();
                settings.address.to_string().to_value()
            }
            "port" => {
                let settings = self.settings.lock().unwrap();
                (settings.port as u32).to_value()
            }
            "uri" => self.obj().uri().to_value(),
            "buffer-size" => {
                let settings = self.settings.lock().unwrap();
                settings.buffer_size.to_value()
            }
            "mtu" => {
                let settings = self.settings.lock().unwrap();
                settings.mtu.to_value()
            }
            "caps" => {
                let settings = self.settings.lock().unwrap();
                settings.caps.to_value()
            }
            "multicast-iface" => {
                let settings = self.settings.lock().unwrap();
                settings.multicast_iface.to_value()
            }
            "source-filter" => {
                let settings = self.settings.lock().unwrap();
                if settings.source_filter.is_empty() {
                    None::<&str>.to_value()
                } else {
                    use std::fmt::Write;

                    let mut s = String::new();

                    for (i, addr) in settings.source_filter.iter().enumerate() {
                        if i > 0 {
                            s.push(',');
                        }
                        let _ = write!(&mut s, "{addr}");
                    }
                    s.to_value()
                }
            }
            "source-filter-exclusive" => {
                let settings = self.settings.lock().unwrap();
                settings.source_filter_exclusive.to_value()
            }
            "auto-multicast" => {
                let settings = self.settings.lock().unwrap();
                settings.auto_multicast.to_value()
            }
            "loop" => {
                let settings = self.settings.lock().unwrap();
                settings.loop_.to_value()
            }
            "reuse" => {
                let settings = self.settings.lock().unwrap();
                settings.reuse.to_value()
            }
            "batch-size" => {
                let settings = self.settings.lock().unwrap();
                settings.batch_size.to_value()
            }
            "allow-gro" => {
                let settings = self.settings.lock().unwrap();
                settings.allow_gro.to_value()
            }
            "preserve-packetization" => {
                let settings = self.settings.lock().unwrap();
                settings.preserve_packetization.to_value()
            }
            "socket" => {
                let settings = self.settings.lock().unwrap();
                settings
                    .socket
                    .as_ref()
                    .map(net::GioSocketWrapper::as_socket)
                    .to_value()
            }
            "used-socket" => {
                let settings = self.settings.lock().unwrap();
                settings
                    .used_socket
                    .as_ref()
                    .map(net::GioSocketWrapper::as_socket)
                    .to_value()
            }
            "close-socket" => {
                let settings = self.settings.lock().unwrap();
                settings.close_socket.to_value()
            }
            "skip-first-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.skip_first_bytes.to_value()
            }
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                (settings.timeout.as_nanos() as u64).to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for UdpSrc {}

impl ElementImpl for UdpSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "UDP Source",
                "Source/Network",
                "Reads an UDP stream",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_any();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseSrcImpl for UdpSrc {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut settings = self.settings.lock().unwrap();

        let mut state = self.state.borrow_mut();

        let poll = mio::Poll::new().map_err(|err| {
            gst::error_msg!(gst::ResourceError::OpenRead, ["Failed create poll: {err}"])
        })?;
        let waker = Arc::new(
            mio::Waker::new(poll.registry(), WAKER_TOKEN).map_err(|err| {
                gst::error_msg!(gst::ResourceError::OpenRead, ["Failed create waker: {err}"])
            })?,
        );

        {
            let mut waker_storage = self.waker.lock().unwrap();
            *waker_storage = Some(waker.clone());
        }

        let saddr = SocketAddr::new(settings.address, settings.port);

        let mut notify_address = false;
        let mut notify_port = false;
        let mut socket;

        if let Some(ref set_socket) = settings.socket {
            socket = net::UdpSocket::wrap_socket(
                &*self.obj(),
                set_socket,
                &settings.source_filter,
                settings.source_filter_exclusive,
                settings.batch_size,
                settings.allow_gro,
                settings.mtu,
                settings.preserve_packetization,
                settings.skip_first_bytes,
            )
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to wrap application socket: {err:?}"]
                )
            })?;

            let local_addr = socket.local_addr();
            gst::debug!(CAT, imp = self, "Application socket bound to {local_addr}");
            settings.port = local_addr.port();
            settings.address = local_addr.ip();
            notify_port = true;
            notify_address = true;
        } else {
            socket = net::UdpSocket::bind(
                &*self.obj(),
                saddr,
                settings.reuse,
                settings.multicast_iface.as_deref(),
                &settings.source_filter,
                settings.source_filter_exclusive,
                settings.auto_multicast,
                settings.loop_,
                settings.buffer_size,
                settings.batch_size,
                settings.allow_gro,
                settings.mtu,
                settings.preserve_packetization,
                settings.skip_first_bytes,
            )
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to create socket: {err:?}"]
                )
            })?;

            if settings.port == 0 {
                let port = socket.local_addr().port();
                gst::debug!(CAT, imp = self, "Bound to port {port}");
                settings.port = port;
                notify_port = true;
            }
        }

        poll.registry()
            .register(&mut socket, SOCKET_TOKEN, mio::Interest::READABLE)
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to register socket: {err}"]
                )
            })?;

        settings.used_socket = Some(socket.socket_wrapper().clone());

        state.poll = Some(poll);
        state.waker = Some(waker);
        state.socket = Some(socket);

        let caps = settings.caps.clone();
        drop(state);
        drop(settings);

        if let Some(ref caps) = caps {
            let _ = self.obj().set_caps(caps);
        }

        if notify_port {
            self.obj().notify("port");
        }
        if notify_address {
            self.obj().notify("address");
        }
        self.obj().notify("used-socket");

        gst::info!(CAT, imp = self, "Started");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.borrow_mut();
        let mut settings = self.settings.lock().unwrap();
        if let Some(socket) = settings.socket.take()
            && socket.is_external()
            && settings.close_socket
        {
            use gio::prelude::*;

            let _ = socket.as_socket().close();
        }
        *state = State::default();
        *self.waker.lock().unwrap() = None;

        gst::info!(CAT, imp = self, "Stopped");

        Ok(())
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Unlocking");
        if let Some(waker) = self.waker.lock().unwrap().take() {
            let _ = waker.wake();
        }
        gst::debug!(CAT, imp = self, "Unlocked");

        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Stopping unlocking");
        let state = self.state.borrow();
        if let Some(ref waker) = state.waker {
            let mut waker_storage = self.waker.lock().unwrap();
            *waker_storage = Some(waker.clone());
        }
        gst::debug!(CAT, imp = self, "Stopped unlocking");

        Ok(())
    }
}

impl PushSrcImpl for UdpSrc {
    fn create(
        &self,
        _buffer: Option<&mut gst::BufferRef>,
    ) -> Result<CreateSuccess, gst::FlowError> {
        let mut state = self.state.borrow_mut();
        let State {
            poll,
            events,
            socket,
            waker,
        } = &mut *state;

        {
            let mut waker_storage = self.waker.lock().unwrap();
            *waker_storage = waker.clone();
        }

        let poll = poll.as_mut().unwrap();
        let socket = socket.as_mut().unwrap();

        let timeout = self.settings.lock().unwrap().timeout;
        let timeout = if timeout.is_zero() {
            None
        } else {
            Some(timeout)
        };

        'outer_loop: loop {
            // First try reading packets

            match socket.recv() {
                Ok(None) => {}
                Ok(Some(net::BufferOrList::Buffer(buffer))) => {
                    return Ok(CreateSuccess::NewBuffer(buffer));
                }
                Ok(Some(net::BufferOrList::List(list))) => {
                    return Ok(CreateSuccess::NewBufferList(list));
                }
                Err(err) => {
                    gst::error!(CAT, imp = self, "Receive error: {err:?}");
                    return Err(gst::FlowError::Error);
                }
            }

            // Otherwise wait for packets to be available

            let mut start_time = std::time::Instant::now();
            loop {
                let remaining_timeout = if let Some(timeout) = timeout {
                    let remaining_timeout = timeout.saturating_sub(start_time.elapsed());

                    if remaining_timeout.is_zero() {
                        gst::debug!(CAT, imp = self, "Timed out");

                        let _ = self.post_message(
                            gst::message::Element::builder(
                                gst::Structure::builder("GstUDPSrcTimeout")
                                    .field("timeout", timeout.as_nanos() as u64)
                                    .build(),
                            )
                            .src(&*self.obj())
                            .build(),
                        );
                        start_time = std::time::Instant::now();
                        Some(timeout)
                    } else {
                        Some(remaining_timeout)
                    }
                } else {
                    None
                };

                gst::trace!(
                    CAT,
                    imp = self,
                    "Polling with timeout {remaining_timeout:?}"
                );

                if let Err(err) = poll.poll(events, remaining_timeout) {
                    // If timed out, handle above.
                    //
                    // poll() can also just return Ok()) without any events on
                    // timeout so we have to handle it above anyway.
                    if err.kind() == std::io::ErrorKind::TimedOut {
                        continue;
                    }

                    gst::error!(CAT, imp = self, "Poll error: {err:?}");
                    return Err(gst::FlowError::Error);
                };

                for event in events.iter() {
                    if event.token() == WAKER_TOKEN {
                        let waker_storage = self.waker.lock().unwrap();
                        if waker_storage.is_none() {
                            gst::debug!(CAT, imp = self, "Flushing");
                            return Err(gst::FlowError::Flushing);
                        }
                    } else if event.token() == SOCKET_TOKEN {
                        if event.is_readable() {
                            gst::trace!(CAT, imp = self, "Socket readable again");
                            continue 'outer_loop;
                        } else if event.is_write_closed() {
                            gst::debug!(CAT, imp = self, "Socket unexpectedly write closed");
                            // This can happen receiving an ICMP error
                            // when the socket is bound to the same port as a UDP sender
                            continue 'outer_loop;
                        } else if event.is_read_closed() {
                            gst::debug!(CAT, imp = self, "Socket unexpectedly read closed");
                            return Err(gst::FlowError::Error);
                        } else if event.is_error() {
                            gst::error!(CAT, imp = self, "Socket error");
                            return Err(gst::FlowError::Error);
                        }
                    } else {
                        // Spurious wakeup
                    }
                }
            }
        }
    }
}

impl URIHandlerImpl for UdpSrc {
    const URI_TYPE: gst::URIType = gst::URIType::Src;

    fn protocols() -> &'static [&'static str] {
        &["udp"]
    }

    fn uri(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();
        let mut uri = if settings.address.is_ipv6() {
            format!("udp://[{}]:{}", settings.address, settings.port)
        } else {
            format!("udp://{}:{}", settings.address, settings.port)
        };

        if !settings.source_filter.is_empty() {
            uri.push_str("?source-filter=");
            for (i, addr) in settings.source_filter.iter().enumerate() {
                if i > 0 {
                    uri.push(',');
                }
                uri.push_str(&addr.to_string());
            }
            if settings.source_filter_exclusive {
                uri.push_str("&source-filter-exclusive=true");
            }
        }

        Some(uri)
    }

    fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
        let (addr, port, source_filter, source_filter_exclusive) = parse_uri(uri)?;

        let mut settings = self.settings.lock().unwrap();

        gst::debug!(
            CAT,
            imp = self,
            "Setting address to {addr} and port to {port}"
        );

        settings.address = addr;
        settings.port = port;

        gst::debug!(
            CAT,
            imp = self,
            "Setting source-filter to {source_filter:?} (exclusive: {source_filter_exclusive})"
        );
        settings.source_filter = source_filter;
        settings.source_filter_exclusive = source_filter_exclusive;

        drop(settings);

        self.obj().notify("address");
        self.obj().notify("port");
        self.obj().notify("source-filter");
        self.obj().notify("source-filter-exclusive");

        Ok(())
    }
}

fn parse_uri(uri: &str) -> Result<(IpAddr, u16, Vec<IpAddr>, bool), glib::Error> {
    let Some((scheme, remainder)) = uri.split_once("://") else {
        return Err(glib::Error::new(
            gst::URIError::BadUri,
            "Invalid URI format",
        ));
    };

    if scheme.to_lowercase() != "udp" {
        return Err(glib::Error::new(
            gst::URIError::UnsupportedProtocol,
            format!("Unsupported URI scheme {scheme}").as_str(),
        ));
    }

    let (addr, remainder) = if let Some(remainder) = remainder.strip_prefix('[') {
        let Some((ip, remainder)) = remainder.split_once(']') else {
            return Err(glib::Error::new(
                gst::URIError::BadUri,
                "Invalid IPv6 address in URI",
            ));
        };

        let Some(remainder) = remainder.strip_prefix(':') else {
            return Err(glib::Error::new(
                gst::URIError::BadUri,
                "Missing port in URI",
            ));
        };

        (
            IpAddr::V6(ip.parse::<Ipv6Addr>().map_err(|err| {
                glib::Error::new(
                    gst::URIError::BadUri,
                    format!("Invalid URI IPv6 address: {err}").as_str(),
                )
            })?),
            remainder,
        )
    } else {
        let Some((host, remainder)) = remainder.split_once(':') else {
            return Err(glib::Error::new(
                gst::URIError::BadUri,
                "Missing port in URI",
            ));
        };

        let ip = if let Ok(ip) = host.parse::<Ipv4Addr>() {
            IpAddr::V4(ip)
        } else {
            use std::net::ToSocketAddrs;

            if host.is_empty() {
                return Err(glib::Error::new(
                    gst::URIError::BadUri,
                    "Invalid empty URI host",
                ));
            }

            let saddr = (host, 0u16)
                .to_socket_addrs()
                .map_err(|err| {
                    glib::Error::new(
                        gst::URIError::BadUri,
                        format!("Couldn't resolve URI host: {err}").as_str(),
                    )
                })?
                .next()
                .ok_or_else(|| {
                    glib::Error::new(gst::URIError::BadUri, "Couldn't resolve URI host")
                })?;

            saddr.ip()
        };

        (ip, remainder)
    };

    let (port, source_filter, source_filter_exclusive) = if let Some((port, query)) =
        remainder.split_once('?')
    {
        let mut source_filter = Vec::new();
        let mut source_filter_exclusive = false;

        for (key, value) in query.split('&').filter_map(|s| s.split_once('=')) {
            match key {
                "source-filter" => {
                    source_filter = parse_source_filter(value)?;
                }
                "source-filter-exclusive" => {
                    source_filter_exclusive = match value {
                        "true" | "1" => true,
                        "false" | "0" => false,
                        _ => {
                            return Err(glib::Error::new(
                                gst::URIError::BadUri,
                                format!("Invalid source-filter-exclusive value {value}").as_str(),
                            ));
                        }
                    };
                }
                "multicast-source" => {
                    // Backwards compatibility with old udpsrc. Theoretically it supported mixed
                    // inclusive and exclusive filters, which made no sense and only inclusive
                    // filters we supported anyway so that's what we do here as a best effort.
                    source_filter = parse_multicast_source(value)?;
                    source_filter_exclusive = false;
                }
                _ => {}
            }
        }

        (port, source_filter, source_filter_exclusive)
    } else {
        (remainder, Vec::new(), false)
    };

    let port = match port.parse::<u16>() {
        Ok(port) => port,
        Err(err) => {
            return Err(glib::Error::new(
                gst::URIError::BadUri,
                format!("Invalid URI port: {err}").as_str(),
            ));
        }
    };

    Ok((addr, port, source_filter, source_filter_exclusive))
}

fn parse_source_filter(source_filter: &str) -> Result<Vec<IpAddr>, glib::Error> {
    let mut addrs = Vec::new();

    if source_filter.is_empty() {
        return Ok(addrs);
    }

    for addr_str in source_filter.split(',') {
        if addr_str.is_empty() {
            continue;
        }

        let addr = match addr_str.parse::<IpAddr>() {
            Ok(addr) => addr,
            Err(_err) => {
                use std::net::ToSocketAddrs;

                let saddr = (addr_str, 0u16)
                    .to_socket_addrs()
                    .map_err(|err| {
                        glib::Error::new(
                            gst::URIError::BadUri,
                            format!("Couldn't resolve source filter address: {err}").as_str(),
                        )
                    })?
                    .next()
                    .ok_or_else(|| {
                        glib::Error::new(
                            gst::URIError::BadUri,
                            "Couldn't resolve source filter address",
                        )
                    })?;

                saddr.ip()
            }
        };

        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }

    Ok(addrs)
}

fn parse_multicast_source(mut multicast_source: &str) -> Result<Vec<IpAddr>, glib::Error> {
    let mut addrs = Vec::new();

    while !multicast_source.is_empty() {
        let (positive, remainder) = if let Some(remainder) = multicast_source.strip_prefix('+') {
            (true, remainder)
        } else if let Some(remainder) = multicast_source.strip_prefix('-') {
            (false, remainder)
        } else {
            // Assume it's a positive source
            (true, multicast_source)
        };

        let next_idx = remainder.match_indices(['+', '-']).next();
        let (addr, remainder) = next_idx
            .map(|(next_idx, _)| remainder.split_at(next_idx))
            .unwrap_or((remainder, ""));

        let addr = {
            use std::net::ToSocketAddrs;

            if addr.is_empty() {
                return Err(glib::Error::new(
                    gst::URIError::BadUri,
                    "Invalid empty URI host",
                ));
            }

            let saddr = (addr, 0u16)
                .to_socket_addrs()
                .map_err(|err| {
                    glib::Error::new(
                        gst::URIError::BadUri,
                        format!("Couldn't resolve URI host: {err}").as_str(),
                    )
                })?
                .next()
                .ok_or_else(|| {
                    glib::Error::new(gst::URIError::BadUri, "Couldn't resolve URI host")
                })?;

            saddr.ip()
        };

        if positive {
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        } else {
            // Negative filters are ignored here as old udpsrc did not support them anyway.
        }

        multicast_source = remainder;
    }

    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_uri() {
        let (addr, port, _source_filter, _exclusive) = parse_uri("udp://0.0.0.0:5000").unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);

        let (addr, port, _source_filter, _exclusive) = parse_uri("udp://[::]:5000").unwrap();
        assert_eq!(addr, Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0));
        assert_eq!(port, 5000);

        let (_addr, port, _source_filter, _exclusive) = parse_uri("udp://localhost:5000").unwrap();
        // We don't know what localhost actually maps to
        assert_eq!(port, 5000);

        let (addr, port, _source_filter, _exclusive) = parse_uri("udp://0.0.0.0:5000?").unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);

        let (addr, port, _source_filter, _exclusive) =
            parse_uri("udp://0.0.0.0:5000?foo=bar&baz=baz").unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);

        let (addr, port, source_filter, exclusive) =
            parse_uri("udp://0.0.0.0:5000?foo=bar&multicast-source=+127.0.0.1").unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);
        assert_eq!(source_filter, vec![Ipv4Addr::new(127, 0, 0, 1)]);
        assert!(!exclusive);

        let (addr, port, source_filter, exclusive) =
            parse_uri("udp://0.0.0.0:5000?multicast-source=+127.0.0.1+127.0.0.2").unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);
        assert_eq!(
            source_filter,
            vec![Ipv4Addr::new(127, 0, 0, 1), Ipv4Addr::new(127, 0, 0, 2)]
        );
        assert!(!exclusive);

        let (addr, port, source_filter, exclusive) =
            parse_uri("udp://0.0.0.0:5000?multicast-source=127.0.0.1-127.0.0.2").unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);
        assert_eq!(source_filter, vec![Ipv4Addr::new(127, 0, 0, 1)]);
        assert!(!exclusive);

        let (addr, port, source_filter, exclusive) =
            parse_uri("udp://0.0.0.0:5000?multicast-source=-127.0.0.1").unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);
        assert!(source_filter.is_empty());
        assert!(!exclusive);

        let (addr, port, source_filter, exclusive) =
            parse_uri("udp://0.0.0.0:5000?source-filter=127.0.0.1,127.0.0.2").unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);
        assert_eq!(
            source_filter,
            vec![Ipv4Addr::new(127, 0, 0, 1), Ipv4Addr::new(127, 0, 0, 2)]
        );
        assert!(!exclusive);

        let (addr, port, source_filter, exclusive) = parse_uri(
            "udp://0.0.0.0:5000?source-filter=127.0.0.1,127.0.0.2&source-filter-exclusive=false",
        )
        .unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);
        assert_eq!(
            source_filter,
            vec![Ipv4Addr::new(127, 0, 0, 1), Ipv4Addr::new(127, 0, 0, 2)]
        );
        assert!(!exclusive);

        let (addr, port, source_filter, exclusive) =
            parse_uri("udp://0.0.0.0:5000?source-filter=127.0.0.1&source-filter-exclusive=true")
                .unwrap();
        assert_eq!(addr, Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(port, 5000);
        assert_eq!(source_filter, vec![Ipv4Addr::new(127, 0, 0, 1)]);
        assert!(exclusive);

        let Err(err) = parse_uri("udp://") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));

        let Err(err) = parse_uri("udpppp://") else {
            unreachable!();
        };
        assert_eq!(
            err.kind::<gst::URIError>(),
            Some(gst::URIError::UnsupportedProtocol)
        );

        let Err(err) = parse_uri("udp://::1:5000") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));

        let Err(err) = parse_uri("udp://127.0.0.1") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));

        let Err(err) = parse_uri("udp://:1") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));

        let Err(err) = parse_uri("udp://::1") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));

        let Err(err) = parse_uri("udp://[::1]") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));

        let Err(err) = parse_uri("udp://[localhost]") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));

        let Err(err) = parse_uri("udp://0.0.0.0/test") else {
            unreachable!();
        };
        assert_eq!(err.kind::<gst::URIError>(), Some(gst::URIError::BadUri));
    }
}
