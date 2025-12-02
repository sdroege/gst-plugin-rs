use anyhow::{anyhow, Error};
use clap::Parser;
use futures::prelude::*;
use gst::glib;
use gst::prelude::*;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex};
use tokio::runtime;
use url::Url;
use warp::{http, Filter, Reply};

#[derive(Parser, Debug)]
struct Args {
    host_addr: String,
}

pub static RUNTIME: LazyLock<runtime::Runtime> = LazyLock::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

const ROOT: &str = "whip";
const ENDPOINT_PATH: &str = "endpoint";
const RESOURCE_PATH: &str = "resource";
const CONTENT_SDP: &str = "application/sdp";
const STUN_SERVER: &str = "stun://stun.l.google.com:19302";

pub fn build_link_header(url_str: &str) -> Result<String, url::ParseError> {
    let url = url::Url::parse(url_str)?;

    let mut link_str: String = "<".to_owned() + url.scheme();
    if let Some(host) = url.host_str() {
        link_str += &format!(":{}", host);
    }

    if let Some(port) = url.port() {
        link_str += &format!(":{}", port.to_string().as_str());
    }

    link_str += url.path();

    if let Some(query) = url.query() {
        link_str += &format!("?{}", query);
    }

    link_str += ">";

    if let Some(password) = url.password() {
        link_str += &format!("; rel=\"ice-server\"; username=\"{}\"; credential:\"{}\"; credential-type:\"password\";",
            url.username(), password);
    }

    Ok(link_str)
}

struct Session {
    webrtcsrc: gst::Element,
    pipeline: gst::Pipeline,
}

struct State {
    sessions: HashMap<String, Session>,
    finalize_handles: HashMap<String, tokio::task::JoinHandle<()>>,
}

impl State {}

type Server = Arc<Mutex<State>>;

async fn delete_handler(
    server: Server,
    id: String,
) -> Result<impl warp::reply::Reply, warp::Rejection> {
    let session = server.lock().unwrap().sessions.remove(&id);

    if let Some(session) = session {
        let rx = {
            let signaller = session
                .webrtcsrc
                .dynamic_cast_ref::<gst::ChildProxy>()
                .unwrap()
                .child_by_name("signaller")
                .unwrap();

            let (tx, rx) = tokio::sync::oneshot::channel::<
                Result<Option<gst::Structure>, gst::PromiseError>,
            >();
            let promise = gst::Promise::with_change_func(move |reply| {
                let _ = tx.send(reply.map(|res| res.map(|s| s.to_owned())));
            });

            signaller.emit_by_name::<()>("delete", &[&id, &promise]);

            rx
        };

        let reply = rx.await.unwrap().unwrap().unwrap();

        let status = reply.get::<u32>("status").unwrap() as u16;

        let server_clone = server.clone();
        let id_clone = id.clone();
        server.lock().unwrap().finalize_handles.insert(
            id.clone(),
            RUNTIME.spawn(async move {
                session
                    .pipeline
                    .call_async_future(move |pipeline| {
                        let _ = pipeline.set_state(gst::State::Null);
                        server_clone
                            .lock()
                            .unwrap()
                            .finalize_handles
                            .remove(&id_clone);
                    })
                    .await;
            }),
        );

        eprintln!("Session {id} was removed");

        Ok(http::StatusCode::from_u16(status).unwrap().into_response())
    } else {
        Ok(warp::reply::reply().into_response())
    }
}

async fn options_handler() -> Result<impl warp::reply::Reply, warp::Rejection> {
    let mut links = http::HeaderMap::new();
    links.append(
        http::header::LINK,
        warp::http::HeaderValue::from_str(&build_link_header(STUN_SERVER).unwrap()).unwrap(),
    );

    let mut res = http::Response::builder()
        .header("Access-Post", CONTENT_SDP)
        .body(bytes::Bytes::new())
        .unwrap();

    let headers = res.headers_mut();
    headers.extend(links);

    Ok(res)
}

