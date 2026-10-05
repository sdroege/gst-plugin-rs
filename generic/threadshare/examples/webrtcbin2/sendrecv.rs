// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
// Copyright (C) 2026 François Laignel <francois@centricular.com>
//
// SPDX-License-Identifier: MIT

// ## Instructions
//
// * Open https://webrtc.gstreamer.net/#data-channel=0 in a browser
// * In the "getUserMedia constraints being used" box, enter: {"video":false,"audio":true}
//
// ## Remote offer
//
// * Run the example with:
//   cargo r --example ts-webrtcbin2-sendrecv
// * On the webpage, in the "Enter id" text box, enter the id displayed on the command line. Ex.:
//   > Registering id 1743 with server
//
// ## Local offer
//
// * Run the example with:
//   cargo r --example ts-webrtcbin2-sendrecv -- --peer-id __id__
//
//   replacing '__id__' by the value of "Our id is ..." from the web page.
//
// Current limitations:
//
// * data channel is not supported.

use std::sync::{Arc, Mutex, Weak};

use futures::channel::{mpsc, oneshot};
use futures::prelude::*;

use async_tungstenite::tungstenite::{Error as WsError, Message as WsMessage};

use anyhow::{Context, anyhow, bail};

use clap::Parser;

use rand::prelude::*;

use gst::glib;
use gst::prelude::*;

const STUN_SERVER: &str = "stun://stun.l.google.com:19302";

// upgrade weak reference or return
#[macro_export]
macro_rules! upgrade_weak {
    ($x:ident, $r:expr) => {{
        match $x.upgrade() {
            Some(o) => o,
            None => return $r,
        }
    }};
    ($x:ident) => {
        upgrade_weak!($x, ())
    };
}

#[derive(Debug, clap::Parser)]
struct Args {
    #[clap(short, long, default_value = "wss://webrtc.gstreamer.net:8443")]
    server: String,
    /// Peer ID that should be called. If not given then an incoming call is expected.
    #[clap(short, long)]
    peer_id: Option<u32>,
    /// Our ID. If not given then a random ID is created.
    #[clap(short, long)]
    our_id: Option<u32>,
    /// Request that the peer creates an offer.
    #[clap(short, long, default_value = "false")]
    remote_offerer: bool,
}

// JSON messages we communicate with
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum JsonMsg {
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        mid: Option<String>,
    },
    Sdp {
        #[serde(rename = "type")]
        type_: String,
        sdp: String,
    },
}

#[derive(Debug)]
struct AppInner {
    args: Args,
    join_handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    sink_pipeline: gst::Pipeline,
    sendrecv_pipeline: gst::Pipeline,
    _webrtcsend: gst::Element,
    webrtcrecv: gst::Element,
    session: gst::Object,
    send_msg_tx: Mutex<mpsc::UnboundedSender<WsMessage>>,
}

#[derive(Debug)]
struct App(Arc<AppInner>);
#[derive(Debug)]
struct AppWeak(Weak<AppInner>);

// To be able to access the App's fields directly
impl std::ops::Deref for App {
    type Target = AppInner;

    fn deref(&self) -> &AppInner {
        &self.0
    }
}

impl AppWeak {
    // Try upgrading a weak reference to a strong one
    fn upgrade(&self) -> Option<App> {
        self.0.upgrade().map(App)
    }
}

const TS_MAX_THROTTLING_MS: u32 = 10;
// Using the same threadshare sheduler for
// both send & recv, but could be different.
const TS_SEND_CONTEXT: &str = "ts-ctx";
const TS_RECV_CONTEXT: &str = "ts-ctx";

const INTER_RECV_NAME: &str = "inter-recv.1";

impl App {
    fn downgrade(&self) -> AppWeak {
        AppWeak(Arc::downgrade(&self.0))
    }

