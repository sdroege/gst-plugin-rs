// Copyright (C) 2025 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use pretty_assertions::assert_eq;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().unwrap();
    });
}

#[test]
fn test_parse() {
    init();
    let data = include_bytes!("608-all-features.scc").as_ref();
    let expected_data = include_bytes!("608-all-features-unbuffered.json").as_ref();

    let deserializer = serde_json::Deserializer::from_reader(expected_data);
    let mut input_iter = deserializer
        .into_iter::<serde_json::Value>()
        .map(|item| item.unwrap());

    let mut h = gst_check::Harness::new_parse("sccparse ! cea608tojson unbuffered=true");
    h.set_src_caps_str("application/x-scc");
    h.set_sink_caps_str("application/x-json");

    let buf = gst::Buffer::from_mut_slice(Vec::from(data));
    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));

    loop {
        let Some(input_item) = input_iter.next() else {
            break;
        };

        let buf = h.pull().unwrap();

        let data = buf.map_readable().unwrap();

        let s = std::str::from_utf8(&data).unwrap_or_else(|_| panic!("Non-UTF8 data"));

        let output_item = s.parse::<serde_json::Value>().unwrap();

        assert_eq!(input_item, output_item);
    }
}
