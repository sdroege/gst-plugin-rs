// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#[macro_export]
macro_rules! plugin_define(
    ($name:expr, $description:expr, $plugin_init:ident,
     $version:expr, $license:expr, $source:expr,
     $package:expr, $origin:expr, $release_datetime:expr) => {
        pub mod plugin_desc {
            use $crate::glib::translate::{from_glib_borrow, ToGlib};

            // Not using c_char here because it requires the libc crate
            #[allow(non_camel_case_types)]
            type c_char = i8;

            #[repr(C)]
            pub struct GstPluginDesc($crate::gst_ffi::GstPluginDesc);
            unsafe impl Sync for GstPluginDesc {}

            #[no_mangle]
            #[allow(non_upper_case_globals)]
            pub static gst_plugin_desc: GstPluginDesc = GstPluginDesc($crate::gst_ffi::GstPluginDesc {
                major_version: 1,
                minor_version: 8,
                name: $name as *const u8 as *const c_char,
                description: $description as *const u8 as *const c_char,
                plugin_init: Some(plugin_init_trampoline),
                version: $version as *const u8 as *const c_char,
                license: $license as *const u8 as *const c_char,
                source: $source as *const u8 as *const c_char,
                package: $package as *const u8 as *const c_char,
                origin: $origin as *const u8 as *const c_char,
                release_datetime: $release_datetime as *const u8 as *const c_char,
                _gst_reserved: [0 as $crate::glib_ffi::gpointer; 4],
            });

            unsafe extern "C" fn plugin_init_trampoline(plugin: *mut $crate::gst_ffi::GstPlugin) -> $crate::glib_ffi::gboolean {
                use std::panic::{self, AssertUnwindSafe};

                let result = panic::catch_unwind(AssertUnwindSafe(|| super::$plugin_init(&from_glib_borrow(plugin)).to_glib()));
                match result {
                    Ok(result) => result,
                    Err(err) => {
                        let cat = $crate::gst::DebugCategory::get("GST_PLUGIN_LOADING").unwrap();
                        if let Some(cause) = err.downcast_ref::<&str>() {
                            gst_error!(cat, "Failed to initialize plugin due to panic: {}", cause);
                        } else if let Some(cause) = err.downcast_ref::<String>() {
                            gst_error!(cat, "Failed to initialize plugin due to panic: {}", cause);
                        } else {
                            gst_error!(cat, "Failed to initialize plugin due to panic");
                        }

                        $crate::glib_ffi::GFALSE
                    }
                }
            }
        }
    };
);
