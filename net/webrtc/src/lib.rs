/**
 * plugin-rswebrtc:
 *
 * Since: plugins-rs-0.9
 */
use gst::glib;

pub mod gcc;
mod signaller;
pub mod webrtcsink;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    webrtcsink::register(plugin)?;
    gcc::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    rswebrtc,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
