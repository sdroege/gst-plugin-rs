// Copyright (C) 2020 Markus Ebner <info@ebner-markus.de>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use gst::prelude::*;

const ENCODE_PIPELINE: &str = "videotestsrc is-live=false num-buffers=100 ! videoconvert ! gifenc ! filesink location=test.gif";

fn main() {
    gst::init().unwrap();
    gstgif::plugin_register_static().expect("Failed to register gif plugin");

    let pipeline = gst::parse_launch(ENCODE_PIPELINE).unwrap();
    let bus = pipeline.bus().unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .expect("Failed to set pipeline state to playing");

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                println!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                break;
            }
            _ => (),
        }
    }
    pipeline
        .set_state(gst::State::Null)
        .expect("Failed to set pipeline state to null");
}
