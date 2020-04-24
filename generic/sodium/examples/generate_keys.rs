// generate_keys.rs
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

use clap::{App, Arg};
use serde::{Deserialize, Serialize};
use sodiumoxide::crypto::box_;
use std::fs::File;

#[derive(Debug, Serialize, Deserialize)]
struct Keys {
    public: box_::PublicKey,
    private: box_::SecretKey,
}

impl Keys {
    fn new() -> Self {
        let (public, private) = box_::gen_keypair();
        Keys { public, private }
    }

    fn write_to_file(&self, path: &str, json: bool) {
        if json {
            let path = if !path.ends_with(".json") {
                format!("{}.json", path)
            } else {
                path.into()
            };

            let file =
                File::create(&path).unwrap_or_else(|_| panic!("Failed to create file at {}", path));
            serde_json::to_writer(file, &self)
                .unwrap_or_else(|_| panic!("Failed to write to file at {}", path));
        } else {
            use std::io::Write;
            use std::path::PathBuf;

            let mut private = PathBuf::from(path);
            private.set_extension("prv");
            let mut file = File::create(&private)
                .unwrap_or_else(|_| panic!("Failed to create file at {}", private.display()));
            file.write_all(&self.private.0)
                .unwrap_or_else(|_| panic!("Failed to write to file at {}", private.display()));

            let mut public = PathBuf::from(path);
            public.set_extension("pub");
            let mut file = File::create(&public)
                .unwrap_or_else(|_| panic!("Failed to create file at {}", public.display()));
            file.write_all(self.public.as_ref())
                .unwrap_or_else(|_| panic!("Failed to write to file at {}", public.display()));
        }
    }
}

fn main() {
    let matches = App::new("Generate the keys to be used with the sodium element")
        .version("1.0")
        .author("Jordan Petridis <jordan@centricular.com>")
        .about("Generate a pair of Sodium's crypto_box_curve25519xsalsa20poly1305 keys.")
        .arg(
            Arg::with_name("path")
                .long("path")
                .short("p")
                .value_name("FILE")
                .help("Path to write the Keys")
                .required(true)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("json")
                .long("json")
                .short("j")
                .help("Write a JSON file instead of a key.prv/key.pub pair"),
        )
        .get_matches();

    let keys = Keys::new();

    let path = matches.value_of("path").unwrap();
    keys.write_to_file(path, matches.is_present("json"));
}
