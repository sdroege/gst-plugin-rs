use anyhow::{Error, anyhow};
use clap::Parser;
use futures::prelude::*;
use gst::glib;
use gst::prelude::*;
use http::HeaderMap;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex};
use tokio::runtime;
use url::Url;
use warp::{Filter, Reply, http};

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

const ROOT: &str = "whep";
const ENDPOINT_PATH: &str = "endpoint";
const RESOURCE_PATH: &str = "resource";
const CONTENT_SDP: &str = "application/sdp";
const STUN_SERVER: &str = "stun://stun.l.google.com:19302";

pub fn build_link_header(url_str: &str) -> Result<String, url::ParseError> {
    let url = url::Url::parse(url_str)?;

    let mut link_str: String = "<".to_owned() + url.scheme();
    if let Some(host) = url.host_str() {
        write!(&mut link_str, ":{host}").unwrap();
    }

    if let Some(port) = url.port() {
        write!(&mut link_str, ":{port}").unwrap();
    }

    write!(&mut link_str, "{}", url.path()).unwrap();

    if let Some(query) = url.query() {
        write!(&mut link_str, "?{query}").unwrap();
    }

    write!(&mut link_str, ">").unwrap();

    if let Some(password) = url.password() {
        write!(&mut link_str, "; rel=\"ice-server\"; username=\"{}\"; credential:\"{}\"; credential-type:\"password\";",
            url.username(), password).unwrap();
    }

    Ok(link_str)
}

struct State {
    webrtcsink: gst::Element,
    shutdown_tx: tokio::sync::mpsc::Sender<()>,
}

impl State {}

type Server = Arc<Mutex<State>>;

async fn delete_handler(
    server: Server,
    id: String,
) -> Result<impl warp::reply::Reply, warp::Rejection> {
    let fut = {
        let signaller = server
            .lock()
            .unwrap()
            .webrtcsink
            .dynamic_cast_ref::<gst::ChildProxy>()
            .unwrap()
            .child_by_name("signaller")
            .unwrap();

        let (promise, fut) = gst::Promise::new_future();

        signaller.emit_by_name::<()>("delete", &[&id, &promise]);

        fut
    };

    let reply = fut.await.unwrap().unwrap();

    let status = reply.get::<u32>("status").unwrap() as u16;

    eprintln!("Session {id} was removed");

    Ok(http::StatusCode::from_u16(status).unwrap().into_response())
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
    let fut = {
        let signaller = server
            .lock()
            .unwrap()
            .webrtcsink
            .dynamic_cast_ref::<gst::ChildProxy>()
            .unwrap()
            .child_by_name("signaller")
            .unwrap();

        let bytes = glib::Bytes::from(body.as_ref());
        let (promise, fut) = gst::Promise::new_future();

        signaller.emit_by_name::<()>("post", &[&id, &bytes, &promise]);

        fut
    };

    let reply = fut.await.unwrap().unwrap();

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

async fn patch_handler(
    body: warp::hyper::body::Bytes,
    mut headers: HeaderMap,
    server: Server,
    id: String,
) -> Result<impl warp::reply::Reply, warp::Rejection> {
    let fut = {
        let signaller = server
            .lock()
            .unwrap()
            .webrtcsink
            .dynamic_cast_ref::<gst::ChildProxy>()
            .unwrap()
            .child_by_name("signaller")
            .unwrap();

        let bytes = glib::Bytes::from(body.as_ref());
        let (promise, fut) = gst::Promise::new_future();
        let mut headers_builder = gst::Structure::builder("whep-signaller/headers");

        for (header, value) in headers.drain() {
            if let Some(header) = header {
                let value = value
                    .to_str()
                    .expect("Header value should contain only visible ASCII strings");
                headers_builder = headers_builder.field(header.to_string(), value);
            }
        }

        signaller.emit_by_name::<()>("patch", &[&id, &bytes, &headers_builder.build(), &promise]);

        fut
    };

    let reply = fut.await.unwrap().unwrap();

    let status = reply.get::<u32>("status").unwrap() as u16;

    if status == http::StatusCode::NO_CONTENT {
        eprintln!("Session patched with ID {id}");
    }

    Ok(http::StatusCode::from_u16(status).unwrap().into_response())
}

fn serve(
    args: &Args,
) -> Result<(tokio::task::JoinHandle<()>, tokio::sync::mpsc::Sender<()>), Error> {
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
            return Err(anyhow!(format!("Unable to start WHEP Server: {e:?}")));
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);

    let pipeline = gst::Pipeline::new();
    let videotestsrc = gst::ElementFactory::make("videotestsrc").build()?;
    let webrtcsink = gst::ElementFactory::make("whepserversink").build()?;

    let signaller = webrtcsink
        .dynamic_cast_ref::<gst::ChildProxy>()
        .unwrap()
        .child_by_name("signaller")
        .unwrap();

    signaller.set_property("host-addr", None::<String>);

    pipeline.add_many([&videotestsrc, &webrtcsink])?;
    gst::Element::link_many([&videotestsrc, &webrtcsink])?;

    let server = Arc::new(Mutex::new(State {
        webrtcsink,
        shutdown_tx: tx.clone(),
    }));

    let api = filter(server.clone());

    let jh = RUNTIME.spawn(async move {
        pipeline
            .call_async_future(|pipeline| pipeline.set_state(gst::State::Playing).unwrap())
            .await;

        let bus = pipeline.bus().unwrap();
        let pipeline_weak = pipeline.downgrade();
        RUNTIME.spawn(async move {
            let mut bus_stream = bus.stream();

            while let Some(bus_msg) = bus_stream.next().await {
                use gst::MessageView::*;

                let Some(pipeline) = pipeline_weak.upgrade() else {
                    break;
                };

                match bus_msg.view() {
                    Error(msg) => {
                        let err = msg.error();

                        eprintln!("Pipeline errored out: {err}");
                        let shutdown_tx = server.lock().unwrap().shutdown_tx.clone();
                        let _ = shutdown_tx.send(()).await;
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

        warp::serve(api)
            .bind(addr)
            .await
            .graceful(async move {
                let msg = rx.recv().await;
                match msg {
                    Some(_) => eprintln!("Server shut down signal received"),
                    None => eprintln!("Sender dropped"),
                }
            })
            .run()
            .await;

        eprintln!("Stopped the server task...");
        let finalize_handle = RUNTIME.spawn(async move {
            pipeline
                .call_async_future(move |pipeline| {
                    let _ = pipeline.set_state(gst::State::Null);
                })
                .await;
        });

        let _ = finalize_handle.await;
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

    let patch_filter = warp::post()
        .and(warp::path(RESOURCE_PATH))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(warp::body::bytes())
        .and(warp::header::headers_cloned())
        .and(with_server(server.clone()))
        .and_then(move |id, body, headers, server| async move {
            patch_handler(body, headers, server, id).await
        });

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
        .or(prefix.and(patch_filter))
        .or(prefix.and(delete_filter))
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

    RUNTIME.block_on(async move {
        let _ = shutdown_tx.send(()).await;
    });

    RUNTIME.block_on(async move {
        let _ = handle.await;
    });

    Ok(())
}
