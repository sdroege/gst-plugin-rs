// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

/**
 * SECTION:element-udpsink2
 * @see_also: udpsrc2, multiudpsink2, udpsink.
 *
 * `udpsink2` is a network sink that sends UDP packets to a single client.
 * It can be combined with RTP payloaders to implement RTP streaming.
 *
 * The destination is configured with the #GstUdpSink2:host and
 * #GstUdpSink2:port properties, which default to `127.0.0.1` and `5004`.
 * Hostnames are resolved when the destination is set, and IP addresses can be
 * given directly, including IPv6 literals such as `::1`.
 *
 * The destination can also be set with the #GstUdpSink2:uri property.
 * `udpsink2` implements a #GstURIHandler interface that handles
 * `udp://host:port` type URIs. IPv6 addresses must be enclosed in brackets
 * within the URI, for example `udp://[::1]:5004`.
 *
 * `udpsink2` can send to multicast groups by setting the #GstUdpSink2:host
 * property to a multicast address. It automatically joins and leaves the
 * multicast group as the destination is changed, unless disabled with the
 * #GstBaseUdpSink2:auto-multicast property. The interface used to
 * join the group can be set with the #GstBaseUdpSink2:multicast-iface
 * property. The multicast TTL and loopback behaviour are controlled with the
 * #GstBaseUdpSink2:ttl-mc and #GstBaseUdpSink2:loop properties, and the
 * unicast TTL with the #GstBaseUdpSink2:ttl property.
 *
 * Alternatively one can provide a custom socket to `udpsink2` with the
 * #GstBaseUdpSink2:socket property. In that case `udpsink2` will not allocate a
 * socket itself but use the provided one. A second socket for IPv6 can be
 * provided with the #GstBaseUdpSink2:socket-v6 property. The sockets
 * currently in use can be read back with the read-only #GstBaseUdpSink2:used-socket
 * and #GstBaseUdpSink2:used-socket-v6 properties. A provided socket is closed
 * when setting the element to READY by default. This behaviour can be
 * overridden with the #GstBaseUdpSink2:close-socket property, in which case
 * the application is responsible for closing the socket.
 *
 * The #GstBaseUdpSink2:buffer-size property is used to change the default
 * kernel send buffer size used for sending packets. The buffer size may be
 * increased for high-volume connections, or may be decreased to limit the
 * possible backlog of outgoing data. The system places an absolute limit on
 * these values, on Linux, for example, the default buffer size is typically
 * 50K and can be increased to maximally 100K.
 *
 * The socket can be bound to a specific address and port with the
 * #GstBaseUdpSink2:bind-address and #GstBaseUdpSink2:bind-port properties.
 * The Quality of Service differentiated services code point can be set with
 * the #GstBaseUdpSink2:qos-dscp property.
 *
 * The #GstBaseUdpSink2:bytes-served and #GstBaseUdpSink2:bytes-to-serve
 * properties report the total number of bytes sent to the client and the
 * number of bytes received to serve to the client.
 *
 * ## Examples
 * |[
 * gst-launch-1.0 -v audiotestsrc ! udpsink2 host=127.0.0.1 port=5000
 * ]| Send audio to a UDP destination.
 *
 * |[
 * gst-launch-1.0 -v audiotestsrc ! udpsink2 uri=udp://239.255.0.1:5000
 * ]| Send audio to a multicast group using a URI.
 *
 * To actually receive the packets sent by the above pipeline one can use the
 * `udpsrc2` element. When running the following pipeline in another terminal,
 * the above mentioned pipeline should send packets to it.
 * |[
 * gst-launch-1.0 -v udpsrc2 port=5000 ! fakesink dump=1
 * ]|
 *
 * Since: plugins-rs-0.16.0
 */
use gst::{glib, prelude::*, subclass::prelude::*};
use gst_base::subclass::prelude::*;

use std::sync::LazyLock;

use crate::baseudpsink::BaseUdpSinkExt;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "udpsink2",
        gst::DebugColorFlags::empty(),
        Some("UDP Sink 2"),
    )
});

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 5004;

#[derive(Default)]
pub struct UdpSink;

#[glib::object_subclass]
impl ObjectSubclass for UdpSink {
    const NAME: &'static str = "GstUdpSink2";
    type Type = super::UdpSink;
    type ParentType = crate::baseudpsink::BaseUdpSink;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for UdpSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("host")
                    .nick("Host")
                    .blurb("Host to send packets to")
                    .default_value(DEFAULT_HOST)
                    .build(),
                glib::ParamSpecUInt::builder("port")
                    .nick("Port")
                    .blurb("Port to send packets to")
                    .default_value(DEFAULT_PORT as u32)
                    .maximum(u16::MAX as u32)
                    .build(),
                glib::ParamSpecString::builder("uri")
                    .nick("URI")
                    .blurb("URI in the form of udp://host:port")
                    .default_value("udp://127.0.0.1:5004")
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let obj = self.obj();
        match pspec.name() {
            "host" => {
                let host = value.get::<Option<&str>>().expect("type checked upstream");
                let host = host.unwrap_or(DEFAULT_HOST);
                let port = obj
                    .get_first_client()
                    .map(|(c, _)| c.port)
                    .unwrap_or(DEFAULT_PORT);
                obj.set_client(host, port);
            }
            "port" => {
                let port = value.get::<u32>().expect("type checked upstream") as u16;
                let host = obj
                    .get_first_client()
                    .map(|(c, _)| c.host.as_str().to_owned())
                    .unwrap_or(DEFAULT_HOST.to_string());
                obj.set_client(&host, port);
            }
            "uri" => {
                let uri = value.get::<Option<&str>>().expect("type checked upstream");
                if let Some(uri) = uri {
                    if let Err(err) = self.obj().set_uri(uri) {
                        gst::warning!(CAT, imp = self, "Failed setting URI '{uri}': {err}");
                    }
                } else {
                    obj.set_client(DEFAULT_HOST, DEFAULT_PORT);
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let obj = self.obj();
        match pspec.name() {
            "host" => obj
                .get_first_client()
                .map(|(c, _)| c.host.as_str().to_owned())
                .unwrap_or(DEFAULT_HOST.to_string())
                .to_value(),
            "port" => (obj
                .get_first_client()
                .map(|(c, _)| c.port)
                .unwrap_or(DEFAULT_PORT) as u32)
                .to_value(),
            "uri" => self.uri().unwrap_or_default().to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        // Configure a default client if none was set during construction
        let obj = self.obj();
        if obj.get_first_client().is_none() {
            obj.set_client(DEFAULT_HOST, DEFAULT_PORT);
        }
    }
}

impl GstObjectImpl for UdpSink {}

impl ElementImpl for UdpSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "UDP Sink",
                "Sink/Network",
                "Sends UDP packets",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
}

impl BaseSinkImpl for UdpSink {}

impl crate::baseudpsink::BaseUdpSinkImpl for UdpSink {}

impl URIHandlerImpl for UdpSink {
    const URI_TYPE: gst::URIType = gst::URIType::Sink;

    fn protocols() -> &'static [&'static str] {
        &["udp"]
    }

    fn uri(&self) -> Option<String> {
        let (client, _) = self.obj().get_first_client()?;
        Some(format!(
            "udp://{}:{}",
            client.host.uri_string(),
            client.port
        ))
    }

    fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
        let (host, port) = crate::uri::parse_uri_for_sink(uri)?;

        gst::debug!(CAT, imp = self, "Setting host to {host} and port to {port}");

        self.obj().set_client(host.as_str(), port);

        Ok(())
    }
}
