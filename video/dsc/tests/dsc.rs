// Copyright (C) 2025, Fluendo S.A.
//      Author: Diego Nieto <dnieto@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gstdsc::HashMethod;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_RSA_SHA256};
use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use rsa::rand_core::OsRng;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static KEY_COUNTER: AtomicU64 = AtomicU64::new(0);

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstdsc::plugin_register_static().expect("dsc test");
    });
}

/// The end-to-end tests below push real H.265 through the elements, which
/// requires an encoder. It is not available everywhere (e.g. the Windows CI
/// runners ship no x265enc), so skip those tests instead of failing.
fn have_x265enc() -> bool {
    gst::ElementFactory::find("x265enc").is_some()
}

fn create_test_keys() -> (PathBuf, PathBuf) {
    let temp_dir = std::env::temp_dir();
    let unique_id = format!(
        "{}_{}",
        std::process::id(),
        KEY_COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    let private_key_path = temp_dir.join(format!("test_private_key_{unique_id}.pem"));
    let cert_path = temp_dir.join(format!("test_cert_{unique_id}.pem"));

    let mut rng = OsRng;
    let private_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();

    let private_pem = private_key
        .to_pkcs8_pem(LineEnding::LF)
        .unwrap()
        .to_string();
    fs::write(&private_key_path, private_pem.as_bytes()).unwrap();

    let signing_key =
        KeyPair::from_pkcs8_pem_and_sign_algo(&private_pem, &PKCS_RSA_SHA256).unwrap();

    let mut params = CertificateParams::new(vec!["test.example.com".to_string()]).unwrap();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CountryName, "US");
    dn.push(DnType::StateOrProvinceName, "Test");
    dn.push(DnType::OrganizationName, "Test Org");
    dn.push(DnType::CommonName, "test.example.com");
    params.distinguished_name = dn;

    let cert = params.self_signed(&signing_key).unwrap();
    let cert_pem = cert.pem();
    fs::write(&cert_path, cert_pem).unwrap();

    (private_key_path, cert_path)
}

fn create_test_h264_buffer(index: usize, is_i_frame: bool) -> gst::Buffer {
    create_test_h265_buffer(index, is_i_frame)
}

fn create_test_h265_buffer(index: usize, is_first_au: bool) -> gst::Buffer {
    fn append_h265_nal(data: &mut Vec<u8>, nal_type: u8, payload_seed: usize) {
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.push((nal_type << 1) & 0x7E);
        data.push(0x01);
        for j in 0..32 {
            data.push(((payload_seed + j) % 256) as u8);
        }
    }

    let mut data = Vec::new();

    if is_first_au {
        // HM-minimum parameter-set coverage
        append_h265_nal(&mut data, 32, index * 100 + 1); // VPS
        append_h265_nal(&mut data, 33, index * 100 + 2); // SPS
        append_h265_nal(&mut data, 34, index * 100 + 3); // PPS
        append_h265_nal(&mut data, 19, index * 100 + 4); // IDR_W_RADL (VCL)
    } else {
        append_h265_nal(&mut data, 1, index * 100 + 5); // TRAIL_R (VCL)
    }

    let mut buffer = gst::Buffer::from_slice(data);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        if !is_first_au {
            buffer_ref.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
    }
    buffer
}

fn cleanup_test_keys(private_key_path: &PathBuf, cert_path: &PathBuf) {
    let _ = fs::remove_file(private_key_path);
    let _ = fs::remove_file(cert_path);
}

#[test]
fn test_signer_verifier_sha256() {
    test_signer_verifier_with_hash("sha256");
}

#[test]
fn test_signer_verifier_sha384() {
    test_signer_verifier_with_hash("sha384");
}

#[test]
fn test_signer_verifier_sha512() {
    test_signer_verifier_with_hash("sha512");
}

#[test]
fn test_signer_verifier_sha1() {
    test_signer_verifier_with_hash("sha1");
}

#[test]
fn test_signer_verifier_sha224() {
    test_signer_verifier_with_hash("sha224");
}

fn hash_method_from_str(hash_method: &str) -> HashMethod {
    match hash_method {
        "sha1" => HashMethod::Sha1,
        "sha224" => HashMethod::Sha224,
        "sha256" => HashMethod::Sha256,
        "sha384" => HashMethod::Sha384,
        "sha512" => HashMethod::Sha512,
        _ => HashMethod::Sha256,
    }
}

