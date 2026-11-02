// Copyright (C) 2026 Collabora
//  @author: Daniel Morin <daniel.morin@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*, subclass::prelude::*};
use std::{
    collections::HashSet,
    sync::{LazyLock, Mutex},
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "tensordecodebin2",
        gst::DebugColorFlags::empty(),
        Some("Tensor decode bin"),
    )
});

#[derive(Default)]
struct State {
    factories_cookie: u32,
    tensordec_factories: Vec<gst::ElementFactory>,
    aggregated_caps: Option<gst::Caps>,
}

pub struct TensorDecodeBin {
    sinkpad: gst::GhostPad,
    srcpad: gst::GhostPad,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for TensorDecodeBin {
    const NAME: &'static str = "GstTensorDecodeBin2";
    type Type = super::TensorDecodeBin;
    type ParentType = gst::Bin;

    fn with_class(klass: &Self::Class) -> Self {
        let sink_templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::GhostPad::builder_from_template(&sink_templ)
            .event_function(|pad, parent, event| {
                TensorDecodeBin::catch_panic_pad_function(
                    parent,
                    || false,
                    |self_| self_.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                TensorDecodeBin::catch_panic_pad_function(
                    parent,
                    || false,
                    |self_| self_.sink_query(pad, query),
                )
            })
            .build();

        let src_templ = klass.pad_template("src").unwrap();
        let srcpad = gst::GhostPad::from_template(&src_templ);

        Self {
            sinkpad,
            srcpad,
            state: Mutex::new(State::default()),
        }
    }
}

impl ObjectImpl for TensorDecodeBin {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for TensorDecodeBin {}

impl ElementImpl for TensorDecodeBin {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "tensordecodebin2",
                "Tensordecoder/Bin",
                "Tensor Decode Bin",
                "Daniel Morin <daniel.morin@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_any();

            vec![
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
            ]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BinImpl for TensorDecodeBin {}

impl TensorDecodeBin {
    /// Keep only element factories advertising a "Tensordecoder" klass, excluding itselves, with
    /// rank >= MARGINAL.
    fn decoder_filter(&self, feature: &gst::PluginFeature) -> bool {
        let Some(factory) = feature.downcast_ref::<gst::ElementFactory>() else {
            return false;
        };

        if !factory.klass().contains("Tensordecoder") {
            return false;
        }

        if self.obj().factory().as_ref() == Some(factory) {
            return false;
        }

        // Transitional: also exclude the legacy "tensordecodebin"
        // TODO: remove once that element is retired and this port takes over its name.
        if factory.name() == "tensordecodebin" {
            return false;
        }

        if factory.rank() < gst::Rank::MARGINAL {
            return false;
        }

        gst::debug!(CAT, imp = self, "adding {} factory", factory.name());
        true
    }

    /// Cached, rank-sorted list of tensor decoder factories, rebuilding it when the registry's
    /// feature list cookie has changed.
    fn tensordec_factories(&self) -> Vec<gst::ElementFactory> {
        let registry = gst::Registry::get();
        let cookie = registry.feature_list_cookie();

        let mut state = self.state.lock().unwrap();
        if state.tensordec_factories.is_empty() || state.factories_cookie != cookie {
            let mut factories: Vec<gst::ElementFactory> = registry
                .features_filtered(|feature| self.decoder_filter(feature), false)
                .into_iter()
                .filter_map(|feature| feature.downcast::<gst::ElementFactory>().ok())
                .collect();
            factories
                .sort_by_key(|f| std::cmp::Reverse(f.upcast_ref::<gst::PluginFeature>().rank()));

            state.tensordec_factories = factories;
            state.factories_cookie = cookie;
            state.aggregated_caps = None;
        }

        state.tensordec_factories.clone()
    }

    /// A tensor decoder is only considered if it has exactly one Always sink pad template and one
    /// Always src pad template. Request/sometimes pads aren't supported.
    fn compatible_sink_template(
        &self,
        factory: &gst::ElementFactory,
    ) -> Option<gst::StaticPadTemplate> {
        let mut sink_templates = Vec::new();
        let mut src_templates = Vec::new();

        for tpl in factory.static_pad_templates() {
            if tpl.presence() != gst::PadPresence::Always {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Tensor decoder {} has a {:?} pad which is not currently supported by \
                     tensordecodebin2 and is ignored.",
                    factory.longname(),
                    tpl.presence(),
                );
                continue;
            }

            match tpl.direction() {
                gst::PadDirection::Sink => sink_templates.push(tpl),
                gst::PadDirection::Src => src_templates.push(tpl),
                _ => gst::warning!(
                    CAT,
                    imp = self,
                    "Tensor decoder {} has a pad template with UNKNOWN direction, skipping this \
                     template.",
                    factory.longname(),
                ),
            }
        }

        if sink_templates.len() != 1 || src_templates.len() != 1 {
            gst::warning!(
                CAT,
                imp = self,
                "tensordecodebin2 only supports tensor decoders with 1 always sinkpad and 1 \
                 always srcpad, but {} has {} sinkpad(s) and {} srcpad(s) and will not be \
                 considered",
                factory.longname(),
                sink_templates.len(),
                src_templates.len(),
            );
            return None;
        }

        sink_templates.into_iter().next()
    }

    fn remove_all_elements(&self) {
        gst::debug!(CAT, imp = self, "Removing all children");

        let obj = self.obj();
        for child in obj.children() {
            let _ = obj.remove(&child);
            let _ = child.set_state(gst::State::Null);
        }
    }

    fn sink_caps_event(&self, caps: &gst::Caps) -> bool {
        let _ = self.sinkpad.set_target(None::<&gst::Pad>);
        let _ = self.srcpad.set_target(None::<&gst::Pad>);
        self.remove_all_elements();

        match self.try_sink_caps_event(caps) {
            Ok(()) => true,
            Err(()) => {
                self.remove_all_elements();
                false
            }
        }
    }

    fn try_sink_caps_event(&self, caps: &gst::Caps) -> Result<(), ()> {
        let obj = self.obj();

        // We check that the tensor group can be handled by a tensor decoder.
        let ecaps_struct = caps.structure(0);
        let has_tensors = ecaps_struct.is_some_and(|s| s.has_field("tensors"));

        if !has_tensors {
            // No tensor caps, we don't need any tensor decoder.
            gst::info!(
                CAT,
                imp = self,
                "No tensor caps in, tensordecodebin2 will be passthrough"
            );

            let identity = gst::ElementFactory::make("identity")
                .build()
                .map_err(|_| gst::error!(CAT, imp = self, "Failed to create identity"))?;

            obj.add(&identity)
                .map_err(|_| gst::error!(CAT, imp = self, "Failed to add identity"))?;

            let sinkpad = identity.static_pad("sink").unwrap();
            self.sinkpad.set_target(Some(&sinkpad)).map_err(|_| {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to set sinkpad target to identity.sinkpad"
                )
            })?;

            let srcpad = identity.static_pad("src").unwrap();
            let _ = identity.sync_state_with_parent();

            self.srcpad
                .set_target(Some(&srcpad))
                .map_err(|_| gst::error!(CAT, imp = self, "Failed to set srcpad target"))?;

            return Ok(());
        }

        // NOTE: tensordecodebin2 assumes that a tensordecoder does not modify the media or the
        // capabilities. This is not a fundamental limitation of tensor capabilities but rather a
        // limitation of the current tensordecodebin2 implementation. To implement support for
        // tensordecoder-induced capability changes, we would need to maintain a full history of
        // transformations. Currently, a tensordecoder assumes the tensor was produced by
        // inference on the attached media. However, this assumption will not hold if a
        // tensordecoder can modify media. Consequently, a tensordecoder following one that
        // changes media would need to retrieve media details from the time the inference
        // produced the tensor being decoded.
        let factories = self.tensordec_factories();

        // Tracks, for this caps event, which tensor-group-ids have already been claimed by a
        // selected decoder, so that we don't use two tensor decoders that both support the same
        // tensor caps.
        let ecaps_tensorgroups =
            ecaps_struct.and_then(|s| s.get_optional::<gst::Structure>("tensors").ok().flatten());
        let mut handled_group_ids: HashSet<String> = HashSet::new();

        let mut srcpad: Option<gst::Pad> = None;

        for factory in &factories {
            // `compatible_sink_template` returning `None` means this factory doesn't satisfy
            // tensordecodebin2's "exactly one always sink/src pad" requirement
            let Some(tpl) = self.compatible_sink_template(factory) else {
                continue;
            };

            let tplcaps = tpl.caps();

            // Check that the sinkpad has at least a tensors field.
            let Some(tpl_struct) = tplcaps.structure(0).filter(|s| s.has_field("tensors")) else {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Element from {} factory have no tensors capabilities",
                    factory.longname(),
                );
                continue;
            };

            if !caps.is_subset(&tplcaps) {
                continue;
            }

            let Some(matched_group_ids) = unhandled_tensor_group_ids(
                tpl_struct,
                ecaps_tensorgroups.as_deref(),
                &handled_group_ids,
            ) else {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Element from {} factory has unusable tensors capabilities",
                    factory.longname(),
                );
                continue;
            };

            if matched_group_ids.is_empty() {
                gst::debug!(
                    CAT,
                    imp = self,
                    "Skipping {} factory, its tensor-group-id(s) are already handled by a \
                     previously selected decoder",
                    factory.longname(),
                );
                continue;
            }

            let Ok(elem) = factory.create().build() else {
                continue;
            };

            let Some(new_sinkpad) = elem.static_pad(tpl.name_template()) else {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Element {:?} from {} factory has no sinkpad",
                    elem,
                    factory.longname(),
                );
                continue;
            };

            if !pad_accept_caps(&new_sinkpad, caps) {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Factory {:?}'s sinkpad {:?} didn't accept caps: {:?}",
                    factory,
                    new_sinkpad,
                    caps,
                );
                continue;
            }