    async fn new(
        args: Args,
    ) -> anyhow::Result<(
        Self,
        impl Stream<Item = gst::Message>,
        impl Stream<Item = gst::Message>,
        impl Stream<Item = WsMessage>,
    )> {
        // Create the GStreamer pipelines

        // the audio sources & WebRTC sender / receiver elements
        let sendrecv_pipeline = gst::parse::launch(&format!("pipeline. (name=sendrecv
            ts-audiotestsrc name=a4-src freq=440 is-live=true volume=0.07 samples-per-buffer=480 context={TS_SEND_CONTEXT} context-wait={TS_MAX_THROTTLING_MS}
            ! audio/x-raw,channels=2,rate=48000
            ! src-selector.sink_%u

            ts-audiotestsrc name=a5-src freq=880 is-live=true volume=0.07 samples-per-buffer=480 context={TS_SEND_CONTEXT} context-wait={TS_MAX_THROTTLING_MS}
            ! audio/x-raw,channels=2,rate=48000
            ! src-selector.sink_%u

            ts-input-selector name=src-selector context={TS_SEND_CONTEXT} context-wait={TS_MAX_THROTTLING_MS}
            ! ts-clocksync
            ! opusenc perfect-timestamp=true
            ! rtpopuspay2 name=apay pt=111 ! application/x-rtp,encoding-name=OPUS
            ! webrtcsend name=webrtcsend early-data-mode=drop

            webrtcrecv name=webrtcrecv threadshare-mode=true
                threadshare-context={TS_RECV_CONTEXT} threadshare-context-wait={TS_MAX_THROTTLING_MS}
            )",
        ))?
        .downcast::<gst::Pipeline>()
        .unwrap();

