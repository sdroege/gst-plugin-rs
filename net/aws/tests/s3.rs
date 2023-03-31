// Copyright (C) 2022, Asymptotic Inc.
//      Author: Arun Raghavan <arun@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//

use gst::prelude::*;

const DEFAULT_S3_REGION: &str = "us-west-2";

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstaws::plugin_register_static().unwrap();
    });
}

// The test times out on Windows for some reason, skip until we figure out why
#[cfg(not(target_os = "windows"))]
#[test_with::env(AWS_ACCESS_KEY_ID)]
#[test_with::env(AWS_SECRET_ACCESS_KEY)]
#[tokio::test]
async fn test_s3() {
    init();
    // Makes it easier to get AWS SDK logs if needed
    env_logger::init();

    let region = std::env::var("AWS_REGION").unwrap_or_else(|_| DEFAULT_S3_REGION.to_string());
    let bucket =
        std::env::var("AWS_S3_BUCKET").unwrap_or_else(|_| "gst-plugins-rs-tests".to_string());
    let key = format!("s3-test-{:?}.txt", chrono::Utc::now());
    let uri = format!("s3://{region}/{bucket}/{key}");
    let content = "Hello, world!\n".as_bytes();

    // Manually add the element so we can configure it before it goes to PLAYING
    let mut h1 = gst_check::Harness::new_empty();
    // Need to add_parse() because the Harness API / Rust bindings aren't conducive to creating and
    // adding an element manually
    h1.add_parse(format!("awss3sink uri={uri}").as_str());

    h1.set_src_caps(gst::Caps::builder("text/plain").build());
    h1.play();

    h1.push(gst::Buffer::from_slice(content)).unwrap();
    h1.push_event(gst::event::Eos::new());

    let mut h2 = gst_check::Harness::new("awss3src");
    h2.element().unwrap().set_property("uri", uri.clone());
    h2.play();

    let buf = h2.pull_until_eos().unwrap().unwrap();
    assert_eq!(
        content,
        buf.into_mapped_buffer_readable().unwrap().as_slice()
    );

    let region_provider = aws_config::meta::region::RegionProviderChain::first_try(
        aws_sdk_s3::config::Region::new(region.clone()),
    )
    .or_default_provider();

    let config = aws_config::from_env().region(region_provider).load().await;
    let client = aws_sdk_s3::Client::new(&config);

    client
        .delete_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .unwrap();
}
