use gst::glib;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use std::sync::LazyLock;

use super::STAMP;

pub static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtcteststamper",
        gst::DebugColorFlags::empty(),
        Some("WebRTC Test Stamper"),
    )
});

#[derive(Default)]
pub struct Stamper;

impl BaseTransformImpl for Stamper {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let inbuf = inbuf.map_readable().unwrap();

        outbuf.copy_from_slice(0, &inbuf[0..]).unwrap();
        outbuf.copy_from_slice(inbuf.size(), &[STAMP]).unwrap();

        Ok(gst::FlowSuccess::Ok)
    }

    fn transform_size(
        &self,
        direction: gst::PadDirection,
        _caps: &gst::Caps,
        size: usize,
        _othercaps: &gst::Caps,
    ) -> Option<usize> {
        assert_ne!(direction, gst::PadDirection::Src);

        // Reserve size for the stamp
        Some(size + 1)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Stamper {
    const NAME: &'static str = "Stamper";
    type Type = super::Stamper;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for Stamper {}

impl GstObjectImpl for Stamper {}

impl ElementImpl for Stamper {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Frame stamper",
                "Test/Stamper",
                "Append a stamp to incoming frames",
                "François Laignel <francois@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_any();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

#[derive(Default)]
pub struct StampChecker;

impl BaseTransformImpl for StampChecker {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let inbuf = inbuf.map_readable().unwrap();

        let last_byte = inbuf
            .size()
            .checked_sub(1)
            .expect("frame must contain at least 1 byte");

        let stamp = inbuf[last_byte];
        if stamp != STAMP {
            gst::error!(
                CAT,
                imp = self,
                "Unexpected stamp {stamp:?}, reference: {STAMP:?}"
            );
            return Err(gst::FlowError::Error);
        }

        // Remove the stamp
        outbuf.copy_from_slice(0, &inbuf[0..last_byte]).unwrap();

        Ok(gst::FlowSuccess::Ok)
    }

    fn transform_size(
        &self,
        direction: gst::PadDirection,
        _caps: &gst::Caps,
        size: usize,
        _othercaps: &gst::Caps,
    ) -> Option<usize> {
        assert_ne!(direction, gst::PadDirection::Src);

        // Outgoing frame will have the stamp removed
        size.checked_sub(1)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for StampChecker {
    const NAME: &'static str = "StampChecker";
    type Type = super::StampChecker;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for StampChecker {}

impl GstObjectImpl for StampChecker {}

impl ElementImpl for StampChecker {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Frame stamp checker",
                "Test/StampChecker",
                "Check and remove the stamp from incoming frames",
                "François Laignel <francois@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_any();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}
