// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use glib;
use glib::prelude::*;
use gst;
use gst::prelude::*;

use gst_plugin::properties::*;
use gst_plugin::object::*;
use gst_plugin::element::*;
use gst_plugin::bytes::*;

use std::sync::{Arc, Mutex};
use std::{cmp, mem, i32};
use std::io::Write;

struct State {
    dummy: i32,
}

impl Default for State {
    fn default() -> State {
        State { dummy: 0 }
    }
}

struct UdpSrc {
    cat: gst::DebugCategory,
    src_pad: gst::Pad,
    state: Mutex<State>,
}

impl UdpSrc {
    fn class_init(klass: &mut ElementClass) {
        klass.set_metadata(
            "Thread-sharing UDP source",
            "Source/Network",
            "Receives data over the network via UDP",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        );
        klass.add_pad_template(src_pad_template);
    }

    fn init(element: &Element) -> Box<ElementImpl<Element>> {
        let templ = element.get_pad_template("src").unwrap();
        let src_pad = gst::Pad::new_from_template(&templ, "src");

        src_pad.set_event_function(|pad, parent, event| {
            UdpSrc::catch_panic_pad_function(
                parent,
                || false,
                |udpsrc, element| udpsrc.src_event(pad, element, event),
            )
        });
        src_pad.set_query_function(|pad, parent, query| {
            UdpSrc::catch_panic_pad_function(
                parent,
                || false,
                |udpsrc, element| udpsrc.src_query(pad, element, query),
            )
        });
        element.add_pad(&src_pad).unwrap();

        Box::new(Self {
            cat: gst::DebugCategory::new(
                "ts-udpsrc",
                gst::DebugColorFlags::empty(),
                "Thread-sharing UDP source",
            ),
            state: Mutex::new(State::default()),
            src_pad: src_pad,
        })
    }

    fn catch_panic_pad_function<T, F: FnOnce(&Self, &Element) -> T, G: FnOnce() -> T>(
        parent: &Option<gst::Object>,
        fallback: G,
        f: F,
    ) -> T {
        let element = parent
            .as_ref()
            .cloned()
            .unwrap()
            .downcast::<Element>()
            .unwrap();
        let udpsrc = element.get_impl().downcast_ref::<UdpSrc>().unwrap();
        element.catch_panic(fallback, |element| f(udpsrc, element))
    }

    fn src_event(&self, pad: &gst::Pad, element: &Element, mut event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(self.cat, obj: pad, "Handling event {:?}", event);

        let mut handled = true;
        match event.view() {
            EventView::FlushStart(..) => {}
            EventView::FlushStop(..) => {}
            _ => (),
        }

        if handled {
            gst_log!(self.cat, obj: pad, "Handled event {:?}", event);
            pad.event_default(Some(element), event)
        } else {
            gst_log!(self.cat, obj: pad, "Didn't handle event {:?}", event);
            false
        }
    }

    fn src_query(&self, pad: &gst::Pad, element: &Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(self.cat, obj: pad, "Handling query {:?}", query);
        match query.view_mut() {
            _ => (),
        };

        gst_log!(self.cat, obj: pad, "Forwarding query {:?}", query);
        pad.query_default(Some(element), query)
    }
}

impl ObjectImpl<Element> for UdpSrc {}

impl ElementImpl<Element> for UdpSrc {
    fn change_state(
        &self,
        element: &Element,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        gst_trace!(self.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {}
            _ => (),
        }

        let mut ret = element.parent_change_state(transition);
        if ret == gst::StateChangeReturn::Failure {
            return ret;
        }

        match transition {
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                *state = Default::default();
            }
            gst::StateChange::ReadyToPaused => {
                ret = gst::StateChangeReturn::NoPreroll;
            }
            _ => (),
        }

        ret
    }
}

struct UdpSrcStatic;

impl ImplTypeStatic<Element> for UdpSrcStatic {
    fn get_name(&self) -> &str {
        "UdpSrc"
    }

    fn new(&self, element: &Element) -> Box<ElementImpl<Element>> {
        UdpSrc::init(element)
    }

    fn class_init(&self, klass: &mut ElementClass) {
        UdpSrc::class_init(klass);
    }
}

pub fn register(plugin: &gst::Plugin) {
    let udpsrc_static = UdpSrcStatic;
    let type_ = register_type(udpsrc_static);
    gst::Element::register(plugin, "ts-udpsrc", 0, type_);
}
