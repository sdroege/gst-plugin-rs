use gst::glib;
use gst::glib::subclass::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;

#[derive(Copy, Clone)]
pub struct Signallable {
    _parent: glib::gobject_ffi::GTypeInterface,
    pub start: fn(&super::Signallable),
    pub stop: fn(&super::Signallable),
    pub send_sdp: fn(&super::Signallable, &str, &gst_webrtc::WebRTCSessionDescription),
    pub add_ice: fn(&super::Signallable, &str, &str, Option<u32>, Option<String>),
    pub end_session: fn(&super::Signallable, &str),
}

impl Signallable {
    fn request_meta(_iface: &super::Signallable) -> Option<gst::Structure> {
        None
    }
    fn start(_iface: &super::Signallable) {}
    fn stop(_iface: &super::Signallable) {}
    fn send_sdp(
        _iface: &super::Signallable,
        _session_id: &str,
        _sdp: &gst_webrtc::WebRTCSessionDescription,
    ) {
    }
    fn add_ice(
        _iface: &super::Signallable,
        _session_id: &str,
        _candidate: &str,
        _sdp_m_line_index: Option<u32>,
        _sdp_mid: Option<String>,
    ) {
    }
    fn end_session(_iface: &super::Signallable, _session_id: &str) {}
}

#[glib::object_interface]
unsafe impl prelude::ObjectInterface for Signallable {
    const NAME: &'static ::std::primitive::str = "GstRSWebRTCSignallableIface";
    type Prerequisites = (glib::Object,);

    fn interface_init(&mut self) {
        self.start = Signallable::start;
        self.stop = Signallable::stop;
        self.send_sdp = Signallable::send_sdp;
        self.add_ice = Signallable::add_ice;
        self.end_session = Signallable::end_session;
    }

    fn signals() -> &'static [Signal] {
        static SIGNALS: Lazy<Vec<Signal>> = Lazy::new(|| {
            vec![
                /**
                 * GstRSWebRTCSignallableIface::session-ended:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 * @session-id: The ID of the session that ended
                 *
                 * Some WebRTC Session was closed.
                 */
                Signal::builder("session-ended")
                    .param_types([str::static_type()])
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::producer-added:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 * @producer_id: The ID of the producer that was added
                 * @meta: The metadata structure of the producer
                 *
                 * Some new producing peer is ready to produce a WebRTC stream.
                 */
                Signal::builder("producer-added")
                    .param_types([str::static_type(), <Option<gst::Structure>>::static_type()])
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::producer-removed:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 * @producer_id: The ID of the producer that was added
                 * @meta: The metadata structure of the producer
                 *
                 * Some new producing peer is stopped producing streams.
                 */
                Signal::builder("producer-removed")
                    .param_types([str::static_type(), <Option<gst::Structure>>::static_type()])
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::session-started:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 *
                 * A new session started,
                 */
                Signal::builder("session-started")
                    .param_types([str::static_type(), str::static_type()])
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::session-requested:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 * @session_id: The ID of the producer that was added
                 * @peer_id: The ID of the consumer peer who wants to initiate a
                 *           session
                 */
                Signal::builder("session-requested")
                    .param_types([
                        str::static_type(),
                        str::static_type(),
                        gst_webrtc::WebRTCSessionDescription::static_type(),
                    ])
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::error:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 * @error: The error message as a string
                 */
                Signal::builder("error")
                    .param_types([str::static_type()])
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::request-meta:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 *
                 * The signaller requests a meta about the peer using it
                 *
                 * Return: The metadata about the peer represented by the signaller
                 */
                Signal::builder("request-meta")
                    .return_type::<Option<gst::Structure>>()
                    .class_handler(|_token, args| {
                        let arg0 = args[0usize]
                            .get::<&super::Signallable>()
                            .unwrap_or_else(|e| {
                                panic!("Wrong type for argument {}: {:?}", 0usize, e)
                            });
                        Some(Signallable::request_meta(arg0).to_value())
                    })
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::handle-ice:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 * @session_id: Id of the session the ice information is about
                 * @sdp_m_line_index: The mlineindex of the ice candidate
                 * @sdp_mid: Media ID of the ice candidate
                 * @candiate: Information about the candidate
                 */
                Signal::builder("handle-ice")
                    .param_types([
                        str::static_type(),
                        u32::static_type(),
                        <Option<String>>::static_type(),
                        str::static_type(),
                    ])
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::session-description:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 * @session_id: Id of the session being described
                 * @description: The WebRTC session description
                 */
                Signal::builder("session-description")
                    .param_types([
                        str::static_type(),
                        gst_webrtc::WebRTCSessionDescription::static_type(),
                    ])
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::start:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 *
                 * Starts the signaller, connecting it to the signalling server.
                 */
                Signal::builder("start")
                    .flags(glib::SignalFlags::ACTION)
                    .class_handler(|_token, args| {
                        let arg0 = args[0usize]
                            .get::<&super::Signallable>()
                            .unwrap_or_else(|e| {
                                panic!("Wrong type for argument {}: {:?}", 0usize, e)
                            });
                        Signallable::start(arg0);

                        None
                    })
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::stop:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 *
                 * Stops the signaller, disconnecting it to the signalling server.
                 */
                Signal::builder("stop")
                    .flags(glib::SignalFlags::ACTION)
                    .class_handler(|_tokens, args| {
                        let arg0 = args[0usize]
                            .get::<&super::Signallable>()
                            .unwrap_or_else(|e| {
                                panic!("Wrong type for argument {}: {:?}", 0usize, e)
                            });
                        Signallable::stop(arg0);

                        None
                    })
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::shutdown:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 */
                Signal::builder("shutdown").build(),
                /**
                 * GstRSWebRTCSignallableIface::consumer-added:
                 * @self: The object implementing #GstRSWebRTCSignallableIface
                 * @peer_id: Id of the consumer
                 * @webrtcbin: The internal WebRTCBin element
                 *
                 * This signal can be used to tweak @webrtcbin, creating a data
                 * channel for example.
                 */
                Signal::builder("consumer-added")
                    .param_types([String::static_type(), gst::Element::static_type()])
                    .build(),
                /**
                 * GstRSWebRTCSignallableIface::consumer-removed:
                 * @consumer_id: Identifier of the consumer that was removed
                 * @webrtcbin: The webrtcbin connected to the newly removed consumer
                 *
                 * This signal is emitted right after the connection with a consumer
                 * has been dropped.
                 */
                glib::subclass::Signal::builder("consumer-removed")
                    .param_types([String::static_type(), gst::Element::static_type()])
                    .build(),
            ]
        });
        SIGNALS.as_ref()
    }
}

