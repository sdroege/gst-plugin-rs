//  Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
//                2016 Luis de Bethencourt <luisbg@osg.samsung.com>
//
//  This library is free software; you can redistribute it and/or
//  modify it under the terms of the GNU Library General Public
//  License as published by the Free Software Foundation; either
//  version 2 of the License, or (at your option) any later version.
//
//  This library is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
//  Library General Public License for more details.
//
//  You should have received a copy of the GNU Library General Public
//  License along with this library; if not, write to the
//  Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
//  Boston, MA 02110-1301, USA.

use std::os::raw::c_void;

pub struct Plugin(*const c_void);

impl Plugin {
    pub unsafe fn new(plugin: *const c_void) -> Plugin {
        Plugin(plugin)
    }

    pub unsafe fn to_raw(&self) -> *const c_void {
        self.0
    }
}

macro_rules! plugin_define(
    ($name:expr, $description:expr, $plugin_init:ident,
     $version:expr, $license:expr, $source:expr,
     $package:expr, $origin:expr, $release_datetime:expr) => {
        pub mod plugin_desc {
            use std::os::raw::c_void;
            use utils::GBoolean;
            use plugin::Plugin;

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