            if obj.add(&elem).is_err() {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Failed to add element {:?} from {} factory",
                    elem,
                    factory.longname(),
                );
                continue;
            }

            gst::debug!(CAT, imp = self, "selected tensor decoder: {:?}", elem);

            if elem.sync_state_with_parent().is_err() {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Element {:?} failed to synchronise its state with parent and will not be \
                     added to this bin.",
                    elem,
                );
                let _ = obj.remove(&elem);
                continue;
            }

            if let Some(prev_srcpad) = srcpad.take() {
                prev_srcpad.link(&new_sinkpad).map_err(|_| {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Could not link {:?} and {:?}",
                        prev_srcpad,
                        new_sinkpad,
                    )
                })?;
            } else {
                self.sinkpad
                    .set_target(Some(&new_sinkpad))
                    .map_err(|_| gst::error!(CAT, imp = self, "Failed to set sinkpad target"))?;
            }

            handled_group_ids.extend(matched_group_ids);
            srcpad = elem.static_pad("src");
        }

        let srcpad = srcpad.ok_or_else(|| {
            gst::warning!(
                CAT,
                imp = self,
                "Could not find tensor decoder for {:?}",
                caps
            )
        })?;

        self.srcpad
            .set_target(Some(&srcpad))
            .map_err(|_| gst::error!(CAT, imp = self, "Failed to set srcpad target"))?;

        Ok(())
    }

    fn sink_event(&self, pad: &gst::GhostPad, event: gst::Event) -> bool {
        if let gst::EventView::Caps(ev) = event.view() {
            if !self.sink_caps_event(&ev.caps_owned()) {
                return false;
            }

            return gst::Pad::event_default(pad, Some(&*self.obj()), event);
        }

        gst::Pad::event_default(pad, Some(&*self.obj()), event)
    }

    fn tensordecoders_caps(&self) -> gst::Caps {
        let factories = self.tensordec_factories();

        let mut state = self.state.lock().unwrap();
        if let Some(ref caps) = state.aggregated_caps {
            return caps.clone();
        }

        let mut acc_caps = gst::Caps::new_empty();
        for factory in &factories {
            let Some(tpl) = self.compatible_sink_template(factory) else {
                gst::warning!(
                    CAT,
                    imp = self,
                    "No compatible sinkpad template found for {} factory",
                    factory.longname(),
                );
                continue;
            };

            acc_caps.merge(tpl.caps());
        }

        state.aggregated_caps = Some(acc_caps.clone());
        acc_caps
    }

    fn sink_query(&self, pad: &gst::GhostPad, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                let filter_caps = q.filter_owned();

                let mut acc_caps = self.tensordecoders_caps();
                if let Some(ref filter) = filter_caps {
                    acc_caps = acc_caps.intersect(filter);
                }

                let mut dn_query = gst::query::Caps::new(Some(&acc_caps));
                if self.srcpad.peer_query(&mut dn_query)
                    && let Some(filter) = dn_query.result_owned()
                {
                    acc_caps = acc_caps.intersect(&filter);
                }

                q.set_result(&acc_caps);
                true
            }
            gst::QueryViewMut::AcceptCaps(q) => {
                let acc_caps = self.tensordecoders_caps();
                let caps = q.caps_owned();
                q.set_result(acc_caps.can_intersect(&caps));
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }
}

fn pad_accept_caps(pad: &gst::Pad, caps: &gst::Caps) -> bool {
    let mut query = gst::query::AcceptCaps::new(caps);
    pad.query(&mut query) && query.result()
}

/// Returns the tensor-group-ids declared by `tpl_struct`'s' "tensors" field that aren't already
/// in `handled_group_ids`, or `None` if `tpl_struct` has no usable "tensors" structure.
fn unhandled_tensor_group_ids(
    tpl_struct: &gst::StructureRef,
    ecaps_tensorgroups: Option<&gst::StructureRef>,
    handled_group_ids: &HashSet<String>,
) -> Option<Vec<String>> {
    let tpl_tensorgroups = tpl_struct
        .get_optional::<gst::Structure>("tensors")
        .ok()
        .flatten()?;
    ecaps_tensorgroups?;

    Some(
        tpl_tensorgroups
            .fields()
            .filter(|name| !handled_group_ids.contains(name.as_str()))
            .map(|name| name.as_str().to_string())
            .collect(),
    )
}
