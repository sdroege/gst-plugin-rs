// Copyright (C) 2025 Matthew Waters <matthew@centricular.com>
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct Test {
    pipeline: gst::Pipeline,
    local_send: gst::Element,
    local_recv: gst::Element,
    remote_send: gst::Element,
    remote_recv: gst::Element,
}

static WEBRTC_ID: AtomicUsize = AtomicUsize::new(0);

impl Test {
    fn new() -> Self {
        let local_id = WEBRTC_ID.fetch_add(1, Ordering::SeqCst);
        let remote_id = WEBRTC_ID.fetch_add(1, Ordering::SeqCst);
        let ret = Self {
            pipeline: gst::Pipeline::new(),
            local_send: gst::ElementFactory::make("webrtcsend")
                .property("id", local_id.to_string())
                .build()
                .unwrap(),
            local_recv: gst::ElementFactory::make("webrtcrecv")
                .property("id", local_id.to_string())
                .build()
                .unwrap(),
            remote_send: gst::ElementFactory::make("webrtcsend")
                .property("id", remote_id.to_string())
                .build()
                .unwrap(),
            remote_recv: gst::ElementFactory::make("webrtcrecv")
                .property("id", remote_id.to_string())
                .build()
                .unwrap(),
        };
        ret.pipeline
            .add_many([
                &ret.local_send,
                &ret.local_recv,
                &ret.remote_send,
                &ret.remote_recv,
            ])
            .unwrap();
        ret
    }
}

fn init() {
    gst::init().unwrap();
    gstrsrtp::plugin_register_static().unwrap();
    gstrswebrtcbin2::plugin_register_static().unwrap();
}

#[test]
fn construct_within_tokio_runtime() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            init();
            let test = Test::new();
            test.pipeline.set_state(gst::State::Playing).unwrap();
            negotiate_trickle_ice(&test);
        })
}