async fn post_handler(
    body: warp::hyper::body::Bytes,
    server: Server,
    id: Option<String>,
) -> Result<impl warp::reply::Reply, warp::Rejection> {
    let (rx, ws, pipeline) = {
        let pipeline = gst::Pipeline::builder().build();
        let ws = gst::ElementFactory::make("whipserversrc")
            .property("stun-server", STUN_SERVER)
            .build()
            .unwrap();
        let signaller = ws
            .dynamic_cast_ref::<gst::ChildProxy>()
            .unwrap()
            .child_by_name("signaller")
            .unwrap();

        signaller.set_property("host-addr", None::<String>);

        ws.set_property("enable-data-channel-navigation", true);

        let pipe = pipeline.clone();
        ws.connect_pad_added(move |_ws, pad| {
            if pad.name().contains("video_") {
                link_video(pad, &pipe);
            } else if pad.name().contains("audio_") {
                link_audio(pad, &pipe);
            } else {
                println!("unknown pad type {}", pad.name());
            }
        });

        let pipe = pipeline.clone();
        ws.connect_pad_removed(move |_ws, pad| {
            if pad.name().contains("video_") {
                unlink_video(pad, &pipe);
            } else if pad.name().contains("audio_") {
                unlink_audio(pad, &pipe);
            } else {
                println!("unknown pad type {}", pad.name());
            }
        });
        pipeline.add(&ws).unwrap();

        let bytes = glib::Bytes::from(body.as_ref());
        let (tx, rx) =
            tokio::sync::oneshot::channel::<Result<Option<gst::Structure>, gst::PromiseError>>();
        let promise = gst::Promise::with_change_func(move |reply| {
            let _ = tx.send(reply.map(|res| res.map(|s| s.to_owned())));
        });

        signaller.emit_by_name::<()>("post", &[&id, &bytes, &promise]);

        (rx, ws, pipeline)
    };

    pipeline
        .call_async_future(|pipeline| pipeline.set_state(gst::State::Playing).unwrap())
        .await;

    let reply = rx.await.unwrap().unwrap().unwrap();

    let status = reply.get::<u32>("status").unwrap() as u16;
    let headers = reply.get::<gst::Structure>("headers").unwrap();
    let body = reply.get::<glib::Bytes>("body");

    if status == http::StatusCode::CREATED {
        let session_id = headers
            .value("location")
            .ok()
            .and_then(|location| location.get::<String>().ok())
            .and_then(|location| {
                location
                    .rsplit_once("/")
                    .map(|segments| segments.1.to_string())
            })
            .unwrap();

        eprintln!("Session created with ID {session_id}");

        server.lock().unwrap().sessions.insert(
            session_id.to_string(),
            Session {
                webrtcsrc: ws,
                pipeline: pipeline.clone(),
            },
        );

        RUNTIME.spawn(async move {
            let bus = pipeline.bus().unwrap();
            let mut bus_stream = bus.stream();

            while let Some(bus_msg) = bus_stream.next().await {
                use gst::MessageView::*;

                match bus_msg.view() {
                    Error(msg) => {
                        let err = msg.error();

                        eprintln!("Pipeline {session_id} errored out: {err}");

                        let session = server.lock().unwrap().sessions.remove(&session_id);
                        let server_clone = server.clone();
                        let id_clone = session_id.clone();
                        if let Some(session) = session {
                            server.lock().unwrap().finalize_handles.insert(
                                session_id,
                                RUNTIME.spawn(async move {
                                    session
                                        .pipeline
                                        .call_async_future(move |pipeline| {
                                            let _ = pipeline.set_state(gst::State::Null);
                                            server_clone
                                                .lock()
                                                .unwrap()
                                                .finalize_handles
                                                .remove(&id_clone);
                                        })
                                        .await;
                                }),
                            );
                        }
                        break;
                    }
                    Latency(_) => {
                        pipeline
                            .call_async_future(|pipeline| {
                                let _ = pipeline.recalculate_latency();
                            })
                            .await;
                    }
                    _ => (),
                }
            }
        });
    }

    let mut response_builder = http::Response::builder().status(status);

    for (name, value) in headers.iter() {
        response_builder =
            response_builder.header(name.to_string(), value.get::<String>().unwrap());
    }

    let body = body.ok().map(bytes::Bytes::from_owner);

    let response = response_builder.body(body).unwrap();

    Ok(response)
}

