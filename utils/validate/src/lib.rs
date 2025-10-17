#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

use std::sync::LazyLock;

use gst::glib;

mod check_last_frame_qrcode;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rsvalidate",
        gst::DebugColorFlags::empty(),
        Some("GStreamer Validate Rust Plugin"),
    )
});

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    if let Err(err) = check_last_frame_qrcode::register_validate_actions(plugin) {
        gst::warning!(
            gst::CAT_RUST,
            "Failed to register validate actions: {}",
            err
        );

        return Err(err);
    }

    Ok(())
}

gst::plugin_define!(
    rsvalidate,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