        // the audio sink pipeline:
        // When an incoming stream is received, the WebRTC receiver will hook a decodebin and the decoded
        // stream will be transfered to the sink pipeline via a ts-inter channel
        let sink_pipeline = gst::parse::launch(&format!(
            "pipeline. (name=sink
            ts-intersrc name=intersrc-recv inter-context={INTER_RECV_NAME} context={TS_RECV_CONTEXT} context-wait={TS_MAX_THROTTLING_MS}
            ! audioconvert ! audioresample
            ! ts-blocking-adapter
            ! autoaudiosink name=audio-sink
            )",
        ))?
        .downcast::<gst::Pipeline>()
        .unwrap();

        // Make sure all the pipelines share the same time ref
        let clock = gst::SystemClock::obtain();
        let base_time = clock.time();

        sendrecv_pipeline.set_clock(Some(&clock)).unwrap();
        sendrecv_pipeline.set_start_time(gst::ClockTime::NONE);
        sendrecv_pipeline.set_base_time(base_time);

        sink_pipeline.set_clock(Some(&clock)).unwrap();
        sink_pipeline.set_start_time(gst::ClockTime::NONE);
        sink_pipeline.set_base_time(base_time);

        // Get access to the webrtcsend/recv by name
        let webrtcsend = sendrecv_pipeline
            .by_name("webrtcsend")
            .expect("can't find webrtcsend");
        let webrtcrecv = sendrecv_pipeline
            .by_name("webrtcrecv")
            .expect("can't find webrtcrecv");

        // Push packets downstream asap: the source already paces packets.
        // Synchronising here would also cause outbound RTP packets to be sent
        // in accordance with the inbound branches' latency.
        webrtcsend
            .downcast_ref::<gst::Bin>()
            .unwrap()
            .connect_deep_element_added(|_webrtcsend, _sub_bin, elem| {
                if elem.type_() == gst_app::AppSink::static_type() {
                    elem.set_property("sync", false);
                }
            });

        // Create a stream for handling the GStreamer message asynchronously
        let sendrecv_bus = sendrecv_pipeline.bus().unwrap();
        let sendrecv_gst_msg_rx = sendrecv_bus.stream();

        let sink_bus = sink_pipeline.bus().unwrap();
        let sink_gst_msg_rx = sink_bus.stream();

        // Channel for outgoing WebSocket messages from other threads
        let (send_ws_msg_tx, send_ws_msg_rx) = mpsc::unbounded::<WsMessage>();

        if sendrecv_pipeline.set_state(gst::State::Ready).is_err() {
            gst::element_error!(
                sendrecv_pipeline,
                gst::LibraryError::Failed,
                ("Failed to set send pipeline to Ready")
            );
        }

        if sink_pipeline.set_state(gst::State::Playing).is_err() {
            gst::element_error!(
                sink_pipeline,
                gst::LibraryError::Failed,
                ("Failed to set sink pipeline to Playing")
            );
        }

        let session = webrtcsend.property::<gst::Object>("session");

        // Set some properties on webrtcsession
        session.emit_by_name::<()>("add-stun-server", &[&STUN_SERVER]);

        // FIXME: allows dns resolution to complete
        tokio::time::sleep(core::time::Duration::from_secs(1)).await;

        let app = App(Arc::new(AppInner {
            args,
            join_handles: Default::default(),
            sink_pipeline,
            sendrecv_pipeline,
            _webrtcsend: webrtcsend,
            webrtcrecv,
            session,
            send_msg_tx: Mutex::new(send_ws_msg_tx),
        }));

        // Connect to on-negotiation-needed to handle sending an Offer
        if app.args.peer_id.is_some() && !app.args.remote_offerer
            || app.args.peer_id.is_none() && app.args.remote_offerer
        {
            let app_clone = app.downgrade();
            app.session.connect_closure(
                "on-negotiation-needed",
                false,
                glib::closure!(move |_session: &gst::Object| {
                    let app = upgrade_weak!(app_clone);
                    if let Err(err) = app.on_negotiation_needed() {
                        gst::element_error!(
                            app.sendrecv_pipeline,
                            gst::LibraryError::Failed,
                            ("Failed to negotiate: {err:?}")
                        );
                    }
                }),
            );
        }

        // Whenever there is a new ICE candidate, send it to the peer
        let app_clone = app.downgrade();
        app.session.connect_closure(
            "on-ice-candidate",
            false,
            glib::closure!(move |_session: &gst::Object,
                                 mlineindex: u32,
                                 mid: Option<String>,
                                 candidate: &str| {
                let app = upgrade_weak!(app_clone);

                if let Err(err) = app.on_ice_candidate(mlineindex, mid, candidate) {
                    gst::element_error!(
                        app.sendrecv_pipeline,
                        gst::LibraryError::Failed,
                        ("Failed to send ICE candidate: {err:?}")
                    );
                }
            }),
        );

        // Whenever there is a new stream incoming from the peer, handle it
        let app_clone = app.downgrade();
        app.webrtcrecv.connect_pad_added(move |_webrtc, pad| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_incoming_stream(pad) {
                gst::element_error!(
                    app.sendrecv_pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to handle incoming stream: {err:?}")
                );
            }
        });

        // Asynchronously set the pipeline to Playing if we're creating the offer,
        // otherwise do that after the offer was received.
        if app.args.peer_id.is_some() && !app.args.remote_offerer {
            app.sendrecv_pipeline.call_async(|pipeline| {
                // If this fails, post an error on the bus so we exit
                if pipeline.set_state(gst::State::Playing).is_err() {
                    gst::element_error!(
                        pipeline,
                        gst::LibraryError::Failed,
                        ("Failed to set sendrecv pipeline to Playing")
                    );
                }
            });
        }

        Ok((app, sink_gst_msg_rx, sendrecv_gst_msg_rx, send_ws_msg_rx))
    }

    // Handle WebSocket messages, both our own as well as WebSocket protocol messages
    fn handle_websocket_message(&self, msg: &str) -> anyhow::Result<()> {
        if msg.starts_with("ERROR") {
            bail!("Got error message: {msg}");
        }

        if msg == "OFFER_REQUEST" {
            self.sendrecv_pipeline.call_async(|pipeline| {
                // If this fails, post an error on the bus so we exit
                if pipeline.set_state(gst::State::Playing).is_err() {
                    gst::element_error!(
                        pipeline,
                        gst::LibraryError::Failed,
                        ("Failed to set pipeline to Playing")
                    );
                }
            });

            return Ok(());
        }

        let json_msg: JsonMsg = serde_json::from_str(msg)?;

        match json_msg {
            JsonMsg::Sdp { type_, sdp } => self.handle_sdp(&type_, &sdp),
            JsonMsg::Ice {
                sdp_mline_index,
                mid,
                candidate,
            } => self.handle_ice(sdp_mline_index, mid, &candidate),
        }
    }

    // Handle GStreamer messages coming from the pipeline
    fn handle_pipeline_message(
        &self,
        pipeline: &gst::Pipeline,
        message: &gst::Message,
    ) -> Result<(), anyhow::Error> {
        use gst::message::MessageView;

        match message.view() {
            MessageView::Error(err) => bail!(
                "{} Error from element {}: {} ({})",
                pipeline.name(),
                err.src()
                    .map(|s| String::from(s.path_string()))
                    .unwrap_or_else(|| String::from("None")),
                err.error(),
                err.debug().unwrap_or_else(|| glib::GString::from("None")),
            ),
            MessageView::Warning(warning) => {
                println!(
                    "{} Warning: \"{}\"",
                    pipeline.name(),
                    warning.debug().unwrap()
                );
            }
            MessageView::Latency(_) => {
                pipeline.call_async(|pipeline| {
                    println!("Recalculating latency for {} pipeline", pipeline.name());
                    let _ = pipeline.recalculate_latency();
                });
            }
            _ => (),
        }

        Ok(())
    }

    // Whenever webrtcsession tells us that (re-)negotiation is needed, simply ask
    // for a new offer SDP from webrtcsession without any customization and then
    // asynchronously send it to the peer via the WebSocket connection
    fn on_negotiation_needed(&self) -> anyhow::Result<()> {
        println!("starting negotiation");

        let app_clone = self.downgrade();
        let promise = gst::Promise::with_change_func(move |reply| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_offer_created(reply) {
                gst::element_error!(
                    app.sendrecv_pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to send SDP offer: {err:?}")
                );
            }
        });

        self.session
            .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);

        Ok(())
    }

    // Once webrtcbin has create the offer SDP for us, handle it by sending it to the peer via the
    // WebSocket connection
    fn on_offer_created(
        &self,
        reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
    ) -> anyhow::Result<()> {
        let reply = match reply {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                bail!("Offer creation future got no response");
            }
            Err(err) => {
                bail!("Offer creation future got error response: {err:?}");
            }
        };

        let offer = reply
            .value("sdp")
            .unwrap()
            .get::<String>()
            .expect("Invalid argument");
        self.session.emit_by_name::<()>(
            "set-local-description",
            &[&"offer", &offer, &None::<gst::Promise>],
        );

        println!("sending SDP offer to peer: {offer}",);

        let message = serde_json::to_string(&JsonMsg::Sdp {
            type_: "offer".to_string(),
            sdp: offer,
        })
        .unwrap();

        self.send_msg_tx
            .lock()
            .unwrap()
            .unbounded_send(WsMessage::text(message))
            .context("Failed to send SDP offer")?;

        Ok(())
    }

    // Once webrtcsession has create the answer SDP for us, handle it by sending it to the peer via the
    // WebSocket connection
    fn on_answer_created(
        &self,
        reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
    ) -> anyhow::Result<()> {
        let reply = match reply {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                bail!("Answer creation future got no response");
            }
            Err(err) => {
                bail!("Answer creation future got error response: {err:?}");
            }
        };

        let answer = reply
            .value("sdp")
            .unwrap()
            .get::<String>()
            .expect("Invalid argument");
        self.session.emit_by_name::<()>(
            "set-local-description",
            &[&"answer", &answer, &None::<gst::Promise>],
        );

        println!("sending SDP answer to peer: {answer}",);

        let message = serde_json::to_string(&JsonMsg::Sdp {
            type_: "answer".to_string(),
            sdp: answer,
        })
        .unwrap();

        self.send_msg_tx
            .lock()
            .unwrap()
            .unbounded_send(WsMessage::text(message))
            .context("Failed to send SDP answer")?;

        Ok(())
    }

    fn configure_pipeline_on_offer(
        &self,
        offer: &sdp_types::Session,
    ) -> anyhow::Result<oneshot::Receiver<()>> {
        // Extract audio payload types from the SDP and configure accordingly on the
        // pipeline as these have to match with the offer
        let mut opus_pt = None;
        for media in &offer.medias {
            for fmt in media.fmt.split(" ") {
                if fmt == "webrtc-datachannel" {
                    continue;
                }

                let pt = match fmt.parse::<u8>() {
                    Ok(pt) => pt,
                    Err(_) => continue,
                };

                let Some(Ok(rtpmap)) = media.get_first_attribute_typed::<sdp_types::RtpMap>()
                else {
                    bail!("no or invalid RTP Map in {media:?}");
                };
                if rtpmap.payload_type == pt
                    && rtpmap.encoding_name.eq_ignore_ascii_case("opus")
                    && opus_pt.is_none()
                {
                    opus_pt = Some(pt);
                }
            }
        }

        let (sender, recver) = oneshot::channel();

        if let Some(opus_pt) = opus_pt {
            let apay = self.sendrecv_pipeline.by_name("apay").unwrap();
            apay.set_property("pt", opus_pt as u32);

            let srcpad = apay.static_pad("src").unwrap();
            srcpad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
                let sender = Arc::new(Mutex::new(Some(sender)));
                move |pad, info| {
                    let Some(event) = info.event() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let gst::EventView::Caps(_caps) = event.view() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let Some(peer) = pad.peer() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    peer.send_event(event.clone());
                    if let Some(sender) = sender.lock().unwrap().take() {
                        let _ = sender.send(());
                    }
                    gst::PadProbeReturn::Remove
                }
            });
        } else {
            gst::element_error!(
                self.sendrecv_pipeline,
                gst::LibraryError::Failed,
                ("Not all streams found in the offer")
            );
            bail!("Not all streams found in the offer");
        }

        Ok(recver)
    }

    // Handle incoming SDP answers from the peer
    fn handle_sdp(&self, type_: &str, sdp: &str) -> anyhow::Result<()> {
        if type_ == "answer" {
            print!("Received answer:\n{sdp}\n");

            self.session.emit_by_name::<()>(
                "set-remote-description",
                &[&type_, &sdp, &None::<gst::Promise>],
            );

            Ok(())
        } else if type_ == "offer" {
            print!("Received offer:\n{sdp}\n");

            // And then asynchronously start our pipeline and do the next steps. The
            // pipeline needs to be started before we can create an answer
            let join_handle = tokio::task::spawn({
                let app_weak = self.downgrade();
                let type_ = type_.to_string();
                let sdp_str = sdp.to_string();
                async move {
                    let app = upgrade_weak!(app_weak);

                    let sdp =
                        sdp_types::Session::parse(sdp_str.as_bytes()).expect("Failed to parse SDP");

                    let Ok(recver) = app.configure_pipeline_on_offer(&sdp) else {
                        return;
                    };

                    // If this fails, post an error on the bus so we exit
                    if app
                        .sendrecv_pipeline
                        .set_state(gst::State::Playing)
                        .is_err()
                    {
                        gst::element_error!(
                            app.sendrecv_pipeline,
                            gst::LibraryError::Failed,
                            ("Failed to set pipeline to Playing")
                        );
                        return;
                    }

                    app.0.session.emit_by_name::<()>(
                        "set-remote-description",
                        &[&type_, &sdp_str, &None::<gst::Promise>],
                    );

                    let app_clone = app.downgrade();
                    let promise = gst::Promise::with_change_func(move |reply| {
                        let app = upgrade_weak!(app_clone);

                        if let Err(err) = app.on_answer_created(reply) {
                            gst::element_error!(
                                app.sendrecv_pipeline,
                                gst::LibraryError::Failed,
                                ("Failed to send SDP answer: {:?}", err)
                            );
                        }
                    });
                    recver.await.unwrap();

                    app.0
                        .session
                        .emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
                }
            });
            self.0.join_handles.lock().unwrap().push(join_handle);

            Ok(())
        } else {
            bail!("Sdp type is not \"answer\" or \"offer\" but \"{type_}\"")
        }
    }

    // Handle incoming ICE candidates from the peer by passing them to webrtcbin
    fn handle_ice(
        &self,
        sdp_mline_index: u32,
        mid: Option<String>,
        candidate: &str,
    ) -> anyhow::Result<()> {
        self.session.emit_by_name::<()>(
            "add-ice-candidate",
            &[&sdp_mline_index, &mid, &candidate, &None::<gst::Promise>],
        );

        Ok(())
    }

    // Asynchronously send ICE candidates to the peer via the WebSocket connection as a JSON
    // message
    fn on_ice_candidate(
        &self,
        mlineindex: u32,
        mid: Option<String>,
        candidate: &str,
    ) -> anyhow::Result<()> {
        let message = serde_json::to_string(&JsonMsg::Ice {
            candidate: candidate.to_string(),
            mid,
            sdp_mline_index: mlineindex,
        })
        .unwrap();

        self.send_msg_tx
            .lock()
            .unwrap()
            .unbounded_send(WsMessage::text(message))
            .context("Failed to send ICE candidate")?;

        Ok(())
    }

    // Whenever there's a new incoming, encoded stream from the peer create a new decodebin
    fn on_incoming_stream(&self, pad: &gst::Pad) -> anyhow::Result<()> {
        // Early return for the source pads we're adding ourselves
        if pad.direction() != gst::PadDirection::Src {
            return Ok(());
        }

        let decodebin = gst::ElementFactory::make("decodebin").build().unwrap();
        let app_clone = self.downgrade();
        decodebin.connect_pad_added(move |_decodebin, pad| {
            let app = upgrade_weak!(app_clone);

            if let Err(err) = app.on_incoming_decodebin_stream(pad) {
                gst::element_error!(
                    app.sendrecv_pipeline,
                    gst::LibraryError::Failed,
                    ("Failed to handle decoded stream: {:?}", err)
                );
            }
        });

        self.sendrecv_pipeline.add(&decodebin).unwrap();
        decodebin.sync_state_with_parent().unwrap();

        let sinkpad = decodebin.static_pad("sink").unwrap();
        pad.link(&sinkpad).unwrap();

        Ok(())
    }

    // Handle a newly decoded decodebin stream and depending on its type, create the relevant
    // elements or simply ignore it
    fn on_incoming_decodebin_stream(&self, pad: &gst::Pad) -> Result<(), anyhow::Error> {
        let caps = pad.current_caps().unwrap();
        let name = caps.structure(0).unwrap().name();

        let sink = if name.starts_with("audio/") {
            gst::parse::bin_from_description(
                &format!("ts-intersink name=intersink-recv inter-context={INTER_RECV_NAME}"),
                true,
            )?
        } else {
            println!("Unknown pad {pad:?}, ignoring");
            return Ok(());
        };

        self.sendrecv_pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent()
            .with_context(|| format!("can't start sink for stream {caps:?}"))?;

        let sinkpad = sink.static_pad("sink").unwrap();
        pad.link(&sinkpad)
            .with_context(|| format!("can't link sink for stream {caps:?}"))?;

        Ok(())
    }
}

