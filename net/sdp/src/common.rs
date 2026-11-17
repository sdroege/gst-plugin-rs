// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

use anyhow::Context;

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstSapRtcpMode")]
pub(crate) enum RtcpMode {
    #[default]
    #[enum_value(name = "Inactive", nick = "inactive")]
    Inactive,
    #[enum_value(name = "Only receive RTCP", nick = "recvonly")]
    ReceiveOnly,
    #[enum_value(name = "Only send RTCP", nick = "sendonly")]
    SendOnly,
    #[enum_value(name = "Send and receive RTCP", nick = "sendrecv")]
    SendReceive,
}

impl RtcpMode {
    pub(crate) fn receive(self) -> bool {
        matches!(self, Self::ReceiveOnly | Self::SendReceive)
    }

    pub(crate) fn send(self) -> bool {
        matches!(self, Self::SendOnly | Self::SendReceive)
    }
}

// rtprecv/rtpsend share their state process-global via the rtp-id, which
// has to be unique per element instance.
static GLOBAL_RTP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn next_rtp_id(prefix: &str) -> String {
    format!("{prefix}{}", GLOBAL_RTP_ID.fetch_add(1, Ordering::SeqCst))
}

// Checks that the given element factories are available, posting a
// missing element message for each missing one, so that instantiating
// the elements later can not fail, and additionally posts an element
// error and returns a state change error if one is missing
pub(crate) fn ensure_required_elements(
    obj: &impl IsA<gst::Element>,
    elements: &[&'static str],
) -> Result<(), gst::StateChangeError> {
    let missing = elements
        .iter()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .copied()
        .collect::<Vec<_>>();

    if missing.is_empty() {
        return Ok(());
    }

    for name in &missing {
        let _ = obj.post_message(
            gst_pbutils::MissingPluginMessage::builder_for_element(name)
                .src(obj)
                .build(),
        );
    }

    let missing = missing
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    gst::element_error!(
        obj,
        gst::CoreError::MissingPlugin,
        ("Required element(s) {missing} not found")
    );

    Err(gst::StateChangeError)
}

pub(crate) fn remove_element(obj: &impl IsA<gst::Bin>, element: &gst::Element) {
    element.set_locked_state(true);
    let _ = element.set_state(gst::State::Null);
    let _ = obj.remove(element);
}

/// The request pads and elements of a stream chain, taken out of the state for
/// teardown without holding the state mutex.
pub(crate) type StreamChain = (Vec<(gst::Element, gst::Pad)>, Vec<gst::Element>);

/// Stops and removes the elements of a stream chain and releases its request pads.
pub(crate) fn teardown_chain(obj: &impl IsA<gst::Bin>, chain: StreamChain) {
    let (request_pads, elements) = chain;

    for element in &elements {
        element.set_locked_state(true);
        let _ = element.set_state(gst::State::Null);
    }

    for element in &elements {
        let _ = obj.remove(element);
    }

    for (element, pad) in request_pads {
        element.release_request_pad(&pad);
    }
}

// Buffering to decouple the UDP sockets from the rest of the pipeline
pub(crate) fn make_queue(name: &str) -> gst::Element {
    gst::ElementFactory::make("queue")
        .name(name)
        .property("max-size-bytes", 0u32)
        .property(
            "max-size-time",
            gst::ClockTime::from_mseconds(50).nseconds(),
        )
        .property("max-size-buffers", 0u32)
        .build()
        .unwrap()
}

// Up to GStreamer 1.28 included, udpsrc2 triggers a serialized Allocation
// query on Reconfigure, which can block its streaming thread and delay
// timestamping of incoming packets. The fix is only enabled with the optional
// v1_30 feature, so drop the queries here.
pub(crate) fn drop_allocation_queries(udpsrc: &gst::Element) {
    udpsrc.static_pad("src").unwrap().add_probe(
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
}

pub(crate) fn used_socket(element: &gst::Element) -> Result<gio::Socket, anyhow::Error> {
    element
        .property::<glib::Value>("used-socket")
        .get::<gio::Socket>()
        .ok()
        .with_context(|| format!("{} has no socket", element.name()))
}

/// How a udpsrc2 gets its socket.
pub(crate) enum SrcSocket<'a> {
    /// Bind a new socket to `address`:`port` when the element is set to
    /// READY, with the given read timeout (0 = none).
    Bind {
        address: IpAddr,
        port: u32,
        timeout: u64,
    },
    /// Share an already bound socket, which is not closed with the element.
    /// The multicast group is owned by the other element sharing the socket,
    /// so this element does not join it.
    Shared(&'a gio::Socket),
}

/// Creates a udpsrc2 element with the given socket configuration.
///
/// When binding, the element is set to READY already to bind the socket for
/// socket sharing.
pub(crate) fn make_udpsrc2(
    name: &str,
    sock: SrcSocket<'_>,
    caps: Option<&gst::Caps>,
    multicast_iface: Option<&str>,
    source_filter: Option<(&str, bool)>,
) -> Result<gst::Element, anyhow::Error> {
    let mut builder = gst::ElementFactory::make("udpsrc2")
        .name(name)
        .property("multicast-iface", multicast_iface.unwrap_or(""));

    let bind = match sock {
        SrcSocket::Bind {
            address,
            port,
            timeout,
        } => {
            builder = builder
                .property("address", address.to_string())
                .property("port", port)
                .property("timeout", timeout);
            true
        }
        SrcSocket::Shared(socket) => {
            builder = builder
                .property("socket", socket)
                .property("close-socket", false)
                .property("auto-multicast", false);
            false
        }
    };
    if let Some(caps) = caps {
        builder = builder.property("caps", caps);
    }
    if let Some((source_filter, exclusive)) = source_filter {
        builder = builder
            .property("source-filter", source_filter)
            .property("source-filter-exclusive", exclusive);
    }
    let udpsrc = builder.build().unwrap();

    if bind {
        udpsrc
            .set_state(gst::State::Ready)
            .with_context(|| format!("Failed to set {name} to READY"))?;
    }

    drop_allocation_queries(&udpsrc);

    Ok(udpsrc)
}

/// How a udpsink2 gets its socket.
pub(crate) enum SinkSocket<'a> {
    /// Bind a new socket to `bind_port` (0 = ephemeral) when the element is
    /// set to READY, and join the multicast group of the destination if
    /// `auto_multicast`.
    Bind {
        bind_port: u16,
        auto_multicast: bool,
    },
    /// Share an already bound socket, e.g. of the udpsrc2 receiving RTCP, so
    /// that packets are sent from the port they are received on. The socket
    /// is not closed with the element and the multicast group is not joined
    /// again.
    Shared(&'a gio::Socket),
}

/// Creates a udpsink2 element sending to `host`:`port`, never participating
/// in pipeline preroll.
///
/// When binding, the element is set to READY already so its socket can be
/// shared via `used_socket()`; it is synced with the chain once linked.
pub(crate) fn make_udpsink2(
    name: &str,
    host: IpAddr,
    port: u16,
    sock: SinkSocket<'_>,
    multicast_iface: Option<&str>,
    loopback: bool,
) -> Result<gst::Element, anyhow::Error> {
    let mut builder = gst::ElementFactory::make("udpsink2")
        .name(name)
        .property("host", host.to_string())
        .property("port", u32::from(port))
        .property("sync", false)
        .property("async", false)
        .property("multicast-iface", multicast_iface.unwrap_or(""))
        .property("loop", loopback);

    let bind = match sock {
        SinkSocket::Bind {
            bind_port,
            auto_multicast,
        } => {
            builder = builder
                .property("bind-port", u32::from(bind_port))
                .property("auto-multicast", auto_multicast);
            true
        }
        SinkSocket::Shared(socket) => {
            builder = builder
                .property("socket", socket)
                .property("close-socket", false)
                .property("auto-multicast", false);
            false
        }
    };

    let udpsink = builder.build().unwrap();

    if bind {
        udpsink
            .set_state(gst::State::Ready)
            .with_context(|| format!("{name} failed to bind its socket"))?;
    }

    Ok(udpsink)
}

/// Configures the rtpbin2 session `element` (rtpsend or rtprecv) with the
/// per-payload-type caps of `caps`, so that it knows the clock rate and
/// encoding of every payload type before any packet is processed.
pub(crate) fn set_pt_map(element: &gst::Element, session_id: u32, caps: &gst::Caps) {
    let mut pt_map = gst::Structure::new_empty("application/x-rtp2-pt-map");
    for s in caps.iter() {
        let Ok(pt) = s.get::<i32>("payload") else {
            continue;
        };
        pt_map.set(pt.to_string(), gst::Caps::from(s.to_owned()));
    }

    let session = element.emit_by_name::<glib::Object>("get-session", &[&session_id]);
    session.set_property("pt-map", pt_map);
}