fn test_signer_verifier_with_hash(hash_method: &str) {
    init();

    if !have_x265enc() {
        eprintln!("Skipping test: x265enc is not available");
        return;
    }

    let (private_key_path, cert_path) = create_test_keys();
    let key_store_path = cert_path.parent().unwrap().to_str().unwrap();

    let pipeline = gst::Pipeline::new();

    let videotestsrc = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 10i32)
        .build()
        .unwrap();

    let capsfilter1 = gst::ElementFactory::make("capsfilter").build().unwrap();
    let caps1 = gst::Caps::builder("video/x-raw")
        .field("framerate", gst::Fraction::new(30, 1))
        .build();
    capsfilter1.set_property("caps", caps1);

    let videoconvert = gst::ElementFactory::make("videoconvert").build().unwrap();

    let x265enc = gst::ElementFactory::make("x265enc")
        .property("key-int-max", 5i32)
        .build()
        .unwrap();

    let capsfilter2 = gst::ElementFactory::make("capsfilter").build().unwrap();
    let caps2 = gst::Caps::builder("video/x-h265")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    capsfilter2.set_property("caps", caps2);

    let signer = gst::ElementFactory::make("dscsigner")
        .property("hash-method", hash_method_from_str(hash_method))
        .property("private-key-path", private_key_path.to_str().unwrap())
        .property("public-key-uri", cert_path.to_str().unwrap())
        .property("substream-length", 5u32)
        .build()
        .unwrap();

    let verifier = gst::ElementFactory::make("dscverifier")
        .property("key-store-path", key_store_path)
        .build()
        .unwrap();

    let fakesink = gst::ElementFactory::make("fakesink").build().unwrap();

    pipeline
        .add_many([
            &videotestsrc,
            &capsfilter1,
            &videoconvert,
            &x265enc,
            &capsfilter2,
            &signer,
            &verifier,
            &fakesink,
        ])
        .unwrap();

    gst::Element::link_many([
        &videotestsrc,
        &capsfilter1,
        &videoconvert,
        &x265enc,
        &capsfilter2,
        &signer,
        &verifier,
        &fakesink,
    ])
    .unwrap();

    let bus = pipeline.bus().unwrap();
    let mut verification_count = 0;
    let mut verification_success = true;
    let mut pipeline_finished = false;

    pipeline.set_state(gst::State::Playing).unwrap();

    while !pipeline_finished {
        let msg = bus.timed_pop(gst::ClockTime::from_seconds(5));

        match msg {
            Some(msg) => {
                use gst::MessageView;

                match msg.view() {
                    MessageView::Eos(..) => {
                        println!("EOS received");
                        pipeline_finished = true;
                    }
                    MessageView::Error(err) => {
                        panic!(
                            "Error from {:?}: {} ({:?})",
                            err.src().map(|s| s.path_string()),
                            err.error(),
                            err.debug()
                        );
                    }
                    MessageView::Element(element_msg) => {
                        if let Some(structure) = element_msg.structure()
                            && structure.name() == "dsc-verification-result"
                            && let Ok(verified) = structure.get::<bool>("verified")
                        {
                            verification_count += 1;
                            verification_success &= verified;

                            if verified {
                                println!("Verification #{} succeeded", verification_count);
                            } else {
                                eprintln!("Verification #{} failed", verification_count);
                            }
                        }
                    }
                    _ => {}
                }
            }
            None => {
                eprintln!("Timeout waiting for message");
                pipeline_finished = true;
            }
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();

    assert_eq!(
        verification_count, 2,
        "Expected 2 verification messages (one per GOP), got {}",
        verification_count
    );
    assert!(verification_success, "All verifications should succeed");

    cleanup_test_keys(&private_key_path, &cert_path);
}

#[test]
fn test_signer_without_key() {
    init();

    let mut h = gst_check::Harness::new("dscsigner");
    h.play();

    let caps = gst::Caps::builder("video/x-h265")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    h.set_src_caps(caps);

    let buffer1 = create_test_h264_buffer(0, true);
    let mut got_error = h.push(buffer1).is_err();

    for i in 1..5 {
        let buffer = create_test_h264_buffer(i, false);
        got_error |= h.push(buffer).is_err();
    }

    assert!(
        got_error,
        "Expected signer to fail without private key when signature is required"
    );
}

#[test]
fn test_verifier_without_signature_meta() {
    init();

    let (_, cert_path) = create_test_keys();
    let key_store_path = cert_path.parent().unwrap().to_str().unwrap();

    let mut h = gst_check::Harness::new("dscverifier");
    h.play();

    let verifier = h.element().unwrap();
    verifier.set_property("key-store-path", key_store_path);

    let caps = gst::Caps::builder("video/x-h265")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    h.set_src_caps(caps);

    let buffer = create_test_h264_buffer(0, true);

    let result = h.push(buffer);
    assert!(
        result.is_ok(),
        "First I-frame without signature should pass"
    );

    cleanup_test_keys(&PathBuf::new(), &cert_path);
}

#[test]
fn test_signature_meta_preservation() {
    init();

    let (private_key_path, cert_path) = create_test_keys();

    let mut h = gst_check::Harness::new("dscsigner");

    let caps = gst::Caps::builder("video/x-h265")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    h.set_src_caps(caps);

    let signer = h.element().unwrap();
    signer.set_property("hash-method", HashMethod::Sha256);
    signer.set_property("private-key-path", private_key_path.to_str().unwrap());
    signer.set_property("public-key-uri", cert_path.to_str().unwrap());
    signer.set_property("substream-length", 5u32);

    h.play();

    let buffer1 = create_test_h264_buffer(0, true);
    h.push(buffer1).unwrap();

    for i in 1..5 {
        let buffer = create_test_h264_buffer(i, false);
        h.push(buffer).unwrap();
    }

    h.push_event(gst::event::Eos::new());

    let mut signed_buffer_with_meta = None;
    for _ in 0..5 {
        let signed_buffer = h.pull().unwrap();
        if signed_buffer
            .meta::<gst_video::video_meta::VideoDSCVerificationMeta>()
            .is_some()
        {
            signed_buffer_with_meta = Some(signed_buffer);
            break;
        }
    }

    // Verify signature meta exists on at least one buffer
    let signed_buffer =
        signed_buffer_with_meta.expect("Should have at least one buffer with signature meta");

    let has_signature_meta = signed_buffer.foreach_meta(|meta| {
        if meta.api().name() == "GstVideoDSCVerificationMeta" {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    });

    assert!(has_signature_meta, "Buffer should have signature meta");

    // Test that signature meta is preserved when copying
    let copied_buffer = signed_buffer.copy_deep().unwrap();
    let copied_has_signature_meta = copied_buffer.foreach_meta(|meta| {
        if meta.api().name() == "GstVideoDSCVerificationMeta" {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    });

    assert!(
        copied_has_signature_meta,
        "Copied buffer should have signature meta"
    );

    cleanup_test_keys(&private_key_path, &cert_path);
}

#[test]
fn test_multiple_hash_methods_sequential() {
    init();

    if !have_x265enc() {
        eprintln!("Skipping test: x265enc is not available");
        return;
    }

    let (private_key_path, cert_path) = create_test_keys();
    let key_store_path = cert_path.parent().unwrap().to_str().unwrap();
    let hash_methods = ["sha1", "sha224", "sha256", "sha384", "sha512"];

    for hash_method in &hash_methods {
        println!("Testing hash method: {}", hash_method);

        let pipeline = gst::Pipeline::new();

        let videotestsrc = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 10i32)
            .build()
            .unwrap();

        let capsfilter1 = gst::ElementFactory::make("capsfilter").build().unwrap();
        capsfilter1.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("framerate", gst::Fraction::new(30, 1))
                .build(),
        );

        let videoconvert = gst::ElementFactory::make("videoconvert").build().unwrap();
        let x265enc = gst::ElementFactory::make("x265enc")
            .property("key-int-max", 5i32)
            .build()
            .unwrap();

        let capsfilter2 = gst::ElementFactory::make("capsfilter").build().unwrap();
        capsfilter2.set_property(
            "caps",
            gst::Caps::builder("video/x-h265")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        );

        let signer = gst::ElementFactory::make("dscsigner")
            .property("hash-method", hash_method_from_str(hash_method))
            .property("private-key-path", private_key_path.to_str().unwrap())
            .property("public-key-uri", cert_path.to_str().unwrap())
            .build()
            .unwrap();

        let verifier = gst::ElementFactory::make("dscverifier")
            .property("key-store-path", key_store_path)
            .build()
            .unwrap();

        let fakesink = gst::ElementFactory::make("fakesink").build().unwrap();

        pipeline
            .add_many([
                &videotestsrc,
                &capsfilter1,
                &videoconvert,
                &x265enc,
                &capsfilter2,
                &signer,
                &verifier,
                &fakesink,
            ])
            .unwrap();
        gst::Element::link_many([
            &videotestsrc,
            &capsfilter1,
            &videoconvert,
            &x265enc,
            &capsfilter2,
            &signer,
            &verifier,
            &fakesink,
        ])
        .unwrap();

        let bus = pipeline.bus().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        let mut pipeline_finished = false;
        while !pipeline_finished {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(5)) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(..) => pipeline_finished = true,
                    MessageView::Error(err) => {
                        panic!(
                            "Error in {}: {} ({:?})",
                            hash_method,
                            err.error(),
                            err.debug()
                        );
                    }
                    _ => {}
                }
            } else {
                pipeline_finished = true;
            }
        }

        pipeline.set_state(gst::State::Null).unwrap();
    }

    cleanup_test_keys(&private_key_path, &cert_path);
}