fn negotiate_trickle_ice(test: &Test) -> (String, String) {
    let local_session = test.local_send.property::<gst::Object>("session");
    let remote_session = test.remote_send.property::<gst::Object>("session");
    let (tx, rx) = std::sync::mpsc::sync_channel(1);

    local_session.connect_closure("on-ice-candidate", true, {
        let remote_session = remote_session.clone();
        glib::closure!(move |_session: &glib::Object,
                             mlineindex: u32,
                             mid: Option<&str>,
                             candidate: String| {
            remote_session.emit_by_name::<()>(
                "add-ice-candidate",
                &[&mlineindex, &mid, &candidate, &None::<gst::Promise>],
            );
        })
    });

    remote_session.connect_closure("on-ice-candidate", true, {
        let local_session = local_session.clone();
        glib::closure!(move |_session: &glib::Object,
                             mlineindex: u32,
                             mid: Option<&str>,
                             candidate: String| {
            local_session.emit_by_name::<()>(
                "add-ice-candidate",
                &[&mlineindex, &mid, &candidate, &None::<gst::Promise>],
            );
        })
    });

    local_session.clone().emit_by_name::<()>(
        "create-offer",
        &[
            &None::<gst::Structure>,
            &Some(gst::Promise::with_change_func(move |reply| {
                let reply = reply.unwrap().unwrap();
                let sdp = reply.get::<String>("sdp").unwrap();
                tx.send(sdp.clone()).unwrap();
                let local_session_clone = local_session.clone();
                local_session.emit_by_name::<()>(
                    "set-local-description",
                    &[
                        &"offer",
                        &sdp,
                        &gst::Promise::with_change_func(move |_reply| {
                            let _sdp =
                                local_session_clone.property::<String>("pending-local-description");
                            assert!(
                                local_session_clone
                                    .property::<Option<String>>("pending-remote-description")
                                    .is_none()
                            );
                            assert!(
                                local_session_clone
                                    .property::<Option<String>>("current-local-description")
                                    .is_none()
                            );
                            assert!(
                                local_session_clone
                                    .property::<Option<String>>("current-remote-description")
                                    .is_none()
                            );
                        }),
                    ],
                );
                remote_session.clone().emit_by_name::<()>(
                    "set-remote-description",
                    &[
                        &"offer",
                        &sdp,
                        &Some(gst::Promise::with_change_func(move |_reply| {
                            let _sdp =
                                remote_session.property::<String>("pending-remote-description");
                            assert!(
                                remote_session
                                    .property::<Option<String>>("pending-local-description")
                                    .is_none()
                            );
                            assert!(
                                remote_session
                                    .property::<Option<String>>("current-local-description")
                                    .is_none()
                            );
                            assert!(
                                remote_session
                                    .property::<Option<String>>("current-remote-description")
                                    .is_none()
                            );
                            println!("remote remote description set");
                            remote_session.clone().emit_by_name::<()>(
                                "create-answer",
                                &[
                                    &None::<gst::Structure>,
                                    &Some(gst::Promise::with_change_func(move |reply| {
                                        println!("remote answer created");
                                        let reply = reply.unwrap().unwrap();
                                        let sdp = reply.get::<String>("sdp").unwrap();
                                        let remote_session_clone = remote_session.clone();
                                        remote_session.emit_by_name::<()>(
                                            "set-local-description",
                                            &[
                                                &"answer",
                                                &sdp,
                                                &gst::Promise::with_change_func(move |_reply| {
                                                    let _sdp = remote_session_clone
                                                        .property::<String>(
                                                            "current-local-description",
                                                        );
                                                    let _sdp = remote_session_clone
                                                        .property::<String>(
                                                            "current-remote-description",
                                                        );
                                                    assert!(
                                                        remote_session_clone
                                                            .property::<Option<String>>(
                                                                "pending-local-description"
                                                            )
                                                            .is_none()
                                                    );
                                                    assert!(
                                                        remote_session_clone
                                                            .property::<Option<String>>(
                                                                "pending-remote-description"
                                                            )
                                                            .is_none()
                                                    );
                                                }),
                                            ],
                                        );
                                        let local_session_clone = local_session.clone();
                                        local_session.emit_by_name::<()>(
                                            "set-remote-description",
                                            &[
                                                &"answer",
                                                &sdp.clone(),
                                                &Some(gst::Promise::with_change_func(
                                                    move |_reply| {
                                                        let _sdp = local_session_clone
                                                            .property::<String>(
                                                                "current-remote-description",
                                                            );
                                                        let _sdp = local_session_clone
                                                            .property::<String>(
                                                                "current-local-description",
                                                            );
                                                        assert!(
                                                            local_session_clone
                                                                .property::<Option<String>>(
                                                                    "pending-local-description"
                                                                )
                                                                .is_none()
                                                        );
                                                        assert!(
                                                            local_session_clone
                                                                .property::<Option<String>>(
                                                                    "pending-remote-description"
                                                                )
                                                                .is_none()
                                                        );
                                                        tx.send(sdp).unwrap();
                                                    },
                                                )),
                                            ],
                                        );
                                    })),
                                ],
                            )
                        })),
                    ],
                );
            })),
        ],
    );
    let offer = rx.recv().unwrap();
    let answer = rx.recv().unwrap();
    (offer, answer)
}

