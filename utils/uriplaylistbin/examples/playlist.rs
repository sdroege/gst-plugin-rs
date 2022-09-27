// Copyright (C) 2021 OneStream Live <guillaume.desmottes@onestream.live>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use clap::Parser;
use gst::prelude::*;

#[derive(Debug, Parser)]
#[clap(version, author)]
#[clap(about = "An example of uriplaylistbin usage.")]
struct Opt {
    #[clap(short, default_value = "1", action)]
    iterations: u32,
    uris: Vec<String>,
}

fn create_pipeline(uris: Vec<String>, iterations: u32) -> anyhow::Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::new(None);
    let playlist = gst::ElementFactory::make("uriplaylistbin", None)?;

    pipeline.add(&playlist)?;

    playlist.set_property("uris", &uris);
    playlist.set_property("iterations", &iterations);

    let sink_bins = Arc::new(Mutex::new(HashMap::new()));
    let sink_bins_clone = sink_bins.clone();

    let pipeline_weak = pipeline.downgrade();
    playlist.connect_pad_added(move |_playlist, src_pad| {
        let pipeline = match pipeline_weak.upgrade() {
            None => return,
            Some(pipeline) => pipeline,
        };
        let pad_name = src_pad.name();

        let sink = if pad_name.starts_with("audio") {
            gst::parse_bin_from_description("audioconvert ! audioresample ! autoaudiosink", true)
                .unwrap()
        } else if pad_name.starts_with("video") {
            gst::parse_bin_from_description("videoconvert ! autovideosink", true).unwrap()
        } else {
            unimplemented!();
        };

        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();

        let sink_pad = sink.static_pad("sink").unwrap();
        src_pad.link(&sink_pad).unwrap();

        sink_bins.lock().unwrap().insert(pad_name, sink);
    });

    let pipeline_weak = pipeline.downgrade();
    playlist.connect_pad_removed(move |_playlist, pad| {
        let pipeline = match pipeline_weak.upgrade() {
            None => return,
            Some(pipeline) => pipeline,
        };

        // remove sink bin that was handling the pad
        let sink_bins = sink_bins_clone.lock().unwrap();
        let sink = sink_bins.get(&pad.name()).unwrap();
        pipeline.remove(sink).unwrap();
        let _ = sink.set_state(gst::State::Null);
    });

    Ok(pipeline)
}

fn main() -> anyhow::Result<()> {
    gst::init().unwrap();
    gsturiplaylistbin::plugin_register_static().expect("Failed to register uriplaylistbin plugin");

    let opt = Opt::parse();
    if opt.uris.is_empty() {
        anyhow::bail!("Need at least one URI to play");
    }

    let uris = opt
        .uris
        .into_iter()
        .map(|uri| {
            let p = Path::new(&uri);
            match p.canonicalize() {
                Ok(p) => format!("file://{}", p.to_str().unwrap()),
                _ => uri,
            }
        })
        .collect();

    {
        let pipeline = create_pipeline(uris, opt.iterations)?;

        pipeline
            .set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to the `Playing` state");

        let bus = pipeline.bus().unwrap();
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    eprintln!(
                        "Error received from element {:?}: {}",
                        err.src().map(|s| s.path_string()),
                        err.error()
                    );
                    eprintln!("Debugging information: {:?}", err.debug());
                    break;
                }
                MessageView::Eos(..) => break,
                _ => (),
            }
        }

        pipeline
            .set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");
    }

    unsafe {
        gst::deinit();
    }

    Ok(())
}
