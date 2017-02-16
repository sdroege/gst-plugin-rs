// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::os::raw::c_void;

pub struct Plugin(*const c_void);

impl Plugin {
    pub unsafe fn new(plugin: *const c_void) -> Plugin {
        Plugin(plugin)
    }

    pub unsafe fn as_ptr(&self) -> *const c_void {
        self.0
    }
}

#[macro_export]
macro_rules! plugin_define(
    ($name:expr, $description:expr, $plugin_init:ident,
     $version:expr, $license:expr, $source:expr,
     $package:expr, $origin:expr, $release_datetime:expr) => {
        pub mod plugin_desc {
            use std::os::raw::c_void;
            use $crate::utils::GBoolean;
            use $crate::plugin::Plugin;

            #[repr(C)]
            pub struct GstPluginDesc {
                major_version: i32,
                minor_version: i32,
                name: *const u8,
                description: *const u8,
                plugin_init: unsafe extern "C" fn(plugin: *const c_void) -> GBoolean,
                version: *const u8,
                license: *const u8,
                source: *const u8,
                package: *const u8,
                origin: *const u8,
                release_datetime: *const u8,
                _gst_reserved: [usize; 4],
            }

            unsafe impl Sync for GstPluginDesc {}

            #[no_mangle]
            #[allow(non_upper_case_globals)]
            pub static gst_plugin_desc: GstPluginDesc = GstPluginDesc {
                major_version: 1,
                minor_version: 10,
                name: $name as *const u8,
                description: $description as *const u8,
                plugin_init: plugin_init_trampoline,
                version: $version as *const u8,
                license: $license as *const u8,
                source: $source as *const u8,
                package: $package as *const u8,
                origin: $origin as *const u8,
                release_datetime: $release_datetime as *const u8,
                _gst_reserved: [0, 0, 0, 0],
            };

            unsafe extern "C" fn plugin_init_trampoline(plugin: *const c_void) -> GBoolean {
                GBoolean::from_bool(super::$plugin_init(&Plugin::new(plugin)))
            }
        }
    };
);