fn negotiate_without_trickle(test: &Test) -> (String, String) {
    let local_session = test.local_send.property::<gst::Object>("session");
    let remote_session = test.remote_send.property::<gst::Object>("session");
    let (tx, rx) = std::sync::mpsc::sync_channel(1);

    local_session
        .clone()
        .connect_closure("on-ice-candidate", true, {
            let local_session = local_session.clone();
            let remote_session = remote_session.clone();
            let tx = tx.clone();
            glib::closure!(move |_session: &glib::Object,
                                 _mlineindex: u32,
                                 _mid: Option<&str>,
                                 candidate: String| {
                if candidate.is_empty() {
                    let remote_session = remote_session.clone();
                    let sdp = local_session.property::<String>("pending-local-description");
                    tx.send(sdp.clone()).unwrap();
                    assert!(
                        local_session
                            .property::<Option<String>>("pending-remote-description")
                            .is_none()
                    );
                    assert!(
                        local_session
                            .property::<Option<String>>("current-local-description")
                            .is_none()
                    );
                    assert!(
                        local_session
                            .property::<Option<String>>("current-remote-description")
                            .is_none()
                    );
                    remote_session.clone().emit_by_name::<()>(
                        "set-remote-description",
                        &[
                            &"offer",
                            &sdp,
                            &Some(gst::Promise::with_change_func(move |_reply| {
                                println!("remote remote description set");
                                remote_session.clone().emit_by_name::<()>(
                                    "create-answer",
                                    &[
                                        &None::<gst::Structure>,
                                        &Some(gst::Promise::with_change_func(move |reply| {
                                            println!("remote answer created");
                                            let reply = reply.unwrap().unwrap();
                                            let sdp = reply.get::<String>("sdp").unwrap();
                                            let remote_session_clone = remote_session.clone();
                                            remote_session.emit_by_name::<()>(
                                                "set-local-description",
                                                &[
                                                    &"answer",
                                                    &sdp,
                                                    &gst::Promise::with_change_func(
                                                        move |_reply| {
                                                            let _sdp = remote_session_clone
                                                                .property::<String>(
                                                                    "current-local-description",
                                                                );
                                                            let _sdp = remote_session_clone
                                                                .property::<String>(
                                                                    "current-remote-description",
                                                                );
                                                            assert!(
                                                                remote_session_clone
                                                                    .property::<Option<String>>(
                                                                        "pending-local-description"
                                                                    )
                                                                    .is_none()
                                                            );
                                                            assert!(
                                                                remote_session_clone
                                                                    .property::<Option<String>>(
                                                                        "pending-remote-description"
                                                                    )
                                                                    .is_none()
                                                            );
                                                        },
                                                    ),
                                                ],
                                            );
                                        })),
                                    ],
                                )
                            })),
                        ],
                    );
                }
            })
        });
    remote_session
        .clone()
        .connect_closure("on-ice-candidate", true, {
            let local_session = local_session.clone();
            let remote_session = remote_session.clone();
            let tx = tx.clone();
            glib::closure!(move |_session: &glib::Object,
                                 _mlineindex: u32,
                                 _mid: Option<&str>,
                                 candidate: String| {
                if candidate.is_empty() {
                    let tx = tx.clone();
                    let local_session_clone = local_session.clone();
                    let sdp = remote_session.property::<String>("current-local-description");
                    assert!(
                        remote_session
                            .property::<Option<String>>("pending-local-description")
                            .is_none()
                    );
                    assert!(
                        remote_session
                            .property::<Option<String>>("pending-remote-description")
                            .is_none()
                    );
                    assert!(
                        remote_session
                            .property::<Option<String>>("current-remote-description")
                            .is_some()
                    );
                    local_session.emit_by_name::<()>(
                        "set-remote-description",
                        &[
                            &"answer",
                            &sdp.clone(),
                            &Some(gst::Promise::with_change_func(move |_reply| {
                                let _sdp = local_session_clone
                                    .property::<String>("current-remote-description");
                                let _sdp = local_session_clone
                                    .property::<String>("current-local-description");
                                assert!(
                                    local_session_clone
                                        .property::<Option<String>>("pending-local-description")
                                        .is_none()
                                );
                                assert!(
                                    local_session_clone
                                        .property::<Option<String>>("pending-remote-description")
                                        .is_none()
                                );
                                tx.send(sdp).unwrap();
                            })),
                        ],
                    );
                }
            })
        });
    local_session.clone().emit_by_name::<()>(
        "create-offer",
        &[
            &None::<gst::Structure>,
            &Some(gst::Promise::with_change_func(move |reply| {
                let reply = reply.unwrap().unwrap();
                let sdp = reply.get::<String>("sdp").unwrap();
                local_session.emit_by_name::<()>(
                    "set-local-description",
                    &[&"offer", &sdp, &None::<gst::Promise>],
                )
            })),
        ],
    );
    let offer = rx.recv().unwrap();
    let answer = rx.recv().unwrap();
    (offer, answer)
}

const L16_CAPS_STR: &str = "application/x-rtp, payload=11, media=audio, encoding-name=L16, clock-rate=44100, ssrc=(uint)3484078952";

fn add_audio_test_src_harness(h: &mut gst_check::Harness, ssrc: u32) {
    let mut caps = gst::Caps::from_str(L16_CAPS_STR).unwrap();
    if ssrc != 0 {
        let caps = caps.make_mut();
        caps.set("ssrc", ssrc);
    }
    h.add_src_parse(
        "audiotestsrc is-live=true ! rtpL16pay ! capsfilter name=capsfilter ! identity",
        true,
    );
    let capsfilter = h
        .src_harness_mut()
        .unwrap()
        .element()
        .unwrap()
        .downcast::<gst::Bin>()
        .unwrap()
        .by_name("capsfilter")
        .unwrap();
    capsfilter.set_property("caps", caps.clone());
    h.set_src_caps(caps);
}