// Make sure to shut down the pipeline when it goes out of scope
// to release any system resources
impl Drop for AppInner {
    fn drop(&mut self) {
        for join_handle in self.join_handles.lock().unwrap().drain(..) {
            join_handle.abort();
        }
        let _ = self.sendrecv_pipeline.set_state(gst::State::Null);
        let _ = self.sink_pipeline.set_state(gst::State::Null);
    }
}

async fn run(
    args: Args,
    ws: impl Sink<WsMessage, Error = WsError> + Stream<Item = Result<WsMessage, WsError>>,
) -> Result<(), anyhow::Error> {
    // Split the websocket into the Sink and Stream
    let (mut ws_sink, ws_stream) = ws.split();
    // Fuse the Stream, required for the select macro
    let mut ws_stream = ws_stream.fuse();

    // Create our application state
    let (app, sink_gst_msg_rx, sendrecv_gst_msg_rx, send_ws_msg_rx) = App::new(args).await?;

    let mut sink_gst_msg_rx = sink_gst_msg_rx.fuse();
    let mut sendrecv_gst_msg_rx = sendrecv_gst_msg_rx.fuse();
    let mut send_ws_msg_rx = send_ws_msg_rx.fuse();

    // And now let's start our message loop
    loop {
        let ws_msg = tokio::select! {
            // Handle the WebSocket messages here
            ws_msg = ws_stream.select_next_some() => {
                match ws_msg? {
                    WsMessage::Close(_) => {
                        println!("peer disconnected");
                        break
                    },
                    WsMessage::Ping(data) => Some(WsMessage::Pong(data)),
                    WsMessage::Pong(_) => None,
                    WsMessage::Binary(_) => None,
                    WsMessage::Text(text) => {
                        app.handle_websocket_message(&text)?;
                        None
                    },
                    WsMessage::Frame(_) => unreachable!(),
                }
            },
            // Pass the GStreamer messages to the application control logic
            sink_gst_msg = sink_gst_msg_rx.select_next_some() => {
                app.handle_pipeline_message(&app.sink_pipeline, &sink_gst_msg)?;
                None
            },
            sendrecv_gst_msg = sendrecv_gst_msg_rx.select_next_some() => {
                app.handle_pipeline_message(&app.sendrecv_pipeline, &sendrecv_gst_msg)?;
                None
            },
            // Handle WebSocket messages we created asynchronously
            // to send them out now
            ws_msg = send_ws_msg_rx.select_next_some() => Some(ws_msg),
            // Once we're done, break the loop and return
            else => break,
        };

        // If there's a message to send out, do so now
        if let Some(ws_msg) = ws_msg {
            ws_sink.send(ws_msg).await?;
        }
    }

    Ok(())
}

