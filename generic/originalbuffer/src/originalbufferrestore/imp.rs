// Copyright (C) 2024 Collabora Ltd
//   @author: Olivier Crête <olivier.crete@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::subclass::prelude::*;
use gst_video::prelude::*;

use atomic_refcell::AtomicRefCell;

use crate::originalbuffermeta;
use crate::originalbuffermeta::{OriginalBufferMeta, RecordedTransform};

#[derive(Default)]
struct State {
    /// Caps last pushed downstream (the original, pre-transform caps), used to
    /// only push a new `Caps` event when it actually changes.
    restored_caps: Option<gst::Caps>,
    sinkpad_segment: Option<gst::Event>,
}

pub struct OriginalBufferRestore {
    state: AtomicRefCell<State>,
    src_pad: gst::Pad,
    sink_pad: gst::Pad,
}

/// Inverts a 3x3 matrix, returning `None` if it is singular (not invertible).
fn invert_matrix3(m: &[[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let (a, b, c) = (m[0][0], m[0][1], m[0][2]);
    let (d, e, f) = (m[1][0], m[1][1], m[1][2]);
    let (g, h, i) = (m[2][0], m[2][1], m[2][2]);

    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if !det.is_finite() || det.abs() < f32::EPSILON {
        return None;
    }
    let inv_det = 1.0 / det;

    Some([
        [
            (e * i - f * h) * inv_det,
            (c * h - b * i) * inv_det,
            (b * f - c * e) * inv_det,
        ],
        [
            (f * g - d * i) * inv_det,
            (a * i - c * g) * inv_det,
            (c * d - a * f) * inv_det,
        ],
        [
            (d * h - e * g) * inv_det,
            (b * g - a * h) * inv_det,
            (a * e - b * d) * inv_det,
        ],
    ])
}

enum ReverseTransform {
    Matrix(gst_video::video_meta::VideoMetaTransformMatrix),
    Scale {
        matrix: gst_video::video_meta::VideoMetaTransformMatrix,
        scale: gst_video::video_meta::VideoMetaTransformScale,
    },
    SingularMatrix,
}

fn reverse_transform(transform: &RecordedTransform) -> ReverseTransform {
    match transform {
        RecordedTransform::Scale { in_info, out_info } => {
            // The buffer travelled in_info -> out_info; reverse it.
            let scale = gst_video::video_meta::VideoMetaTransformScale::new(out_info, in_info);

            let out_rect = gst_video::VideoRectangle::new(
                0,
                0,
                out_info.width() as i32,
                out_info.height() as i32,
            );
            let in_rect = gst_video::VideoRectangle::new(
                0,
                0,
                in_info.width() as i32,
                in_info.height() as i32,
            );
            // A full-frame matrix built from the (reversed) rectangles is
            // exactly the inverse scale, no explicit inversion needed.
            let matrix = gst_video::video_meta::VideoMetaTransformMatrix::new(
                out_info, &out_rect, in_info, &in_rect,
            );

            ReverseTransform::Scale { matrix, scale }
        }
        RecordedTransform::Matrix {
            in_info,
            in_rectangle,
            out_info,
            out_rectangle,
            matrix,
        } => match invert_matrix3(matrix) {
            Some(inverted) => {
                let mut m = gst_video::video_meta::VideoMetaTransformMatrix::new(
                    out_info,
                    out_rectangle,
                    in_info,
                    in_rectangle,
                );
                *m.matrix_mut() = inverted;
                ReverseTransform::Matrix(m)
            }
            None => ReverseTransform::SingularMatrix,
        },
    }
}

use std::sync::LazyLock;
#[allow(dead_code)]
static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "originalbufferrestore",
        gst::DebugColorFlags::empty(),
        Some("Restore Original buffer as meta"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for OriginalBufferRestore {
    const NAME: &'static str = "GstOriginalBufferRestore";
    type Type = super::OriginalBufferRestore;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let sink_templ = klass.pad_template("sink").unwrap();
        let src_templ = klass.pad_template("src").unwrap();

        let sink_pad = gst::Pad::builder_from_template(&sink_templ)
            .chain_function(|pad, parent, buffer| {
                OriginalBufferRestore::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |obj| obj.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                OriginalBufferRestore::catch_panic_pad_function(
                    parent,
                    || false,
                    |obj| obj.sink_event(pad, parent, event),
                )
            })
            .query_function(|pad, parent, query| {
                OriginalBufferRestore::catch_panic_pad_function(
                    parent,
                    || false,
                    |obj| obj.sink_query(pad, parent, query),
                )
            })
            .build();

        let src_pad = gst::Pad::builder_from_template(&src_templ)
            .event_function(|pad, parent, event| {
                OriginalBufferRestore::catch_panic_pad_function(
                    parent,
                    || false,
                    |obj| obj.src_event(pad, parent, event),
                )
            })
            .build();

        Self {
            src_pad,
            sink_pad,
            state: Default::default(),
        }
    }
}

impl ObjectImpl for OriginalBufferRestore {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sink_pad).unwrap();
        obj.add_pad(&self.src_pad).unwrap();
    }
}

