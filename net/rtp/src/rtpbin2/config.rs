// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use once_cell::sync::Lazy;
use std::sync::{Mutex, Weak};

use crate::rtpbin2::imp::BinSessionInner;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rtpbin2-config",
        gst::DebugColorFlags::empty(),
        Some("RtpBin2 config"),
    )
});

glib::wrapper! {
    pub struct RtpBin2Session(ObjectSubclass<imp::RtpBin2Session>);
}

impl RtpBin2Session {
    pub(crate) fn new(weak_session: Weak<Mutex<BinSessionInner>>) -> Self {
        let ret = glib::Object::new::<Self>();
        let imp = ret.imp();
        imp.set_session(weak_session);
        ret
    }
}

mod imp {
    use std::sync::Arc;

    use super::*;

    #[derive(Debug, Default)]
    struct State {
        pub(super) weak_session: Option<Weak<Mutex<BinSessionInner>>>,
    }

    #[derive(Debug, Default)]
    pub struct RtpBin2Session {
        state: Mutex<State>,
    }

    impl RtpBin2Session {
        pub(super) fn set_session(&self, weak_session: Weak<Mutex<BinSessionInner>>) {
            let mut state = self.state.lock().unwrap();
            state.weak_session = Some(weak_session);
        }

        fn session(&self) -> Option<Arc<Mutex<BinSessionInner>>> {
            self.state
                .lock()
                .unwrap()
                .weak_session
                .as_ref()
                .and_then(|sess| sess.upgrade())
        }

        pub fn set_pt_map(&self, pt_map: Option<gst::Structure>) {
            let Some(session) = self.session() else {
                return;
            };
            let mut session = session.lock().unwrap();
            session.clear_pt_map();
            let Some(pt_map) = pt_map else {
                return;
            };

            for (key, value) in pt_map.iter() {
                let Ok(pt) = key.parse::<u8>() else {
                    gst::warning!(CAT, "failed to parse key as a pt");
                    continue;
                };
                if let Ok(caps) = value.get::<gst::Caps>() {
                    session.add_caps(caps);
                } else {
                    gst::warning!(CAT, "{pt} does not contain a caps value");
                    continue;
                }
            }
        }

        pub fn pt_map(&self) -> gst::Structure {
            let mut ret = gst::Structure::builder("application/x-rtpbin2-pt-map");
            let Some(session) = self.session() else {
                return ret.build();
            };
            let session = session.lock().unwrap();

            for (pt, caps) in session.pt_map() {
                ret = ret.field(pt.to_string(), caps);
            }

            ret.build()
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RtpBin2Session {
        const NAME: &'static str = "GstRtpBin2Session";
        type Type = super::RtpBin2Session;
        type ParentType = glib::Object;
    }

    impl ObjectImpl for RtpBin2Session {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
                vec![glib::ParamSpecBoxed::builder::<gst::Structure>("pt-map")
                    .nick("RTP Payload Type Map")
                    .blurb("Mapping of RTP payload type to caps")
                    .build()]
            });

            PROPERTIES.as_ref()
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "pt-map" => self.pt_map().to_value(),
                _ => unreachable!(),
            }
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "pt-map" => self.set_pt_map(
                    value
                        .get::<Option<gst::Structure>>()
                        .expect("Type checked upstream"),
                ),
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_init;

    use super::*;

    #[test]
    fn pt_map_get_empty() {
        test_init();
        let rtpbin2 = gst::ElementFactory::make("rtpbin2").build().unwrap();
        let _pad = rtpbin2.request_pad_simple("rtp_send_sink_0").unwrap();
        let session = rtpbin2.emit_by_name::<gst::glib::Object>("get-session", &[&0u32]);
        let pt_map = session.property::<gst::Structure>("pt-map");
        assert!(pt_map.has_name("application/x-rtpbin2-pt-map"));
        assert_eq!(pt_map.fields().len(), 0);
    }

    #[test]
    fn pt_map_set() {
        test_init();
        let rtpbin2 = gst::ElementFactory::make("rtpbin2").build().unwrap();
        let _pad = rtpbin2.request_pad_simple("rtp_send_sink_0").unwrap();
        let session = rtpbin2.emit_by_name::<gst::glib::Object>("get-session", &[&0u32]);
        let pt = 96i32;
        let pt_caps = gst::Caps::builder("application/x-rtp")
            .field("payload", pt)
            .field("clock-rate", 90000i32)
            .build();
        let pt_map = gst::Structure::builder("application/x-rtpbin2-pt-map")
            .field(pt.to_string(), pt_caps.clone())
            .build();
        session.set_property("pt-map", pt_map);
        let prop = session.property::<gst::Structure>("pt-map");
        assert!(prop.has_name("application/x-rtpbin2-pt-map"));
        assert_eq!(prop.fields().len(), 1);
        let caps = prop.get::<gst::Caps>(pt.to_string()).unwrap();
        assert_eq!(pt_caps, caps);
    }

    #[test]
    fn pt_map_set_none() {
        test_init();
        let rtpbin2 = gst::ElementFactory::make("rtpbin2").build().unwrap();
        let _pad = rtpbin2.request_pad_simple("rtp_send_sink_0").unwrap();
        let session = rtpbin2.emit_by_name::<gst::glib::Object>("get-session", &[&0u32]);
        session.set_property("pt-map", None::<gst::Structure>);
        let prop = session.property::<gst::Structure>("pt-map");
        assert!(prop.has_name("application/x-rtpbin2-pt-map"));
    }
}
