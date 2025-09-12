// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::transceiver::Transceiver;

#[derive(Default)]
pub struct WebRTCSendSinkPad {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
pub struct State {
    mline: Option<usize>,
    transceiver: Option<Transceiver>,
    block_id: Option<gst::PadProbeId>,
}

impl State {
    pub fn transceiver(&self) -> &Transceiver {
        self.transceiver.as_ref().unwrap()
    }
    pub fn set_transceiver(&mut self, transceiver: Transceiver) {
        self.transceiver = Some(transceiver)
    }

    pub fn mline(&self) -> Option<usize> {
        self.mline
    }

    pub fn set_mline(&mut self, mline: Option<usize>) {
        self.mline = mline
    }
    pub fn set_block_id(&mut self, id: gst::PadProbeId) {
        self.block_id = Some(id);
    }
    pub fn take_block_id(&mut self) -> Option<gst::PadProbeId> {
        self.block_id.take()
    }
}

impl WebRTCSendSinkPad {
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WebRTCSendSinkPad {
    const NAME: &'static str = "GstWebRTCSendSinkPad";
    type Type = super::WebRTCSendSinkPad;
    type ParentType = gst::GhostPad;
}

impl ObjectImpl for WebRTCSendSinkPad {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecObject::builder::<Transceiver>("transceiver")
                    .nick("Transceiver")
                    .blurb("The transceiver in use for this sink pad")
                    .read_only()
                    .build(),
            ]
        });
        PROPS.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "transceiver" => self.state.lock().unwrap().transceiver.to_value(),
            _ => unreachable!(),
        }
    }
}

impl GstObjectImpl for WebRTCSendSinkPad {}

impl PadImpl for WebRTCSendSinkPad {}

impl ProxyPadImpl for WebRTCSendSinkPad {}

impl GhostPadImpl for WebRTCSendSinkPad {}