impl GstObjectImpl for OriginalBufferRestore {}

impl ElementImpl for OriginalBufferRestore {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Original Buffer Restore",
                "Generic",
                "Restores a reference to the buffer in a meta",
                "Olivier Crête <olivier.crete@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_any();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let ret = self.parent_change_state(transition)?;
        if transition == gst::StateChange::PausedToReady {
            let mut state = self.state.borrow_mut();
            *state = State::default();
        }

        Ok(ret)
    }
}

impl OriginalBufferRestore {
    fn sink_event(
        &self,
        pad: &gst::Pad,
        parent: Option<&impl IsA<gst::Object>>,
        event: gst::Event,
    ) -> bool {
        match event.view() {
            gst::EventView::Caps(_) => {
                // The sink caps describe the transformed ("main") buffer
                // stream; they aren't forwarded downstream as-is since the
                // src pad's caps are the restored original caps, pushed from
                // `sink_chain`/the `Gap` handler below instead.
                true
            }
            gst::EventView::Segment(_) => {
                let mut state = self.state.borrow_mut();
                state.sinkpad_segment = Some(event);
                true
            }
            gst::EventView::Gap(e) => {
                let s = e.structure().unwrap();

                if let Ok(buffer) = s.get::<gst::Buffer>("original-buffer") {
                    if let Ok(caps) = s.get::<gst::Caps>("original-caps") {
                        let mut state = self.state.borrow_mut();
                        if state.restored_caps.as_ref() != Some(&caps) {
                            state.restored_caps = Some(caps.clone());
                            drop(state);
                            if !self.src_pad.push_event(gst::event::Caps::new(&caps)) {
                                return false;
                            }
                        }
                    }

                    if !self.push_pending_segment() {
                        return false;
                    }

                    return self.src_pad.push(buffer).is_ok();
                }
                if let Ok(event) = s.get::<gst::Event>("original-gap") {
                    if !self.push_pending_segment() {
                        return false;
                    }
                    return self.src_pad.push_event(event);
                }
                gst::Pad::event_default(pad, parent, event)
            }
            _ => gst::Pad::event_default(pad, parent, event),
        }
    }

    fn push_pending_segment(&self) -> bool {
        let segment_event = self.state.borrow_mut().sinkpad_segment.take();
        match segment_event {
            Some(event) => self.src_pad.push_event(event),
            None => true,
        }
    }

    fn src_event(
        &self,
        pad: &gst::Pad,
        parent: Option<&impl IsA<gst::Object>>,
        event: gst::Event,
    ) -> bool {
        if event.type_() == gst::EventType::Reconfigure
            || event.has_name("gst-original-buffer-forward-upstream-event")
        {
            let s = gst::Structure::builder("gst-original-buffer-forward-upstream-event")
                .field("event", event)
                .build();
            let event = gst::event::CustomUpstream::new(s);
            self.sink_pad.push_event(event)
        } else {
            gst::Pad::event_default(pad, parent, event)
        }
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        parent: Option<&impl IsA<gst::Object>>,
        query: &mut gst::QueryRef,
    ) -> bool {
        if let gst::QueryViewMut::Custom(_) = query.view_mut() {
            let s = query.structure_mut();
            if s.has_name("gst-original-buffer-forward-query")
                && let Ok(mut q) = s.get::<gst::Query>("query")
            {
                s.remove_field("query");
                assert!(q.is_writable());
                let res = self.src_pad.peer_query(q.get_mut().unwrap());

                s.set("query", q);
                s.set("result", res);

                return true;
            }
        }

        gst::Pad::query_default(pad, parent, query)
    }

