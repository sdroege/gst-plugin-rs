//
// Copyright (C) 2021 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gio::prelude::*;
use gst::gst_info;
use gst::prelude::*;
use once_cell::sync::Lazy;
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "hlssink3-test",
        gst::DebugColorFlags::empty(),
        Some("Flex HLS sink test"),
    )
});

macro_rules! try_or_pause {
    ($l:expr) => {
        match $l {
            Ok(v) => v,
            Err(err) => {
                eprintln!("Skipping Test: {:?}", err);
                return Ok(());
            }
        }
    };
}

macro_rules! try_create_element {
    ($l:expr, $n:expr) => {
        match gst::ElementFactory::find($l) {
            Some(factory) => factory.create(Some($n)).unwrap(),
            None => {
                eprintln!("Could not find {} ({}) plugin, skipping test", $l, $n);
                return Ok(());
            }
        }
    };
    ($l:expr) => {{
        let alias: String = format!("test_{}", $l);
        try_create_element!($l, <std::string::String as AsRef<str>>::as_ref(&alias))
    }};
}

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gsthlssink3::plugin_register_static().expect("hlssink3 test");
    });
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum HlsSinkEvent {
    GetPlaylistStream(String),
    GetFragmentStream(String),
    DeleteFragment(String),
}

/// Represents a HLS playlist file that writes to a shared string.
///
/// Only used in tests to check the resulting playlist content without the need to write
/// to actual filesystem.
struct MemoryPlaylistFile {
    /// Reference to the content handler.
    handler: Arc<Mutex<String>>,
}

impl MemoryPlaylistFile {
    /// Whenever we know the file will be overridden we call this method to clear the contents.
    fn clear_content(&self) {
        let mut string = self.handler.lock().unwrap();
        string.clear();
    }
}

impl Write for MemoryPlaylistFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // We don't need to handle errors here as this is used only in tests
        let value = std::str::from_utf8(buf).unwrap();
        let mut string = self.handler.lock().unwrap();
        string.push_str(value);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // We are writing to a `String` object, no need to flush content
        Ok(())
    }
}

