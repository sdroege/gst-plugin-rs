// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::sync::Arc;

use gst::{glib, prelude::*, subclass::prelude::*};
use gst_base::subclass::prelude::*;
use indexmap::IndexMap;

pub mod imp;

glib::wrapper! {
    pub struct BaseUdpSink(ObjectSubclass<imp::BaseUdpSink>)
        @extends gst_base::BaseSink, gst::Element, gst::Object;
}

/// Trait containing extension methods for `BaseUdpSink`.
pub trait BaseUdpSinkExt: IsA<BaseUdpSink> + 'static {
    /// Add a client destination. Resolves the hostname internally.
    /// Returns whether the client was added successfully.
    fn add_client(&self, host: &str, port: u16) -> bool;

    /// Remove a client destination. Matches by the original hostname string and port.
    /// Returns whether any client was removed.
    fn remove_client(&self, host: &str, port: u16) -> bool;

    /// Clear all clients and return the removed ones.
    fn clear_clients(&self) -> Arc<IndexMap<imp::Client, imp::ClientEntry>>;

    /// Get the current list of clients.
    fn clients(&self) -> Arc<IndexMap<imp::Client, imp::ClientEntry>>;

    /// Replace the current list of clients from a comma-separated "host:port" string.
    fn set_clients(&self, clients: &str);

    /// Returns per-client statistics for the given host:port, if it exists.
    /// Matches by the original hostname string and port.
    fn get_stats(&self, host: &str, port: u16) -> Option<imp::ClientStats>;

    /// Returns the first client entry, if any.
    fn get_first_client(&self) -> Option<(imp::Client, imp::ClientEntry)>;

    /// Replaces the client list with a single client. Resolves the hostname
    /// internally and only updates if the client actually changed.
    fn set_client(&self, host: &str, port: u16);

    /// Whether packets are sent multiple times when a destination/port pair
    /// is added multiple times.
    fn send_duplicates(&self) -> bool;

    /// Set whether packets are sent multiple times when a destination/port
    /// pair is added multiple times.
    fn set_send_duplicates(&self, send_duplicates: bool);
}

impl<O: IsA<BaseUdpSink>> BaseUdpSinkExt for O {
    fn add_client(&self, host: &str, port: u16) -> bool {
        self.as_ref().imp().add_client(host, port)
    }

    fn remove_client(&self, host: &str, port: u16) -> bool {
        self.as_ref().imp().remove_client(host, port)
    }

    fn clear_clients(&self) -> Arc<IndexMap<imp::Client, imp::ClientEntry>> {
        self.as_ref().imp().clear_clients()
    }

    fn clients(&self) -> Arc<IndexMap<imp::Client, imp::ClientEntry>> {
        self.as_ref().imp().clients()
    }

    fn set_clients(&self, clients: &str) {
        self.as_ref().imp().set_clients(clients);
    }

    fn get_stats(&self, host: &str, port: u16) -> Option<imp::ClientStats> {
        self.as_ref().imp().get_stats(host, port)
    }

    fn get_first_client(&self) -> Option<(imp::Client, imp::ClientEntry)> {
        self.as_ref().imp().get_first_client()
    }

    fn set_client(&self, host: &str, port: u16) {
        self.as_ref().imp().set_client(host, port);
    }

    fn send_duplicates(&self) -> bool {
        self.as_ref().imp().send_duplicates()
    }

    fn set_send_duplicates(&self, send_duplicates: bool) {
        self.as_ref().imp().set_send_duplicates(send_duplicates);
    }
}

/// Trait to implement in `BaseUdpSink` subclasses.
pub trait BaseUdpSinkImpl: BaseSinkImpl + ObjectSubclass<Type: IsA<BaseUdpSink>> {}

/// Class struct for `BaseUdpSink`.
#[repr(C)]
pub struct Class {
    parent: gst_base::ffi::GstBaseSinkClass,
}

unsafe impl ClassStruct for Class {
    type Type = imp::BaseUdpSink;
}

impl std::ops::Deref for Class {
    type Target = glib::Class<<<Self as ClassStruct>::Type as ObjectSubclass>::ParentType>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(&self.parent as *const _ as *const _) }
    }
}

unsafe impl<T: BaseUdpSinkImpl> IsSubclassable<T> for BaseUdpSink {
    fn class_init(class: &mut glib::Class<Self>) {
        Self::parent_class_init::<T>(class);
    }
}
