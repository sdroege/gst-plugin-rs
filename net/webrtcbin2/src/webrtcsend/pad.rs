// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use super::WebRTCSendSinkPadEarlyDataMode;
use crate::transceiver::Transceiver;

#[derive(Default)]
pub struct WebRTCSendSinkPad {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
pub struct State {
    mline: Option<usize>,
    transceiver: Option<Transceiver>,
    early_data_mode: WebRTCSendSinkPadEarlyDataMode,
    block_id: Option<gst::PadProbeId>,
    received_caps: Option<gst::Caps>,
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

    pub fn early_data_mode(&self) -> WebRTCSendSinkPadEarlyDataMode {
        self.early_data_mode
    }
    pub fn set_block_id(&mut self, id: gst::PadProbeId) {
        self.block_id = Some(id);
    }
    pub fn take_block_id(&mut self) -> Option<gst::PadProbeId> {
        self.block_id.take()
    }

    pub fn set_received_caps(&mut self, caps: gst::Caps) {
        self.received_caps = Some(caps);
    }
    pub fn received_caps(&self) -> Option<&gst::Caps> {
        self.received_caps.as_ref()
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
                glib::ParamSpecEnum::builder::<WebRTCSendSinkPadEarlyDataMode>("early-data-mode")
                    .nick("Early data mode")
                    .blurb(
                        "Controls how this sink pad deals with buffers while connection is pending",
                    )
                    .mutable_ready()
                    .build(),
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
            "early-data-mode" => self.state.lock().unwrap().early_data_mode.to_value(),
            "transceiver" => self.state.lock().unwrap().transceiver.to_value(),
            _ => unreachable!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "early-data-mode" => {
                self.state.lock().unwrap().early_data_mode =
                    value.get::<WebRTCSendSinkPadEarlyDataMode>().unwrap();
            }
            _ => unreachable!(),
        }
    }
}

impl GstObjectImpl for WebRTCSendSinkPad {}

impl PadImpl for WebRTCSendSinkPad {}

impl ProxyPadImpl for WebRTCSendSinkPad {}

impl GhostPadImpl for WebRTCSendSinkPad {}