#[test]
fn test_signer_verifier_h265_hm_minimum_roundtrip() {
    init();

    let (private_key_path, cert_path) = create_test_keys();
    let key_store_path = cert_path.parent().unwrap().to_str().unwrap();

    let mut signer_h = gst_check::Harness::new("dscsigner");
    let signer = signer_h.element().unwrap();
    signer.set_property("hash-method", HashMethod::Sha256);
    signer.set_property("private-key-path", private_key_path.to_str().unwrap());
    signer.set_property("public-key-uri", cert_path.to_str().unwrap());
    // Use substream-length == total buffers: one substream, one verification meta.
    // Cross-substream chain-digest behaviour is a separate pre-existing concern.
    signer.set_property("substream-length", 5u32);

    let h265_caps = gst::Caps::builder("video/x-h265")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();

    signer_h.set_src_caps(h265_caps.clone());
    signer_h.play();

    let mut signed_buffers = Vec::new();
    for i in 0..5 {
        let buffer = create_test_h265_buffer(i, i == 0);
        signer_h.push(buffer).unwrap();
        let signed = signer_h.pull().unwrap();
        signed_buffers.push(signed);
    }

    let verification_meta_count = signed_buffers
        .iter()
        .filter(|b| {
            b.meta::<gst_video::video_meta::VideoDSCVerificationMeta>()
                .is_some()
        })
        .count();

    assert_eq!(
        verification_meta_count, 1,
        "Expected 1 verification meta for 5 buffers with substream-length=5"
    );

    let mut verifier_h = gst_check::Harness::new("dscverifier");
    let verifier = verifier_h.element().unwrap();
    verifier.set_property("key-store-path", key_store_path);
    verifier.set_property("fail-on-verification-error", true);

    verifier_h.set_src_caps(h265_caps);
    verifier_h.play();

    for buffer in signed_buffers {
        let result = verifier_h.push(buffer);
        assert!(result.is_ok(), "Verifier rejected a signed H.265 buffer");
    }

    cleanup_test_keys(&private_key_path, &cert_path);
}

