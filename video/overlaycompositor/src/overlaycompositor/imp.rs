// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use atomic_refcell::AtomicRefCell;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::prelude::*;
use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "overlaycompositor",
        gst::DebugColorFlags::empty(),
        Some("Overlay compositor"),
    )
});

#[derive(Default)]
struct State {
    caps: Option<gst::Caps>,
    output_caps: Option<gst::Caps>,
    info: Option<gst_video::VideoInfo>,
    system_memory: bool,
    downstream_meta: bool,
}

pub struct OverlayCompositor {
    sinkpad: gst::Pad,
    srcpad: gst::Pad,
    state: AtomicRefCell<State>,
}

fn has_overlay_feature(caps: &gst::CapsRef) -> bool {
    caps.features(0).is_some_and(|features| {
        features.contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION)
    })
}

fn with_overlay_feature(caps: &gst::CapsRef, enabled: bool) -> gst::Caps {
    let mut caps = caps.to_owned();
    for (_, features) in caps.get_mut().unwrap().iter_with_features_mut() {
        if !features.is_any() {
            if enabled {
                features.add(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
            } else {
                features.remove(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
            }
        }
    }
    caps
}

#[glib::object_subclass]
impl ObjectSubclass for OverlayCompositor {
    const NAME: &'static str = "GstOverlayCompositor";
    type Type = super::OverlayCompositor;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Self::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.sink_event(pad, event))
            })
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.query(pad, query))
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.query(pad, query))
            })
            .build();

        Self {
            sinkpad,
            srcpad,
            state: AtomicRefCell::new(State::default()),
        }
    }
}