    /// Transfers metadata from `src` onto `dst`, applying the inverse of
    /// `transform` (one recorded transform) to reposition it, or a plain
    /// copy if `transform` is `None` (no recorded transform to invert) or
    /// the meta doesn't support the attempted transform. The reverse
    /// representation of `transform` is only computed (via
    /// `reverse_transform`) the first time it's actually needed, i.e. when a
    /// meta reaches the point of being repositioned, and is then reused for
    /// the rest of this call.
    fn transfer_metas(
        &self,
        src: &gst::BufferRef,
        dst: &mut gst::BufferRef,
        transform: Option<&RecordedTransform>,
    ) {
        let mut reverse: Option<ReverseTransform> = None;

        gst::trace!(
            CAT,
            imp = self,
            "Reversing recorded transform: {:?}",
            transform
        );

        for meta in src.iter_meta::<gst::Meta>() {
            if meta.api() == originalbuffermeta::OriginalBufferMeta::meta_api() {
                continue;
            }

            if meta.has_tag::<gst::meta::tags::Memory>()
                || meta.has_tag::<gst::meta::tags::MemoryReference>()
            {
                continue;
            }

            // Only pay for the matrix inversion / VideoMetaTransform
            // construction once we actually have a meta that needs
            // repositioning.
            if let Some(transform) = transform
                && reverse.is_none()
            {
                reverse = Some(reverse_transform(transform));
            }

            if matches!(reverse, Some(ReverseTransform::SingularMatrix))
                && meta.has_tag::<gst_video::video_meta::tags::Size>()
            {
                // The recorded matrix for this step can't be inverted: this
                // meta's position can't be known, so drop it rather than
                // keep it in the wrong place.
                gst::warning!(
                    CAT,
                    imp = self,
                    "Dropping meta {} that could not be repositioned: non-invertible \
                     transform recorded ({:?})",
                    meta.api(),
                    transform
                );
                continue;
            }

            let transformed = match &reverse {
                None | Some(ReverseTransform::SingularMatrix) => false,
                Some(ReverseTransform::Matrix(m)) => meta.transform(dst, m).is_ok(),
                Some(ReverseTransform::Scale { matrix, scale }) => {
                    meta.transform(dst, matrix).is_ok() || meta.transform(dst, scale).is_ok()
                }
            };

            gst::trace!(
                CAT,
                imp = self,
                "Applied reverse of transform to meta {} with result {}",
                meta.api(),
                transformed
            );

            if !transformed {
                if reverse.is_some() && meta.has_tag::<gst_video::video_meta::tags::Size>() {
                    // A recorded geometric transform explicitly rejected
                    // this spatial metadata (e.g. a ROI that ended up
                    // entirely outside the valid/cropped region): don't
                    // fall back to a plain copy, which would keep it at a
                    // stale, now-incorrect position.
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Dropping meta {} that was rejected by the recorded transform \
                         ({:?})",
                        meta.api(),
                        transform
                    );
                    continue;
                }
                let _ = meta.transform(dst, &gst::meta::MetaTransformCopy::new(..));
            }
        }
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        inbuf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(ometa) = inbuf.meta::<OriginalBufferMeta>() else {
            //gst::element_warning!(self, gst::StreamError::Failed, ["Buffer {} is missing the GstOriginalBufferMeta, put originalbuffersave upstream in your pipeline", buffer]);
            return Ok(gst::FlowSuccess::Ok);
        };

        let original = ometa.original().clone();
        let caps = ometa.caps().clone();
        let transforms: Vec<RecordedTransform> = ometa.transforms().to_vec();

        {
            let mut state = self.state.borrow_mut();
            if state.restored_caps.as_ref() != Some(&caps) {
                if !self.src_pad.push_event(gst::event::Caps::new(&caps)) {
                    gst::info!(CAT, imp = self, "Failed to push caps event");
                }
                state.restored_caps = Some(caps);
            }
        }

        let mut outbuf = original.copy();

        // Ping-pong the downstream metadata back through each recorded
        // transform, inverted and applied in reverse order, so it lands in
        // the original buffer's coordinate space. Each step needs a distinct
        // source and destination buffer (e.g. GstAnalyticsRelationMeta
        // forbids transforming into itself), so all but the last step use
        // small placeholder buffers that only exist to carry metadata
        // between steps; the last step writes into the restored original
        // buffer copy (which may already carry its own, pre-save, metadata).
        if transforms.is_empty() {
            self.transfer_metas(&inbuf, outbuf.make_mut(), None);
        } else {
            let mut current = inbuf.clone();
            for (i, transform) in transforms.iter().enumerate().rev() {
                if i == 0 {
                    self.transfer_metas(&current, outbuf.make_mut(), Some(transform));
                } else {
                    let mut next = gst::Buffer::new();
                    self.transfer_metas(&current, next.make_mut(), Some(transform));
                    current = next;
                }
            }
        }

        inbuf
            .copy_into(
                outbuf.make_mut(),
                gst::BufferCopyFlags::TIMESTAMPS | gst::BufferCopyFlags::FLAGS,
                ..,
            )
            .unwrap();

        if !self.push_pending_segment() {
            return Err(gst::FlowError::Error);
        }

        self.src_pad.push(outbuf)
    }
}