#[test]
fn test_verifier_forwards_queued_buffers_at_eos() {
    init();

    let mut h = gst_check::Harness::new("dscverifier");

    let caps = gst::Caps::builder("video/x-h265")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    h.set_src_caps(caps);
    h.play();

    // In buffering mode a GOP is held back until its verification metadata
    // arrives. A GOP that never gets one (truncated stream) must still be
    // forwarded when EOS is received, instead of being dropped.
    let num_buffers = 3;
    for i in 0..num_buffers {
        let mut buffer = create_test_h265_buffer(i, i == 0);

        let dsc_init = gst_video::H274DigitallySignedContentInitialization::new(
            u8::from(HashMethod::Sha256),
            None,
            Some("file:///nonexistent.crt"),
        );
        let dsc_selection = gst_video::H274DigitallySignedContentSelection::new(0);

        let buffer_ref = buffer.get_mut().unwrap();
        gst_video::video_meta::VideoDSCInitializationMeta::add(buffer_ref, &dsc_init);
        gst_video::video_meta::VideoDSCSelectionMeta::add(buffer_ref, &dsc_selection);

        h.push(buffer).unwrap();
    }

    assert!(
        h.push_event(gst::event::Eos::new()),
        "EOS should be forwarded downstream"
    );

    for i in 0..num_buffers {
        assert!(
            h.pull().is_ok(),
            "Queued buffer {i} should be forwarded before EOS"
        );
    }
}

#[test]
fn test_verifier_forwards_buffers_when_max_buffering_time_exceeded() {
    init();

    let mut h = gst_check::Harness::new("dscverifier");

    let caps = gst::Caps::builder("video/x-h265")
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
    h.set_src_caps(caps);

    let verifier = h.element().unwrap();
    verifier.set_property("max-buffering-time", 250_000_000u64);
    h.play();

    // Three 100ms buffers exceed the 250ms cap without a verification meta.
    // The queued buffers must be forwarded instead of growing without bound.
    for i in 0..3 {
        let mut buffer = create_test_h265_buffer(i, i == 0);

        {
            let buffer_ref = buffer.get_mut().unwrap();
            buffer_ref.set_pts(gst::ClockTime::from_mseconds(100 * i as u64));
            buffer_ref.set_duration(gst::ClockTime::from_mseconds(100));

            if i == 0 {
                let dsc_init = gst_video::H274DigitallySignedContentInitialization::new(
                    u8::from(HashMethod::Sha256),
                    None,
                    Some("file:///nonexistent.crt"),
                );
                let dsc_selection = gst_video::H274DigitallySignedContentSelection::new(0);

                gst_video::video_meta::VideoDSCInitializationMeta::add(buffer_ref, &dsc_init);
                gst_video::video_meta::VideoDSCSelectionMeta::add(buffer_ref, &dsc_selection);
            }
        }

        h.push(buffer).unwrap();
    }

    for i in 0..3 {
        assert!(
            h.try_pull().is_some(),
            "Buffer {i} should be forwarded after max-buffering-time is exceeded"
        );
    }
}