unsafe impl<Obj: SignallableImpl> types::IsImplementable<Obj> for super::Signallable
where
    <Obj as types::ObjectSubclass>::Type: glib::IsA<glib::Object>,
{
    fn interface_init(iface: &mut glib::Interface<Self>) {
        let iface = ::std::convert::AsMut::as_mut(iface);

        fn vstart_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
            obj: &super::Signallable,
        ) {
            let this = obj
                .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                .unwrap()
                .imp();
            SignallableImpl::start(this)
        }
        iface.start = vstart_trampoline::<Obj>;

        fn vstop_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
            this: &super::Signallable,
        ) {
            let this = this
                .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                .unwrap();
            SignallableImpl::stop(this.imp())
        }
        iface.stop = vstop_trampoline::<Obj>;

        fn send_sdp_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
            this: &super::Signallable,
            session_id: &str,
            sdp: &gst_webrtc::WebRTCSessionDescription,
        ) {
            let this = this
                .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                .unwrap();
            SignallableImpl::send_sdp(this.imp(), session_id, sdp)
        }
        iface.send_sdp = send_sdp_trampoline::<Obj>;

        fn add_ice_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
            this: &super::Signallable,
            session_id: &str,
            candidate: &str,
            sdp_m_line_index: Option<u32>,
            sdp_mid: Option<String>,
        ) {
            let this = this
                .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                .unwrap();
            SignallableImpl::add_ice(this.imp(), session_id, candidate, sdp_m_line_index, sdp_mid)
        }
        iface.add_ice = add_ice_trampoline::<Obj>;

        fn end_session_trampoline<Obj: types::ObjectSubclass + SignallableImpl>(
            this: &super::Signallable,
            session_id: &str,
        ) {
            let this = this
                .dynamic_cast_ref::<<Obj as types::ObjectSubclass>::Type>()
                .unwrap();
            SignallableImpl::end_session(this.imp(), session_id)
        }
        iface.end_session = end_session_trampoline::<Obj>;
    }
}

pub trait SignallableImpl: object::ObjectImpl + 'static {
    fn start(&self) {}
    fn stop(&self) {}
    fn send_sdp(&self, _session_id: &str, _sdp: &gst_webrtc::WebRTCSessionDescription) {}
    fn add_ice(
        &self,
        _session_id: &str,
        _candidate: &str,
        _sdp_m_line_index: Option<u32>,
        _sdp_mid: Option<String>,
    ) {
    }
    fn end_session(&self, _session_id: &str) {}
}

pub trait SignallableExt: 'static {
    fn start(&self);
    fn stop(&self);
    fn send_sdp(&self, session_id: &str, sdp: &gst_webrtc::WebRTCSessionDescription);
    fn add_ice(
        &self,
        session_id: &str,
        candidate: &str,
        sdp_m_line_index: Option<u32>,
        sdp_mid: Option<String>,
    );
    fn end_session(&self, session_id: &str);
}

impl<Obj: glib::IsA<super::Signallable>> SignallableExt for Obj {
    fn start(&self) {
        let obj = self.upcast_ref::<super::Signallable>();
        let vtable = obj.interface::<super::Signallable>().unwrap();
        let vtable = vtable.as_ref();
        (vtable.start)(obj)
    }

    fn stop(&self) {
        let obj = self.upcast_ref::<super::Signallable>();
        let vtable = obj.interface::<super::Signallable>().unwrap();
        let vtable = vtable.as_ref();
        (vtable.stop)(obj)
    }

    fn send_sdp(&self, session_id: &str, sdp: &gst_webrtc::WebRTCSessionDescription) {
        let obj = self.upcast_ref::<super::Signallable>();
        let vtable = obj.interface::<super::Signallable>().unwrap();
        let vtable = vtable.as_ref();
        (vtable.send_sdp)(obj, session_id, sdp)
    }

    fn add_ice(
        &self,
        session_id: &str,
        candidate: &str,
        sdp_m_line_index: Option<u32>,
        sdp_mid: Option<String>,
    ) {
        let obj = self.upcast_ref::<super::Signallable>();
        let vtable = obj.interface::<super::Signallable>().unwrap();
        let vtable = vtable.as_ref();
        (vtable.add_ice)(obj, session_id, candidate, sdp_m_line_index, sdp_mid)
    }

    fn end_session(&self, session_id: &str) {
        let obj = self.upcast_ref::<super::Signallable>();
        let vtable = obj.interface::<super::Signallable>().unwrap();
        let vtable = vtable.as_ref();
        (vtable.end_session)(obj, session_id)
    }
}
