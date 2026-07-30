// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(unused_doc_comments)]

/**
 * SECTION:element-multiudpsink2
 * @see_also: udpsink2, udpsrc2, multiudpsink.
 *
 * `multiudpsink2` is a network sink that sends UDP packets to multiple clients.
 * It can be combined with RTP payloaders to implement RTP streaming.
 *
 * The #GstMultiUdpSink2:clients property is a comma-separated list of
 * `host:port` pairs with the destinations. Every packet is sent to each of the
 * listed destinations. Hostnames are resolved when a destination is added, and
 * IPv6 addresses must be enclosed in brackets, for example `[::1]:5000`.
 *
 * When a destination/port pair is listed multiple times, the packet is sent
 * multiple times as well. This behaviour can be disabled with the
 * #GstMultiUdpSink2:send-duplicates property.
 *
 * The list of destinations can be managed at runtime with the `add`, `remove`,
 * `clear` and `get-stats` action signals. `add` and `remove` take a `host` and
 * a `port` and add or remove a destination, while `clear` removes all destinations.
 * The `get-stats` signal takes a `host` and a `port` and returns a structure with
 * the per-destination statistics:
 *
 * * #guint64 `bytes-sent`: the total number of bytes sent to the destination
 * * #guint64 `packets-sent`: the total number of packets sent to the destination
 * * #guint64 `connect-time`: the time in nanoseconds when the destination was added
 * * #guint64 `disconnect-time`: the time in nanoseconds when the destination was removed
 *
 * The `client-added` and `client-removed` signals are emitted whenever a
 * destination is added or removed and contain the `host` and `port` of the
 * affected destination.
 *
 * `multiudpsink2` can send to multicast groups by adding a multicast address
 * with the #GstMultiUdpSink2:clients property. It automatically joins and
 * leaves the multicast groups as destinations are added and removed, unless
 * disabled with the #GstBaseUdpSink2:auto-multicast property. The interface
 * used to join the group can be set with the #GstBaseUdpSink2:multicast-iface
 * property. The multicast TTL and loopback behaviour are controlled with the
 * #GstBaseUdpSink2:ttl-mc and #GstBaseUdpSink2:loop properties, and the
 * unicast TTL with the #GstBaseUdpSink2:ttl property.
 *
 * Alternatively one can provide a custom socket to `multiudpsink2` with the
 * #GstBaseUdpSink2:socket property. In that case `multiudpsink2` will not allocate a
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
 * properties report the total number of bytes sent to all clients and the
 * number of bytes received to serve to clients.
 *
 * ## Examples
 * |[
 * gst-launch-1.0 -v audiotestsrc ! multiudpsink2 clients=127.0.0.1:5000,127.0.0.1:5001
 * ]| Send audio to two UDP destinations.
 *
 * |[
 * gst-launch-1.0 -v audiotestsrc ! multiudpsink2 clients=239.255.0.1:5000
 * ]| Send audio to a multicast group.
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

use crate::baseudpsink::{BaseUdpSinkExt, BaseUdpSinkImpl, imp::BaseUdpSink};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "multiudpsink2",
        gst::DebugColorFlags::empty(),
        Some("Multi UDP Sink 2"),
    )
});

#[derive(Default)]
pub struct MultiUdpSink;

#[glib::object_subclass]
impl ObjectSubclass for MultiUdpSink {
    const NAME: &'static str = "GstMultiUdpSink2";
    type Type = super::MultiUdpSink;
    type ParentType = crate::baseudpsink::BaseUdpSink;
}

impl ObjectImpl for MultiUdpSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("send-duplicates")
                    .nick("Send duplicates")
                    .blurb("When a destination/port pair is added multiple times, send packets multiple times as well")
                    .default_value(true)
                    .build(),
                glib::ParamSpecString::builder("clients")
                    .nick("Clients")
                    .blurb("A comma-separated list of host:port pairs with destinations")
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "send-duplicates" => {
                let send_duplicates = value.get::<bool>().expect("type checked upstream");
                let elem = self.obj();
                elem.set_send_duplicates(send_duplicates);
            }
            "clients" => {
                let clients_str = value
                    .get::<Option<&str>>()
                    .expect("type checked upstream")
                    .unwrap_or("");
                let elem = self.obj();
                elem.set_clients(clients_str);
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "send-duplicates" => {
                let elem = self.obj();
                elem.send_duplicates().to_value()
            }
            "clients" => {
                let elem = self.obj();
                let clients = elem.clients();
                if clients.is_empty() {
                    None::<&str>.to_value()
                } else {
                    BaseUdpSink::format_clients(&clients).to_value()
                }
            }
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![
                glib::subclass::Signal::builder("client-added")
                    .param_types([String::static_type(), i32::static_type()])
                    .build(),
                glib::subclass::Signal::builder("client-removed")
                    .param_types([String::static_type(), i32::static_type()])
                    .build(),
                glib::subclass::Signal::builder("add")
                    .param_types([String::static_type(), i32::static_type()])
                    .action()
                    .class_handler(|args| {
                        let elem = args[0].get::<super::MultiUdpSink>().expect("signal arg");
                        let host = args[1].get::<String>().expect("signal arg");
                        let port = args[2].get::<i32>().expect("signal arg");

                        let Ok(port) = u16::try_from(port) else {
                            gst::error!(CAT, "Invalid port {port} for client '{host}'");
                            return None;
                        };
                        if elem.add_client(&host, port) {
                            elem.emit_by_name::<()>("client-added", &[&host, &(port as i32)]);
                        }

                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("remove")
                    .param_types([String::static_type(), i32::static_type()])
                    .action()
                    .class_handler(|args| {
                        let elem = args[0].get::<super::MultiUdpSink>().expect("signal arg");
                        let host = args[1].get::<String>().expect("signal arg");
                        let port = args[2].get::<i32>().expect("signal arg");

                        let Ok(port) = u16::try_from(port) else {
                            gst::error!(CAT, "Invalid port {port} for client '{host}'");
                            return None;
                        };
                        if elem.remove_client(&host, port) {
                            elem.emit_by_name::<()>("client-removed", &[&host, &(port as i32)]);
                        }

                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("clear")
                    .action()
                    .class_handler(|args| {
                        let elem = args[0].get::<super::MultiUdpSink>().expect("signal arg");

                        let clients = elem.clear_clients();

                        for client in clients.keys() {
                            elem.emit_by_name::<()>(
                                "client-removed",
                                &[&client.host.as_str(), &(client.port as i32)],
                            );
                        }

                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("get-stats")
                    .param_types([String::static_type(), i32::static_type()])
                    .action()
                    .return_type::<gst::Structure>()
                    .class_handler(|args| {
                        let elem = args[0].get::<super::MultiUdpSink>().expect("signal arg");
                        let host = args[1].get::<String>().expect("signal arg");
                        let port = args[2].get::<i32>().expect("signal arg");

                        let Ok(port) = u16::try_from(port) else {
                            gst::error!(CAT, "Invalid port {port} for client '{host}'");
                            return None;
                        };
                        let stats = elem.get_stats(&host, port).unwrap_or_default();
                        let structure = gst::Structure::builder("multiudpsink-stats")
                            .field("bytes-sent", stats.bytes_sent)
                            .field("packets-sent", stats.packets_sent)
                            .field("connect-time", stats.connect_time)
                            .field("disconnect-time", stats.disconnect_time)
                            .build();

                        Some(structure.to_value())
                    })
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }
}

impl GstObjectImpl for MultiUdpSink {}

impl ElementImpl for MultiUdpSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Multi UDP Sink",
                "Sink/Network",
                "Sends UDP packets to multiple destinations",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
}

impl BaseSinkImpl for MultiUdpSink {}

impl BaseUdpSinkImpl for MultiUdpSink {}