impl ObjectImpl for OverlayCompositor {
    fn constructed(&self) {
        self.parent_constructed();

        self.obj().add_pad(&self.sinkpad).unwrap();
        self.obj().add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for OverlayCompositor {}

impl ElementImpl for OverlayCompositor {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Overlay compositor",
                "Filter/Editor/Video",
                "Blends overlay composition metadata into video frames",
                "Seungha Yang <seungha@centricular.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoFormat::iter_raw()
                .into_video_caps()
                .unwrap()
                .any_features()
                .build();

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

            vec![src_pad_template, sink_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let result = self.parent_change_state(transition);

        if transition == gst::StateChange::PausedToReady {
            *self.state.borrow_mut() = State::default();
        }

        result
    }
}

impl OverlayCompositor {
    fn query_caps(&self, pad: &gst::Pad, filter: Option<&gst::CapsRef>) -> gst::Caps {
        let peer_caps = if pad.direction() == gst::PadDirection::Sink {
            self.srcpad.peer_query_caps(None)
        } else {
            self.sinkpad.peer_query_caps(None)
        };
        let caps = peer_caps.intersect(&pad.pad_template_caps());

        let mut result = caps.clone();
        for (structure, features) in caps.iter_with_features() {
            if features.is_any() || !features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY) {
                continue;
            }

            let mut features = features.to_owned();
            if pad.direction() == gst::PadDirection::Sink {
                features.add(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
            } else {
                features.remove(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
            }

            result
                .make_mut()
                .append_structure_full(structure.to_owned(), Some(features));
        }

        if let Some(filter) = filter {
            filter.intersect_with_mode(&result, gst::CapsIntersectMode::First)
        } else {
            result
        }
    }

    fn query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                q.set_result(&self.query_caps(pad, q.filter()));
                true
            }
            gst::QueryViewMut::AcceptCaps(q) => {
                let caps = q.caps();
                let allowed_caps = self.query_caps(pad, Some(caps));
                q.set_result(caps.can_intersect(&allowed_caps));
                true
            }
            gst::QueryViewMut::Allocation(q) => {
                let result = self.srcpad.peer_query(q);
                let system_memory =
                    q.caps()
                        .and_then(|caps| caps.features(0))
                        .is_some_and(|features| {
                            !features.is_any()
                                && features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)
                        });
                if !system_memory {
                    return result;
                }

                // We can handle overlay metadata in system memory even if downstream allocation fails
                if q.find_allocation_meta::<gst_video::VideoOverlayCompositionMeta>()
                    .is_none()
                {
                    q.add_allocation_meta::<gst_video::VideoOverlayCompositionMeta>(None);
                }
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn negotiate(&self) -> bool {
        self.srcpad.check_reconfigure();

        let (caps, system_memory) = {
            let mut state = self.state.borrow_mut();

            state.output_caps = None;
            state.downstream_meta = false;
            let Some(caps) = state.caps.clone() else {
                gst::error!(CAT, imp = self, "Cannot negotiate without input caps");
                return false;
            };

            (caps, state.system_memory)
        };

        let overlay_caps = with_overlay_feature(&caps, true);
        let peer_caps = self.srcpad.peer_query_caps(Some(&overlay_caps));
        let outcaps = if !peer_caps.is_empty() {
            overlay_caps
        } else {
            if has_overlay_feature(&caps) && !system_memory {
                gst::error!(
                    CAT,
                    imp = self,
                    "Cannot remove overlay metadata from non-system-memory input caps {caps}"
                );
                self.srcpad.mark_reconfigure();
                return false;
            }

            with_overlay_feature(&caps, false)
        };

        self.srcpad.push_event(gst::event::Caps::new(&outcaps));

        let mut state = self.state.borrow_mut();
        state.downstream_meta = has_overlay_feature(&outcaps);

        gst::debug!(
            CAT,
            imp = self,
            "Output caps {outcaps:?}, downstream meta: {}, system memory: {}",
            state.downstream_meta,
            state.system_memory
        );

        state.output_caps = Some(outcaps);
        true
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        #[allow(clippy::single_match)]
        match event.view() {
            gst::EventView::Caps(event) => {
                let caps = event.caps_owned();

                gst::debug!(CAT, imp = self, "Setting input caps {caps:?}");

                let Ok(info) = gst_video::VideoInfo::from_caps(&caps) else {
                    *self.state.borrow_mut() = State::default();
                    gst::error!(CAT, imp = self, "Invalid video caps {caps}");
                    return false;
                };

                let features = caps.features(0).unwrap();
                let system_memory =
                    !features.is_any() && features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY);

                *self.state.borrow_mut() = State {
                    caps: Some(caps),
                    info: Some(info),
                    system_memory,
                    ..State::default()
                };

                self.negotiate()
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if self.srcpad.check_reconfigure() && !self.negotiate() {
            return Err(gst::FlowError::NotNegotiated);
        }

        if buffer
            .meta::<gst_video::VideoOverlayCompositionMeta>()
            .is_none()
        {
            return self.srcpad.push(buffer);
        }

        let state = self.state.borrow();
        if state.output_caps.is_none() {
            gst::error!(CAT, imp = self, "Output caps have not been negotiated");
            return Err(gst::FlowError::NotNegotiated);
        }

        if state.downstream_meta || !state.system_memory {
            drop(state);
            return self.srcpad.push(buffer);
        }

        let info = state.info.as_ref().unwrap();

        let overlays = buffer
            .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
            .filter(|meta| meta.overlay().n_rectangles() > 0)
            .map(|meta| meta.overlay_owned())
            .collect::<Vec<_>>();
        if overlays.is_empty() {
            drop(state);
            return self.srcpad.push(buffer);
        }

        let buffer_ref = buffer.make_mut();
        {
            let mut frame =
                match gst_video::VideoFrameRef::from_buffer_ref_writable(buffer_ref, info) {
                    Ok(frame) => frame,
                    Err(err) => {
                        drop(state);
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::Write,
                            ["Failed to map video frame: {err}"]
                        );
                        return Err(gst::FlowError::Error);
                    }
                };

            gst::log!(
                CAT,
                imp = self,
                "Blending {} overlay compositions",
                overlays.len()
            );

            for overlay in overlays {
                if let Err(err) = overlay.blend(&mut frame) {
                    drop(state);
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Failed,
                        ["Failed to blend overlay: {err}"]
                    );
                    return Err(gst::FlowError::Error);
                }
            }
        }
        drop(state);

        while let Some(meta) = buffer_ref.meta_mut::<gst_video::VideoOverlayCompositionMeta>() {
            if let Err(err) = meta.remove() {
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Failed,
                    ["Failed to remove overlay meta: {err}"]
                );
                return Err(gst::FlowError::Error);
            }
        }

        self.srcpad.push(buffer)
    }
}
