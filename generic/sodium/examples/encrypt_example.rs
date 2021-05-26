// encrypt_example.rs
//
// Copyright 2019 Jordan Petridis <jordan@centricular.com>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to
// deal in the Software without restriction, including without limitation the
// rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
// sell copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
// IN THE SOFTWARE.
//
// SPDX-License-Identifier: MIT

use gst::glib;
use gst::prelude::*;
use sodiumoxide::crypto::box_;

use std::error::Error;
use std::fs::File;
use std::path::{Path, PathBuf};

use clap::{App, Arg};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Keys {
    public: box_::PublicKey,
    private: box_::SecretKey,
}

impl Keys {
    fn from_file(file: &Path) -> Result<Self, Box<dyn Error>> {
        let f = File::open(&file)?;
        serde_json::from_reader(f).map_err(From::from)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let matches = App::new("Encrypt a file with in the gstsodium10 format")
        .version("1.0")
        .author("Jordan Petridis <jordan@centricular.com>")
        .arg(
            Arg::with_name("input")
                .short("i")
                .long("input")
                .value_name("FILE")
                .help("File to encrypt")
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("output")
                .short("o")
                .long("output")
                .value_name("FILE")
                .help("File to decrypt")
                .required(true)
                .takes_value(true),
        )
        .get_matches();

    gst::init()?;
    gstsodium::plugin_register_static().expect("Failed to register sodium plugin");

    let input_loc = matches.value_of("input").unwrap();
    let out_loc = matches.value_of("output").unwrap();

    let receiver_keys = {
        let mut r = PathBuf::new();
        r.push(env!("CARGO_MANIFEST_DIR"));
        r.push("examples");
        r.push("receiver_sample");
        r.set_extension("json");
        r
    };

    let sender_keys = {
        let mut s = PathBuf::new();
        s.push(env!("CARGO_MANIFEST_DIR"));
        s.push("examples");
        s.push("sender_sample");
        s.set_extension("json");
        s
    };

    let receiver = &Keys::from_file(&receiver_keys)?;
    let sender = &Keys::from_file(&sender_keys)?;

    let filesrc = gst::ElementFactory::make("filesrc", None).unwrap();
    let encrypter = gst::ElementFactory::make("sodiumencrypter", None).unwrap();
    let filesink = gst::ElementFactory::make("filesink", None).unwrap();

    filesrc
        .set_property("location", &input_loc)
        .expect("Failed to set location property");
    filesink
        .set_property("location", &out_loc)
        .expect("Failed to set location property");

    encrypter.set_property("receiver-key", &glib::Bytes::from_owned(receiver.public))?;
    encrypter.set_property("sender-key", &glib::Bytes::from_owned(sender.private.0))?;

    let pipeline = gst::Pipeline::new(Some("test-pipeline"));
    pipeline
        .add_many(&[&filesrc, &encrypter, &filesink])
        .expect("failed to add elements to the pipeline");
    gst::Element::link_many(&[&filesrc, &encrypter, &filesink])
        .expect("failed to link the elements");

    pipeline.set_state(gst::State::Playing)?;

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

    pipeline.set_state(gst::State::Null)?;

    Ok(())
}