#[test]
fn test_hlssink3_element_with_video_content() -> Result<(), ()> {
    init();

    const BUFFER_NB: i32 = 250;

    let pipeline = gst::Pipeline::new(Some("video_pipeline"));

    let video_src = try_create_element!("videotestsrc");
    video_src.set_property("is-live", true);
    video_src.set_property("num-buffers", BUFFER_NB);

    let x264enc = try_create_element!("x264enc");
    let h264parse = try_create_element!("h264parse");

    let hlssink3 = gst::ElementFactory::make("hlssink3", Some("test_hlssink3"))
        .expect("Must be able to instantiate hlssink3");
    hlssink3.set_property("target-duration", 2u32);
    hlssink3.set_property("playlist-length", 2u32);
    hlssink3.set_property("max-files", 2u32);

    let (hls_events_sender, hls_events_receiver) = mpsc::sync_channel(20);
    let playlist_content = Arc::new(Mutex::new(String::from("")));

    hlssink3.connect("get-playlist-stream", false, {
        let hls_events_sender = hls_events_sender.clone();
        let playlist_content = playlist_content.clone();
        move |args| {
            let location = args[1].get::<String>().expect("No location given");

            hls_events_sender
                .try_send(HlsSinkEvent::GetPlaylistStream(location))
                .expect("Send playlist event");

            // We need an owned type to pass to `gio::WriteOutputStream`
            let playlist = MemoryPlaylistFile {
                handler: Arc::clone(&playlist_content),
            };
            // Since here a new file will be created, the content is cleared
            playlist.clear_content();
            let output = gio::WriteOutputStream::new(playlist);
            Some(output.to_value())
        }
    });

    hlssink3.connect("get-fragment-stream", false, {
        let hls_events_sender = hls_events_sender.clone();
        move |args| {
            let location = args[1].get::<String>().expect("No location given");

            hls_events_sender
                .try_send(HlsSinkEvent::GetFragmentStream(location))
                .expect("Send fragment event");

            let stream = gio::MemoryOutputStream::new_resizable();
            Some(stream.to_value())
        }
    });

    hlssink3.connect("delete-fragment", false, move |args| {
        let location = args[1].get::<String>().expect("No location given");
        hls_events_sender
            .try_send(HlsSinkEvent::DeleteFragment(location))
            .expect("Send delete fragment event");
        Some(true.to_value())
    });

    try_or_pause!(pipeline.add_many(&[&video_src, &x264enc, &h264parse, &hlssink3,]));
    try_or_pause!(gst::Element::link_many(&[
        &video_src, &x264enc, &h264parse, &hlssink3
    ]));

    pipeline.set_state(gst::State::Playing).unwrap();

    gst_info!(
        CAT,
        "hlssink3_video_pipeline: waiting for {} buffers",
        BUFFER_NB
    );

    let mut eos = false;
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Error(..) => unreachable!(),
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
    assert!(eos);

    // Collect all events triggered during execution of the pipeline
    let mut actual_events = Vec::new();
    while let Ok(event) = hls_events_receiver.recv_timeout(Duration::from_millis(1)) {
        actual_events.push(event);
    }
    let expected_ordering_of_events = {
        use self::HlsSinkEvent::*;
        vec![
            GetFragmentStream("segment00000.ts".to_string()),
            GetPlaylistStream("playlist.m3u8".to_string()),
            GetFragmentStream("segment00001.ts".to_string()),
            GetPlaylistStream("playlist.m3u8".to_string()),
            GetFragmentStream("segment00002.ts".to_string()),
            GetPlaylistStream("playlist.m3u8".to_string()),
            DeleteFragment("segment00000.ts".to_string()),
            GetFragmentStream("segment00003.ts".to_string()),
            GetPlaylistStream("playlist.m3u8".to_string()),
            DeleteFragment("segment00001.ts".into()),
            GetFragmentStream("segment00004.ts".to_string()),
            GetPlaylistStream("playlist.m3u8".to_string()),
            DeleteFragment("segment00002.ts".to_string()),
            GetPlaylistStream("playlist.m3u8".to_string()),
        ]
    };
    assert_eq!(expected_ordering_of_events, actual_events);

    let contents = playlist_content.lock().unwrap();
    assert_eq!(
        r###"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:2
#EXT-X-MEDIA-SEQUENCE:4
#EXTINF:2,
segment00003.ts
#EXTINF:0.3,
segment00004.ts
"###,
        contents.to_string()
    );

    Ok(())
}

#[test]
fn test_hlssink3_element_with_audio_content() -> Result<(), ()> {
    init();

    const BUFFER_NB: i32 = 100;

    let pipeline = gst::Pipeline::new(Some("audio_pipeline"));

    let audio_src = try_create_element!("audiotestsrc");
    audio_src.set_property("is-live", true);
    audio_src.set_property("num-buffers", BUFFER_NB);

    let hls_avenc_aac = try_or_pause!(gst::ElementFactory::make(
        "avenc_aac",
        Some("hls_avenc_aac")
    ));
    let hlssink3 = gst::ElementFactory::make("hlssink3", Some("hlssink3"))
        .expect("Must be able to instantiate hlssink3");
    hlssink3.set_property("target-duration", 6u32);

    hlssink3.connect("get-playlist-stream", false, move |_args| {
        let stream = gio::MemoryOutputStream::new_resizable();
        Some(stream.to_value())
    });

    hlssink3.connect("get-fragment-stream", false, move |_args| {
        let stream = gio::MemoryOutputStream::new_resizable();
        Some(stream.to_value())
    });

    hlssink3.connect("delete-fragment", false, move |_| Some(true.to_value()));

    try_or_pause!(pipeline.add_many(&[&audio_src, &hls_avenc_aac, &hlssink3,]));
    try_or_pause!(gst::Element::link_many(&[
        &audio_src,
        &hls_avenc_aac,
        &hlssink3
    ]));

    pipeline.set_state(gst::State::Playing).unwrap();

    gst_info!(CAT, "audio_pipeline: waiting for {} buffers", BUFFER_NB);

    let mut eos = false;
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Error(..) => unreachable!(),
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
    assert!(eos);
    Ok(())
}

