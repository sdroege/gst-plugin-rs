// FIXME: add lgpl 2.1 license

#![crate_type = "cdylib"]

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate glib;
#[macro_use]
extern crate gstreamer as gst;

pub mod packet;

pub mod aws_transcribe_parse;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    aws_transcribe_parse::register(plugin)
}

gst::gst_plugin_define!(
    transcribe,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "LGPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