// Check if all GStreamer plugins we require are available
fn check_plugins() -> anyhow::Result<()> {
    let needed = [
        "audioconvert",
        "audioresample",
        "autodetect",
        "opus",
        "playback",
        "rsrtp",
        "rswebrtcbin2",
        "srtp",
        "threadshare",
    ];

    let registry = gst::Registry::get();
    let missing = needed
        .iter()
        .filter(|n| registry.find_plugin(n).is_none())
        .cloned()
        .collect::<Vec<_>>();

    if !missing.is_empty() {
        bail!("Missing plugins: {missing:?}");
    } else {
        Ok(())
    }
}

async fn async_main() -> anyhow::Result<()> {
    gstrswebrtcbin2::plugin_register_static().unwrap();
    gstrsrtp::plugin_register_static().unwrap();
    gstthreadshare::plugin_register_static().unwrap();

    check_plugins()?;

    let args = Args::parse();

    // Connect to the given server
    let (mut ws, _) = async_tungstenite::tokio::connect_async(&args.server).await?;

    println!("connected");

    // Say HELLO to the server and see if it replies with HELLO
    let our_id = args
        .our_id
        .unwrap_or_else(|| rand::rng().random_range(10..10_000));
    println!("Registering id {our_id} with server");
    ws.send(WsMessage::text(format!("HELLO {our_id}"))).await?;

    let msg = ws
        .next()
        .await
        .ok_or_else(|| anyhow!("didn't receive anything"))??;

    if msg != WsMessage::text("HELLO") {
        bail!("server didn't say HELLO");
    }

    if let Some(peer_id) = args.peer_id {
        println!("Setting up call with peer id {peer_id}");

        // Join the given session
        ws.send(WsMessage::text(format!("SESSION {peer_id}")))
            .await?;

        let msg = ws
            .next()
            .await
            .ok_or_else(|| anyhow!("didn't receive anything"))??;

        if msg != WsMessage::text("SESSION_OK") {
            bail!("server error: {msg:?}");
        }

        // If we expect the peer to create the offer request it now
        if args.remote_offerer {
            println!("Requesting offer from peer");
            ws.send(WsMessage::text("OFFER_REQUEST")).await?;
        }
    } else {
        println!("Waiting for incoming call");
    }

    // All good, let's run our message loop
    run(args, ws).await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();

    gst::init()?;

    let res = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            eprintln!("shutting down due to user request");
            Ok(())
        }
        res = async_main() => res,
    };

    unsafe {
        gst::deinit();
    }

    res
}