fn serve(
    args: &Args,
) -> Result<
    (
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Sender<()>,
    ),
    Error,
> {
    let addr: SocketAddr;
    let host_addr = Url::parse(&args.host_addr)?;
    match host_addr.socket_addrs(|| None) {
        Ok(v) => {
            // pick the first vector item
            addr = v[0];
            eprintln!("using {addr:?} as address");
        }
        Err(e) => {
            eprintln!("error getting addr from uri  {e:?}");
            return Err(anyhow!(format!("Unable to start WHIP Server: {e:?}")));
        }
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();

    let server = Arc::new(Mutex::new(State {
        sessions: HashMap::new(),
        finalize_handles: HashMap::new(),
    }));

    let api = filter(server.clone());

    let jh = RUNTIME.spawn(async move {
        warp::serve(api)
            .bind(addr)
            .await
            .graceful(async move {
                match rx.await {
                    Ok(_) => eprintln!("Server shut down signal received"),
                    Err(e) => eprintln!("{e:?}: Sender dropped"),
                }
            })
            .run()
            .await;

        eprintln!("Stopped the server task...");

        let finalize_handles: Vec<_> = {
            let mut server = server.lock().unwrap();
            let mut finalize_handles = Vec::new();

            for (_, session) in server.sessions.drain() {
                finalize_handles.push(RUNTIME.spawn(async move {
                    session
                        .pipeline
                        .call_async_future(move |pipeline| {
                            let _ = pipeline.set_state(gst::State::Null);
                        })
                        .await;
                }));
            }

            finalize_handles.extend(server.finalize_handles.drain().map(|(_, handle)| handle));

            finalize_handles
        };

        for handle in finalize_handles {
            let _ = handle.await;
        }
    });

    eprintln!("Started the server...");

    Ok((jh, tx))
}

fn with_server(
    server: Server,
) -> impl Filter<Extract = (Server,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || server.clone())
}

fn filter(
    server: Server,
) -> impl Filter<Extract = impl warp::Reply> + Clone + Send + Sync + 'static {
    let prefix = warp::path(ROOT);

    // POST /endpoint
    let post_filter = warp::post()
        .and(warp::path(ENDPOINT_PATH))
        .and(warp::path::end())
        .and(warp::header::exact(
            http::header::CONTENT_TYPE.as_str(),
            CONTENT_SDP,
        ))
        .and(warp::body::bytes())
        .and(with_server(server.clone()))
        .and_then(move |body, server| async move { post_handler(body, server, None).await });

    // POST /endpoint/<session-id>
    let post_filter_with_id = warp::post()
        .and(warp::path(ENDPOINT_PATH))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::header::exact(
            http::header::CONTENT_TYPE.as_str(),
            CONTENT_SDP,
        ))
        .and(warp::body::bytes())
        .and(with_server(server.clone()))
        .and_then(
            move |id, body, server| async move { post_handler(body, server, Some(id)).await },
        );

    // OPTIONS /endpoint
    let options_filter = warp::options()
        .and(warp::path(ENDPOINT_PATH))
        .and(warp::path::end())
        .and_then(move || async move { options_handler().await });

    // TODO: implement patch once the signaller supports it

    // DELETE /resource/:id
    let delete_filter = warp::delete()
        .and(warp::path(RESOURCE_PATH))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(with_server(server.clone()))
        .and_then(move |id, server| async move { delete_handler(server, id).await });

    prefix
        .and(post_filter.or(post_filter_with_id))
        .or(prefix.and(options_filter))
        .or(prefix.and(delete_filter))
}