#[test]
fn test_hlssink3_write_correct_playlist_content() -> Result<(), ()> {
    init();

    const BUFFER_NB: i32 = 50;

    let pipeline = gst::Pipeline::new(Some("video_pipeline"));

    let video_src = try_create_element!("videotestsrc");
    video_src.set_property("is-live", true);
    video_src.set_property("num-buffers", BUFFER_NB);

    let x264enc = try_create_element!("x264enc");
    let h264parse = try_create_element!("h264parse");

    let hlssink3 = gst::ElementFactory::make("hlssink3", Some("test_hlssink3"))
        .expect("Must be able to instantiate hlssink3");
    hlssink3.set_properties(&[
        ("location", &"/www/media/segments/my-own-filename-%03d.ts"),
        ("playlist-location", &"/www/media/main.m3u8"),
        ("playlist-root", &"segments"),
    ]);

    let (hls_events_sender, hls_events_receiver) = mpsc::sync_channel(20);
    let playlist_content = Arc::new(Mutex::new(String::from("")));

    hlssink3.connect("get-playlist-stream", false, {
        let hls_events_sender = hls_events_sender.clone();
        let playlist_content = playlist_content.clone();
        move |args| {
            let location = args[1].get::<String>().expect("No location given");

            hls_events_sender
                .try_send(HlsSinkEvent::GetPlaylistStream(location))
                .expect("Send playlist event");

            // We need an owned type to pass to `gio::WriteOutputStream`
            let playlist = MemoryPlaylistFile {
                handler: Arc::clone(&playlist_content),
            };
            // Since here a new file will be created, the content is cleared
            playlist.clear_content();
            let output = gio::WriteOutputStream::new(playlist);
            Some(output.to_value())
        }
    });

    hlssink3.connect("get-fragment-stream", false, {
        let hls_events_sender = hls_events_sender.clone();
        move |args| {
            let location = args[1].get::<String>().expect("No location given");

            hls_events_sender
                .try_send(HlsSinkEvent::GetFragmentStream(location))
                .expect("Send fragment event");

            let stream = gio::MemoryOutputStream::new_resizable();
            Some(stream.to_value())
        }
    });

    hlssink3.connect("delete-fragment", false, move |args| {
        let location = args[1].get::<String>().expect("No location given");
        hls_events_sender
            .try_send(HlsSinkEvent::DeleteFragment(location))
            .expect("Send delete fragment event");
        Some(true.to_value())
    });

    try_or_pause!(pipeline.add_many(&[&video_src, &x264enc, &h264parse, &hlssink3,]));
    try_or_pause!(gst::Element::link_many(&[
        &video_src, &x264enc, &h264parse, &hlssink3
    ]));

    pipeline.set_state(gst::State::Playing).unwrap();

    gst_info!(
        CAT,
        "hlssink3_video_pipeline: waiting for {} buffers",
        BUFFER_NB
    );

    let mut eos = false;
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Error(..) => unreachable!(),
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
    assert!(eos);

    // Collect all events triggered during execution of the pipeline
    let mut actual_events = Vec::new();
    while let Ok(event) = hls_events_receiver.recv_timeout(Duration::from_millis(1)) {
        actual_events.push(event);
    }
    let expected_ordering_of_events = {
        use self::HlsSinkEvent::*;
        vec![
            GetFragmentStream("/www/media/segments/my-own-filename-000.ts".to_string()),
            GetPlaylistStream("/www/media/main.m3u8".to_string()),
            GetPlaylistStream("/www/media/main.m3u8".to_string()),
        ]
    };
    assert_eq!(expected_ordering_of_events, actual_events);

    let contents = playlist_content.lock().unwrap();
    assert_eq!(
        r###"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:15
#EXT-X-MEDIA-SEQUENCE:1
#EXTINF:1.633,
segments/my-own-filename-000.ts
"###,
        contents.to_string()
    );

    Ok(())
}