#[test]
fn audio_trickle_ice() {
    init();
    let test = Test::new();
    let mut h = gst_check::Harness::with_element(&test.local_send, Some("sink_0"), None);
    h.play();
    test.pipeline.set_state(gst::State::Playing).unwrap();
    add_audio_test_src_harness(&mut h, 0xDEADBEEF);
    let (offer, answer) = negotiate_trickle_ice(&test);
    let offer = sdp_types::Session::parse(offer.as_bytes()).unwrap();
    assert_eq!(offer.medias.len(), 1);
    let answer = sdp_types::Session::parse(answer.as_bytes()).unwrap();
    assert_eq!(answer.medias.len(), 1);

    let (tx, rx) = std::sync::mpsc::sync_channel(2);

    test.remote_recv.connect_pad_added(move |element, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        let mut h = gst_check::Harness::with_element(element, None, Some(pad.name().as_str()));
        h.add_sink_parse("fakesink async=false sync=false");
        tx.send(h).unwrap();
    });

    loop {
        h.push_from_src().unwrap();
        if let Ok(_data) = rx.try_recv() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn audio_non_trickle() {
    init();
    let test = Test::new();
    let mut h = gst_check::Harness::with_element(&test.local_send, Some("sink_0"), None);
    h.play();
    test.pipeline.set_state(gst::State::Playing).unwrap();
    add_audio_test_src_harness(&mut h, 0xDEADBEEF);
    let (offer, answer) = negotiate_without_trickle(&test);
    let offer = sdp_types::Session::parse(offer.as_bytes()).unwrap();
    assert_eq!(offer.medias.len(), 1);
    let answer = sdp_types::Session::parse(answer.as_bytes()).unwrap();
    assert_eq!(answer.medias.len(), 1);

    let (tx, rx) = std::sync::mpsc::sync_channel(2);

    test.remote_recv.connect_pad_added(move |element, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        let mut h = gst_check::Harness::with_element(element, None, Some(pad.name().as_str()));
        h.add_sink_parse("fakesink async=false sync=false");
        tx.send(h).unwrap();
    });

    loop {
        h.push_from_src().unwrap();
        if let Ok(_data) = rx.try_recv() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

const VP8_CAPS_STR: &str = "application/x-rtp, payload=96, media=video, encoding-name=VP8, clock-rate=90000, ssrc=(uint)3484078951";

fn add_video_test_src_harness(h: &mut gst_check::Harness, ssrc: u32) {
    let mut caps = gst::Caps::from_str(VP8_CAPS_STR).unwrap();
    if ssrc != 0 {
        let caps = caps.make_mut();
        caps.set("ssrc", ssrc);
    }
    h.add_src_parse(
        "videotestsrc is-live=true ! vp8enc deadline=1 ! rtpvp8pay ! capsfilter name=capsfilter ! identity",
        true,
    );
    let capsfilter = h
        .src_harness_mut()
        .unwrap()
        .element()
        .unwrap()
        .downcast::<gst::Bin>()
        .unwrap()
        .by_name("capsfilter")
        .unwrap();
    capsfilter.set_property("caps", caps.clone());
    h.set_src_caps(caps);
}

#[test]
fn audio_video_bundle() {
    init();

    if gst::Plugin::load_by_name("vpx").is_err() {
        eprintln!("No vpx plugin available, skipping video-related test");
        return;
    }
    let test = Test::new();
    let mut audio = gst_check::Harness::with_element(&test.local_send, Some("sink_0"), None);
    audio.play();
    test.pipeline.set_state(gst::State::Playing).unwrap();
    add_audio_test_src_harness(&mut audio, 0xDEADBEEF);
    let mut video = gst_check::Harness::with_element(&test.local_send, Some("sink_1"), None);
    video.play();
    add_video_test_src_harness(&mut video, 0x12345678);
    let (offer, answer) = negotiate_trickle_ice(&test);
    let offer = sdp_types::Session::parse(offer.as_bytes()).unwrap();
    assert_eq!(offer.medias.len(), 2);
    let answer = sdp_types::Session::parse(answer.as_bytes()).unwrap();
    assert_eq!(answer.medias.len(), 2);

    let (tx, rx) = std::sync::mpsc::sync_channel(2);

    test.remote_recv.connect_pad_added(move |element, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        let mut h = gst_check::Harness::with_element(element, None, Some(pad.name().as_str()));
        h.add_sink_parse("fakesink async=false sync=false");
        h.play();
        tx.send(h).unwrap();
    });

    // need to keep the srcpad harness alive otherwise flushing will be returned after the first
    // srcpad harness is dropped and cause the test to never end.
    let mut srcpad_harnesses = vec![];
    loop {
        audio.push_from_src().unwrap();
        video.push_from_src().unwrap();
        if let Ok(srcpad_harness) = rx.try_recv() {
            srcpad_harnesses.push(srcpad_harness);
            if srcpad_harnesses.len() >= 2 {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