fn link_video(pad: &gst::Pad, pipeline: &gst::Pipeline) {
    let q = gst::ElementFactory::make("queue")
        .name(format!("queue_{}", pad.name()))
        .build()
        .unwrap();

    let vc = gst::ElementFactory::make("videoconvert")
        .name(format!("videoconvert_{}", pad.name()))
        .build()
        .unwrap();

    let vsink = gst::ElementFactory::make("autovideosink")
        .name(format!("vsink_{}", pad.name()))
        .build()
        .unwrap();

    pipeline.add_many([&q, &vc, &vsink]).unwrap();
    gst::Element::link_many([&q, &vc, &vsink]).unwrap();
    let qsinkpad = q.static_pad("sink").unwrap();
    pad.link(&qsinkpad).expect("linking should work");

    q.sync_state_with_parent().unwrap();
    vc.sync_state_with_parent().unwrap();
    vsink.sync_state_with_parent().unwrap();
}

fn unlink_video(pad: &gst::Pad, pipeline: &gst::Pipeline) {
    let q = pipeline
        .by_name(format!("queue_{}", pad.name()).as_str())
        .unwrap();
    let vc = pipeline
        .by_name(format!("videoconvert_{}", pad.name()).as_str())
        .unwrap();
    let vsink = pipeline
        .by_name(format!("vsink_{}", pad.name()).as_str())
        .unwrap();

    q.set_state(gst::State::Null).unwrap();
    vc.set_state(gst::State::Null).unwrap();
    vsink.set_state(gst::State::Null).unwrap();

    pipeline.remove_many([&q, &vc, &vsink]).unwrap();
}

fn link_audio(pad: &gst::Pad, pipeline: &gst::Pipeline) {
    let aq = gst::ElementFactory::make("queue")
        .name(format!("aqueue_{}", pad.name()))
        .build()
        .unwrap();

    let ac = gst::ElementFactory::make("audioconvert")
        .name(format!("audioconvert_{}", pad.name()))
        .build()
        .unwrap();

    let asink = gst::ElementFactory::make("autoaudiosink")
        .name(format!("asink_{}", pad.name()))
        .build()
        .unwrap();

    pipeline.add_many([&aq, &ac, &asink]).unwrap();
    gst::Element::link_many([&aq, &ac, &asink]).unwrap();
    let qsinkpad = aq.static_pad("sink").unwrap();
    pad.link(&qsinkpad).expect("linking should work");

    aq.sync_state_with_parent().unwrap();
    ac.sync_state_with_parent().unwrap();
    asink.sync_state_with_parent().unwrap();
}

fn unlink_audio(pad: &gst::Pad, pipeline: &gst::Pipeline) {
    let aq = pipeline
        .by_name(format!("aqueue_{}", pad.name()).as_str())
        .unwrap();
    let ac = pipeline
        .by_name(format!("audioconvert_{}", pad.name()).as_str())
        .unwrap();
    let asink = pipeline
        .by_name(format!("asink_{}", pad.name()).as_str())
        .unwrap();

    aq.set_state(gst::State::Null).unwrap();
    ac.set_state(gst::State::Null).unwrap();
    asink.set_state(gst::State::Null).unwrap();

    pipeline.remove_many([&aq, &ac, &asink]).unwrap();
}

fn main() -> Result<(), Error> {
    gst::init()?;

    let args = Args::parse();

    let (jh, shutdown_tx) = serve(&args)?;

    let handle = RUNTIME.spawn(async move {
        let _ = jh.await;
    });

    let ctrl_c = tokio::signal::ctrl_c();

    let _ = RUNTIME.block_on(ctrl_c);

    let _ = shutdown_tx.send(());

    RUNTIME.block_on(async move {
        let _ = handle.await;
    });

    Ok(())
}
