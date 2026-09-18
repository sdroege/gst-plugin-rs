// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};

use crate::listener::{ListenerHandle, Session, SessionEvent, SessionKey};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "sapdeviceprovider",
        gst::DebugColorFlags::empty(),
        Some("SAP Device Provider"),
    )
});

#[derive(Debug, Default)]
struct State {
    listener: Option<ListenerHandle>,
    started: bool,
    devices: HashMap<SessionKey, super::Device>,
}

#[derive(Debug, Default)]
pub struct DeviceProvider {
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for DeviceProvider {
    const NAME: &'static str = "GstSapDeviceProvider";
    type Type = super::DeviceProvider;
    type ParentType = gst::DeviceProvider;
}

impl ObjectImpl for DeviceProvider {}

impl GstObjectImpl for DeviceProvider {}

impl DeviceProviderImpl for DeviceProvider {
    fn metadata() -> Option<&'static gst::subclass::DeviceProviderMetadata> {
        static METADATA: LazyLock<gst::subclass::DeviceProviderMetadata> = LazyLock::new(|| {
            gst::subclass::DeviceProviderMetadata::new(
                "SAP Device Provider",
                "Source/Audio/Video/Network",
                "Discovers and provides SAP sessions as devices",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*METADATA)
    }

    fn probe(&self) -> Vec<gst::Device> {
        self.state
            .lock()
            .unwrap()
            .devices
            .values()
            .map(|d| d.clone().upcast())
            .collect()
    }

    fn start(&self) -> Result<(), gst::LoggableError> {
        let listener =
            ListenerHandle::acquire().map_err(|err| gst::loggable_error!(CAT, "{err}"))?;

        // Drop a listener left over from a start() without a matching stop():
        // its callbacks are still registered and would keep adding devices
        drop(self.take_listener());

        self.state.lock().unwrap().started = true;

        // listen() replays all currently known sessions into the callback
        // synchronously, and the callback locks the state: it must not be
        // called while holding the state mutex.
        listener.listen(glib::clone!(
            #[weak(rename_to = this)]
            self,
            move |session, event| {
                this.session_event(session, event);
            }
        ));

        self.state.lock().unwrap().listener = Some(listener);

        gst::debug!(CAT, imp = self, "Started SAP device provider");

        Ok(())
    }

    fn stop(&self) {
        drop(self.take_listener());

        gst::debug!(CAT, imp = self, "Stopped SAP device provider");
    }
}

impl DeviceProvider {
    /// Takes the listener out of the state and clears all devices.
    ///
    /// Dropping the handle may join the listener thread, whose callbacks lock
    /// the state, so the caller must drop it without holding the mutex.
    fn take_listener(&self) -> Option<ListenerHandle> {
        let mut state = self.state.lock().unwrap();
        state.started = false;
        state.devices.clear();
        state.listener.take()
    }

    fn session_event(&self, session: &Session, event: SessionEvent) {
        enum Action {
            None,
            Add(super::Device),
            Change(super::Device, super::Device),
            Remove(super::Device),
        }

        let key = session.key();
        let action = {
            let mut state = self.state.lock().unwrap();

            // Ignore callbacks still in flight from before stop()
            if !state.started {
                return;
            }

            match event {
                SessionEvent::Added | SessionEvent::Updated => {
                    if let Some(existing) = state.devices.get(&key).cloned() {
                        if event == SessionEvent::Updated {
                            // Applications get notified via the changed
                            // signal
                            gst::debug!(
                                CAT,
                                imp = self,
                                "Updating SAP session {} ({}) device",
                                session.sdp.origin().sess_id,
                                session.sdp.name()
                            );
                            let device = super::Device::new(session);
                            state.devices.insert(key, device.clone());
                            Action::Change(existing, device)
                        } else {
                            gst::debug!(
                                CAT,
                                imp = self,
                                "Ignoring already known SAP session {} ({})",
                                session.sdp.origin().sess_id,
                                session.sdp.name()
                            );
                            Action::None
                        }
                    } else {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "Adding SAP session {} ({}) as device",
                            session.sdp.origin().sess_id,
                            session.sdp.name()
                        );
                        let device = super::Device::new(session);
                        state.devices.insert(key, device.clone());
                        Action::Add(device)
                    }
                }
                SessionEvent::Removed | SessionEvent::Expired => {
                    gst::debug!(CAT, imp = self, "Removing SAP session device {key:?}");
                    match state.devices.remove(&key) {
                        Some(device) => Action::Remove(device),
                        None => Action::None,
                    }
                }
            }
        };

        // Do not hold the state mutex while posting the bus messages
        let obj = self.obj();
        match action {
            Action::None => (),
            Action::Add(device) => obj.device_add(&device),
            Action::Change(old_device, new_device) => obj.device_changed(&old_device, &new_device),
            Action::Remove(device) => obj.device_remove(&device),
        }
    }
}

#[derive(Debug)]
pub struct Device {
    key: OnceLock<SessionKey>,
}

#[glib::object_subclass]
impl ObjectSubclass for Device {
    const NAME: &'static str = "GstSapDevice";
    type Type = super::Device;
    type ParentType = gst::Device;

    fn new() -> Self {
        Self {
            key: OnceLock::new(),
        }
    }
}

impl ObjectImpl for Device {}

impl GstObjectImpl for Device {}

impl DeviceImpl for Device {
    fn create_element(&self, name: Option<&str>) -> Result<gst::Element, gst::LoggableError> {
        let key = self
            .key
            .get()
            .expect("device must be created via Device::new()");

        let element = gst::Object::builder::<crate::sapsrc::SapSrc>()
            .name_if_some(name)
            .property("session-id", &key.sess_id)
            .property("originating-source", key.source.to_string())
            .build()
            .map_err(|err| gst::loggable_error!(CAT, "{err}"))?
            .upcast::<gst::Element>();

        Ok(element)
    }

    fn reconfigure_element(&self, element: &gst::Element) -> Result<(), gst::LoggableError> {
        let key = self
            .key
            .get()
            .expect("device must be created via Device::new()");

        element.set_property("session-id", &key.sess_id);
        element.set_property("originating-source", key.source.to_string());

        Ok(())
    }
}

impl super::Device {
    fn new(session: &Session) -> super::Device {
        let display_name = session.sdp.name();
        let device_class = "Source/Audio/Video/Network";

        let extra_properties = gst::Structure::builder("properties")
            .field("session-id", &session.sdp.origin().sess_id)
            .field("originating-source", session.originating_source.to_string())
            .build();

        let device = gst::Object::builder::<super::Device>()
            .property("display-name", display_name)
            .property("device-class", device_class)
            .property("properties", extra_properties)
            .build()
            .expect("Failed to build SAP device");

        device
            .imp()
            .key
            .set(session.key())
            .expect("session key must not be set already");

        device
    }
}
