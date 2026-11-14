// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Replace the core `typefind` element factory with `TypeFind2` at runtime.
//!
//! The registry stores plugin features in a hash keyed by feature name
//! (`gst_registry_add_feature()` in gstregistry.c), so registering a second
//! factory under the name "typefind" replaces the core one. Everything that
//! resolves the element by name (`gst_element_factory_make("typefind")`,
//! gst-launch, decodebin, parsebin, urisourcebin, discoverer) then gets
//! `TypeFind2`.
//!
//! Two things make the override stick:
//!
//! * The replacement has to be re-asserted whenever another factory appears
//!   under the name "typefind", because loading a plugin runs its
//!   `plugin_init()`, and `coreelements` registers `typefind` again, replacing
//!   our factory. This happens lazily: creating any core element (e.g.
//!   `filesrc`) loads `coreelements` if it is not loaded yet, and that does not
//!   emit "plugin-added" for an already registered plugin. Hence the
//!   "feature-added" handler.
//! * The rank only matters for factory lists; name lookup is rank-independent.
//!   It is set above `GST_RANK_NONE` anyway so that the override wins wherever
//!   ranks are compared.
//!
//! Run with `cargo run -p gst-plugin-typefind2 --example registry_override -- <media-file>`.

use gst::glib;
use gst::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: registry_override <media-file>");
        std::process::exit(1);
    };

    gst::init()?;

    // Register our plugin as a static plugin unless it is already loaded from
    // its shared object (e.g. via GST_PLUGIN_PATH).
    if gst::Registry::get().find_plugin("typefind2").is_none() {
        gsttypefind2::plugin_desc::plugin_register_static()?;
    }

    describe("before");

    register_override()?;

    describe("after");

    let factory = gst::ElementFactory::find("typefind").expect("typefind factory");
    assert_eq!(
        factory.element_type(),
        gsttypefind2::TypeFind2::static_type(),
        "the registry still resolves \"typefind\" to the core element"
    );

    typefind_pipeline(&path)?;
    urisourcebin_pipeline(&path)?;

    Ok(())
}

/// Register `TypeFind2` as "typefind" and keep it registered.
fn register_override() -> Result<(), Box<dyn std::error::Error>> {
    assert_override()?;

    gst::Registry::get().connect_feature_added(|_registry, feature| {
        // Loading a plugin re-runs its plugin_init(), which registers its
        // features again. If something else (re)registers "typefind", put ours
        // back. Our own registration lands here too, and is a no-op.
        if feature.name() != "typefind" {
            return;
        }
        if feature
            .downcast_ref::<gst::ElementFactory>()
            .is_some_and(|f| f.element_type() == gsttypefind2::TypeFind2::static_type())
        {
            return;
        }
        let _ = assert_override();
    });

    Ok(())
}

fn assert_override() -> Result<(), glib::BoolError> {
    let plugin = gst::Registry::get().find_plugin("typefind2");
    gst::Element::register(
        plugin.as_ref(),
        "typefind",
        gst::Rank::MARGINAL,
        gsttypefind2::TypeFind2::static_type(),
    )
}

fn describe(when: &str) {
    match gst::ElementFactory::find("typefind") {
        Some(factory) => println!(
            "{when:>6}: \"typefind\" -> plugin {:?}, GType {}",
            factory.plugin_name(),
            // Factories restored from the registry cache are not loaded yet and
            // report an invalid type until their plugin is loaded.
            factory.element_type().name(),
        ),
        None => println!("{when:>6}: \"typefind\" -> not registered"),
    }
}

/// Run `urisourcebin parse-streams=true`, which builds its typefinding graph
/// by looking up the element named "typefind" by name in the registry (from
/// gst-plugins-base, without knowing our class exists). A `TypeFind2` instance
/// appearing inside its bins proves the override reaches nested consumers.
fn urisourcebin_pipeline(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let pipeline = gst::Pipeline::new();
    let urisourcebin = gst::ElementFactory::make("urisourcebin")
        .property_from_str("uri", &glib::filename_to_uri(path, None)?)
        .property("parse-streams", true)
        .build()?;
    let sink = gst::ElementFactory::make("fakesink").build()?;

    pipeline.add_many([&urisourcebin, &sink])?;
    sink.sync_state_with_parent().ok();

    let added = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    urisourcebin.connect_pad_added({
        let added = added.clone();
        let sink = sink.clone();
        move |_urisourcebin, pad| {
            println!("pad-added: {}", pad.name());
            if pad.link(&sink.static_pad("sink").unwrap()).is_ok() {
                sink.sync_state_with_parent().ok();
                added.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });

    pipeline.set_state(gst::State::Playing)?;

    let mut ours: Vec<_> = Vec::new();
    for _ in 0..100 {
        ours = pipeline
            .iterate_recurse()
            .into_iter()
            .filter_map(|child| child.ok()?.downcast::<gsttypefind2::TypeFind2>().ok())
            .collect();
        if added.load(std::sync::atomic::Ordering::Relaxed) && !ours.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    for element in &ours {
        println!("inside urisourcebin: {} (TypeFind2)", element.path_string());
    }
    pipeline.set_state(gst::State::Null)?;

    if ours.is_empty() {
        Err("urisourcebin did not instantiate our element under the name \"typefind\"".into())
    } else {
        Ok(())
    }
}

/// Run `filesrc ! typefind ! fakesink` and report what our element detects.
fn typefind_pipeline(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", path)
        .build()?;
    let sink = gst::ElementFactory::make("fakesink").build()?;

    // The downcast is the proof: the factory handed us our own class.
    let typefind = gst::ElementFactory::make("typefind")
        .build()?
        .downcast::<gsttypefind2::TypeFind2>()
        .expect("\"typefind\" did not create a TypeFind2");

    typefind.connect_have_type(|_typefind, probability, caps| {
        println!("have-type: probability {probability}, caps {caps}");
    });

    pipeline.add_many([&src, typefind.upcast_ref(), &sink])?;
    gst::Element::link_many([&src, typefind.upcast_ref(), &sink])?;

    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline.bus().expect("pipeline bus");
    let mut result = Ok(());
    for msg in bus.iter_timed(gst::ClockTime::from_seconds(10)) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                result = Err(format!(
                    "{}: {} ({:?})",
                    err.src().map(|s| s.path_string()).unwrap_or_default(),
                    err.error(),
                    err.debug()
                )
                .into());
                break;
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;

    result
}
