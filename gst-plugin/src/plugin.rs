// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst;

pub struct Plugin(*mut gst::GstPlugin);

impl Plugin {
    pub unsafe fn new(plugin: *mut gst::GstPlugin) -> Plugin {
        Plugin(plugin)
    }

    pub unsafe fn as_ptr(&self) -> *mut gst::GstPlugin {
        self.0
    }
}

#[macro_export]
macro_rules! plugin_define(
    ($name:expr, $description:expr, $plugin_init:ident,
     $version:expr, $license:expr, $source:expr,
     $package:expr, $origin:expr, $release_datetime:expr) => {
        pub mod plugin_desc {
            use $crate::plugin::Plugin;
            use $crate::ffi::gst;
            use $crate::ffi::glib;

            // Not using c_char here because it requires the libc crate
            #[allow(non_camel_case_types)]
            type c_char = i8;

            #[repr(C)]
            pub struct GstPluginDesc(gst::GstPluginDesc);
            unsafe impl Sync for GstPluginDesc {}

            #[no_mangle]
            #[allow(non_upper_case_globals)]
            pub static gst_plugin_desc: GstPluginDesc = GstPluginDesc(gst::GstPluginDesc {
                major_version: 1,
                minor_version: 10,
                name: $name as *const u8 as *const c_char,
                description: $description as *const u8 as *const c_char,
                plugin_init: Some(plugin_init_trampoline),
                version: $version as *const u8 as *const c_char,
                license: $license as *const u8 as *const c_char,
                source: $source as *const u8 as *const c_char,
                package: $package as *const u8 as *const c_char,
                origin: $origin as *const u8 as *const c_char,
                release_datetime: $release_datetime as *const u8 as *const c_char,
                _gst_reserved: [0 as glib::gpointer; 4],
            });

            unsafe extern "C" fn plugin_init_trampoline(plugin: *mut gst::GstPlugin) -> glib::gboolean {
                if super::$plugin_init(&Plugin::new(plugin)) {
                    glib::GTRUE
                } else {
                    glib::GFALSE
                }
            }
        }
    };
);
