// GStreamer RTSP Source 2
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
// Copyright (C) 2023-2024 Nirbheek Chauhan <nirbheek centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// https://www.rfc-editor.org/rfc/rfc2326.html

use crate::rtspsrc::RtspSrc2TlsValidationFlags;
use crate::rtspsrc::http_tunnel::TunnelStream;
use crate::utils::create_tls_connector;
use rand::distr::{Alphanumeric, Distribution};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, btree_set::BTreeSet};
use std::convert::TryFrom;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use rustls_pki_types::ServerName;

use futures::{Sink, SinkExt, Stream, StreamExt};
use socket2::Socket;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UdpSocket};
use tokio::runtime;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time;

use rtsp_types::headers::{
    ACCEPT, AUTHORIZATION, CONTENT_BASE, CONTENT_LOCATION, CONTENT_TYPE, CSeq, NptRange, NptTime,
    Public, Range, RtpInfos, RtpLowerTransport, RtpProfile, RtpTransport, RtpTransportParameters,
    Session, Transport, TransportMode, Transports, USER_AGENT, WWW_AUTHENTICATE,
};
use rtsp_types::{Message, Method, Request, Response, StatusCode, Version};

use lru::LruCache;
use url::Url;

use gst::buffer::{MappedBuffer, Readable};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_net::gio;

use super::body::Body;
use super::digest::*;
use super::sdp;
use super::transport::RtspTransportInfo;

const DEFAULT_LOCATION: Option<Url> = None;
const DEFAULT_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(5);
const DEFAULT_PORT_START: u16 = 0;
// Priority list has multicast first, because we want to prefer multicast if it's available
const DEFAULT_PROTOCOLS: &str = "udp-mcast,udp,tcp";
// Equal to MTU + 8 by default to avoid incorrectly detecting an MTU sized buffer as having
// possibly overflown our receive buffer, and triggering a doubling of the buffer sizes.
const DEFAULT_RECEIVE_MTU: u32 = 1500 + 8;
const DEFAULT_DO_RTSP_KEEP_ALIVE: bool = true;
const DEFAULT_LATENCY_MS: u32 = 200;
const DEFAULT_TLS_VALIDATION: RtspSrc2TlsValidationFlags = RtspSrc2TlsValidationFlags::ValidateAll;

const MAX_MESSAGE_SIZE: usize = 1024 * 1024;
const MAX_BIND_PORT_RETRY: u16 = 100;
const UDP_PACKET_MAX_SIZE: u32 = 65535 - 8;
const RTCP_ADDR_CACHE_SIZE: usize = 100;
const MAX_AUTH_RETRIES: u32 = 1;
const INITIAL_NONCE: u32 = 1;

const SIGNAL_TLS_CLIENT_AUTH: &str = "tls-client-auth";
const SIGNAL_GET_PARAMETER: &str = "get-parameter";
const SIGNAL_GET_PARAMETERS: &str = "get-parameters";
const SIGNAL_SET_PARAMETER: &str = "set-parameter";
const GET_PARAMETER_REPLY: &str = "get-parameter-reply";
const SET_PARAMETER_REPLY: &str = "set-parameter-reply";

static RTCP_CAPS: LazyLock<gst::Caps> =
    LazyLock::new(|| gst::Caps::from(gst::Structure::new_empty("application/x-rtcp")));
static SRTP_RTCP_CAPS: LazyLock<gst::Caps> =
    LazyLock::new(|| gst::Caps::from(gst::Structure::new_empty("application/x-srtcp")));

// Hardcoded for now
const DEFAULT_USER_AGENT: &str = concat!(
    "GStreamer rtspsrc2 ",
    env!("CARGO_PKG_VERSION"),
    "-",
    env!("COMMIT_ID")
);

#[allow(clippy::large_enum_variant)]
enum TcpOrTlsStream {
    Plain(TcpStream),
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
    Http(TunnelStream),
}

#[derive(Debug, Clone, PartialEq)]
pub enum AuthMethod {
    Basic,
    Digest,
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn add_auth_header(
    req: &mut Request<Body>,
    username: &str,
    password: &str,
    auth_method: &AuthMethod,
    digest_params: Option<DigestParams>,
    method: &Method,
    uri: &Url,
    nonce_count: u32,
) -> Result<(), RtspError> {
    if username.is_empty() && password.is_empty() {
        return Err(RtspError::Fatal("Missing username/password".to_string()));
    }

    match auth_method {
        AuthMethod::Basic => {
            let credentials = format!("{}:{}", username, password);
            let encoded = data_encoding::BASE64.encode(credentials.as_bytes());
            req.insert_header(AUTHORIZATION, format!("Basic {}", encoded));
        }
        AuthMethod::Digest => {
            use rand::prelude::*;

            let Some(params) = digest_params else {
                return Err(RtspError::Fatal("Missing Digest".to_string()));
            };

            let cnonce = {
                let mut bytes = [0u8; 8];
                rand::rng().fill_bytes(&mut bytes);
                hex::encode(bytes)
            };
            let nc = format!("{:08x}", nonce_count + 1);

            let response = {
                let response =
                    compute_digest_response(&params, method, uri, username, password, &cnonce, &nc);

                let mut parts = vec![
                    format!("username=\"{username}\""),
                    format!("realm=\"{}\"", params.realm),
                    format!("nonce=\"{}\"", params.nonce),
                    format!("uri=\"{uri}\""),
                    format!("response=\"{response}\""),
                ];

                if let Some(algorithm) = params.algorithm {
                    parts.push(format!("algorithm={}", algorithm));
                }

                if let Some(qop) = &params.qop {
                    parts.push(format!("qop={}", qop));
                    parts.push(format!("cnonce=\"{}\"", cnonce));
                    parts.push(format!("nc={}", nc));
                }

                if let Some(opaque) = &params.opaque {
                    parts.push(format!("opaque=\"{}\"", opaque));
                }

                parts.join(", ")
            };

            req.insert_header(AUTHORIZATION, format!("Digest {}", response));
        }
    }

    Ok(())
}

#[allow(clippy::result_large_err)]
fn rebuild_request_with_auth(
    mut req: Request<Body>,
    auth_method: &AuthMethod,
    digest_params: Option<DigestParams>,
    username: &str,
    password: &str,
    nonce_count: u32,
) -> Result<Request<Body>, RtspError> {
    req.remove_header(&AUTHORIZATION);

    let url = req.request_uri().unwrap().clone();
    let method = req.method().clone();

    add_auth_header(
        &mut req,
        username,
        password,
        auth_method,
        digest_params,
        &method,
        &url,
        nonce_count,
    )?;

    Ok(req)
}

fn reset_nonce(
    new_params: Option<&DigestParams>,
    old_params: Option<&DigestParams>,
    current_nonce_count: u32,
) -> bool {
    new_params
        .zip(old_params)
        .is_some_and(|(n, o)| n.nonce != o.nonce && current_nonce_count != 1)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RtspProtocol {
    UdpMulticast,
    Udp,
    Tcp,
}

impl fmt::Display for RtspProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            RtspProtocol::Udp => write!(f, "udp"),
            RtspProtocol::UdpMulticast => write!(f, "udp-mcast"),
            RtspProtocol::Tcp => write!(f, "tcp"),
        }
    }
}

fn rewrite_url_scheme(url: Url, new_scheme: &str) -> Url {
    match url.scheme() {
        "rtsph" => {
            let mut url = url;
            let _ = url.set_scheme(new_scheme);
            url
        }
        _ => url,
    }
}

fn rtsp_url(url: Url) -> Url {
    rewrite_url_scheme(url, "rtsp")
}

fn rtsp_http_url(url: Url) -> Url {
    rewrite_url_scheme(url, "http")
}

fn rtsp_task_state<R, W>(url: Url, read: R, write: W) -> RtspTaskState
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    use super::tcp_message::{async_read, async_write};

    let stream = Box::pin(async_read(read, MAX_MESSAGE_SIZE).fuse());
    let sink = Box::pin(async_write(write));

    RtspTaskState::new(url, stream, sink)
}

fn is_secure_rtp_profile(profile: &RtpProfile) -> bool {
    *profile == RtpProfile::SAvp || *profile == RtpProfile::SAvpF
}

fn reply_with_promise(
    promise: gst::Promise,
    reply_name: &str,
    status_code: StatusCode,
    reason: &str,
    body: Option<String>,
) {
    let mut reply = gst::Structure::builder(reply_name)
        .field("rtsp-code", u16::from(status_code) as u32)
        .field("rtsp-reason", reason)
        // We do not define result codes as C `rtspsrc` does. This
        // field is mostly for compatibility with `rtspsrc`. See
        // `GstRTSPResult`.
        .field(
            "rtsp-result",
            if status_code.is_success() {
                0i32
            } else {
                -1i32
            },
        )
        .build();

    if let Some(body) = body
        && reply_name == GET_PARAMETER_REPLY
    {
        reply.set("body", body)
    }

    promise.reply(Some(reply));
}

fn get_content_type(content: Option<String>) -> String {
    match content {
        Some(c) => c,
        None => "text/parameters".to_string(),
    }
}

fn parameter_method_to_reply(method: &Method) -> &str {
    match method {
        Method::GetParameter => GET_PARAMETER_REPLY,
        Method::SetParameter => SET_PARAMETER_REPLY,
        _ => unreachable!(),
    }
}

fn session_not_found(param_req: ParameterRequest) {
    let reply = parameter_method_to_reply(&param_req.method);

    reply_with_promise(
        param_req.promise,
        reply,
        StatusCode::SessionNotFound,
        &StatusCode::SessionNotFound.to_string(),
        None,
    );
}

#[derive(Debug, Clone)]
struct Settings {
    location: Option<Url>,
    port_start: u16,
    protocols: Vec<RtspProtocol>,
    timeout: gst::ClockTime,
    receive_mtu: u32,
    certificate_file: Option<PathBuf>,
    private_key_file: Option<PathBuf>,
    extra_headers: Vec<(String, String)>,
    do_rtsp_keep_alive: bool,
    latency: u32,
    tls_validation: RtspSrc2TlsValidationFlags,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            location: DEFAULT_LOCATION,
            port_start: DEFAULT_PORT_START,
            timeout: DEFAULT_TIMEOUT,
            protocols: parse_protocols_str(DEFAULT_PROTOCOLS).unwrap(),
            receive_mtu: DEFAULT_RECEIVE_MTU,
            certificate_file: None,
            private_key_file: None,
            extra_headers: Vec::new(),
            do_rtsp_keep_alive: DEFAULT_DO_RTSP_KEEP_ALIVE,
            latency: DEFAULT_LATENCY_MS,
            tls_validation: DEFAULT_TLS_VALIDATION,
        }
    }
}

#[derive(Debug)]
struct ParameterRequest {
    method: Method,
    body: Vec<u8>,
    content_type: String,
    promise: gst::Promise,
}

#[derive(Debug)]
enum Commands {
    Play,
    //Pause,
    Teardown(Option<oneshot::Sender<()>>),
    Data(rtsp_types::Data<Body>),
    KeepAlive,
    GetParameter(ParameterRequest),
    SetParameter(ParameterRequest),
}

#[derive(Debug, Clone)]
struct RtspGstStream {
    stream: gst::Stream,
    media_idx: usize,
    has_mikey: bool,
}

#[derive(Debug, Default)]
struct State {
    streams_aware: bool,
    selection_seqnum: Option<gst::Seqnum>,
    streams: Vec<RtspGstStream>,
    stream_collection: Option<gst::StreamCollection>,
    selected_streams: Option<Vec<RtspGstStream>>,
    stream_selection_done: bool,
    flow_combiner: gst_base::UniqueFlowCombiner,
    emitted_no_more_pads: bool,
    srtpdec: HashMap<u32, gst::Element>,
    srtpenc: HashMap<u32, gst::Element>,
}

impl State {
    fn default_streams(&self) -> Vec<RtspGstStream> {
        if !self.streams_aware {
            // Not STREAMS_AWARE, expose all streams.
            self.streams.clone()
        } else {
            // STREAMS_AWARE, expose one audio and one video stream.
            [gst::StreamType::VIDEO, gst::StreamType::AUDIO]
                .iter()
                .flat_map(|&t| self.streams.iter().find(|s| s.stream.stream_type() == t))
                .cloned()
                .collect::<Vec<_>>()
        }
    }

    fn all_gst_streams(&self) -> Vec<gst::Stream> {
        self.streams
            .iter()
            .map(|s| s.stream.clone())
            .collect::<Vec<gst::Stream>>()
    }

    fn set_selected_streams(&mut self, selected_stream_ids: Option<Vec<glib::GString>>) {
        match selected_stream_ids {
            Some(ids) => {
                let selected_streams = self
                    .streams
                    .iter()
                    .filter(|s| {
                        let id = s.stream.stream_id().unwrap();
                        ids.contains(&id)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                self.selected_streams = Some(selected_streams);
            }
            None => {
                self.selected_streams = Some(self.default_streams());
            }
        }
    }

    fn retrieve_selected_streams(&mut self) -> Vec<gst::Stream> {
        self.selected_streams
            .iter()
            .flat_map(|streams| streams.iter().map(|s| s.stream.clone()))
            .collect()
    }

    // Returns `true` if active streams is equal to the number of source pads
    fn should_post_stream_selection(&self, num_src_pads: usize) -> bool {
        self.selected_streams
            .as_ref()
            .map(|streams| streams.len() == num_src_pads)
            .unwrap_or(false)
    }
}

#[derive(Debug, Default)]
pub struct RtspSrc {
    settings: Mutex<Settings>,
    task_handle: Mutex<Option<JoinHandle<()>>>,
    command_queue: Mutex<Option<mpsc::Sender<Commands>>>,
    state: Mutex<State>,
}

#[derive(thiserror::Error, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum RtspError {
    #[error("Generic I/O error")]
    IOGeneric(#[from] std::io::Error),
    #[error("Read I/O error")]
    Read(#[from] super::tcp_message::ReadError),
    #[error("RTSP header parse error")]
    HeaderParser(#[from] rtsp_types::headers::HeaderParseError),
    #[error("SDP parse error")]
    SDPParser(#[from] sdp_types::ParserError),
    #[error("Unexpected RTSP message: expected, received")]
    UnexpectedMessage(&'static str, rtsp_types::Message<Body>),
    #[error("Invalid RTSP message")]
    InvalidMessage(&'static str),
    #[error("Fatal error")]
    Fatal(String),
    #[error("Authentication challenge")]
    AuthChallenge(AuthMethod, Option<String>),
    #[error("Parameter Error")]
    Parameter(StatusCode, String),
}

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rtspsrc2",
        gst::DebugColorFlags::empty(),
        Some("RTSP source"),
    )
});

static RUNTIME: LazyLock<runtime::Runtime> = LazyLock::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .build()
        .unwrap()
});

fn parse_protocols_str(s: &str) -> Result<Vec<RtspProtocol>, glib::Error> {
    let mut acc = Vec::new();
    if s.is_empty() {
        return Err(glib::Error::new(
            gst::CoreError::Failed,
            "Protocols list is empty",
        ));
    }
    for each in s.split(',') {
        match each {
            "udp-mcast" => acc.push(RtspProtocol::UdpMulticast),
            "udp" => acc.push(RtspProtocol::Udp),
            "tcp" => acc.push(RtspProtocol::Tcp),
            _ => {
                return Err(glib::Error::new(
                    gst::CoreError::Failed,
                    &format!("Unsupported RTSP protocol: {each}"),
                ));
            }
        }
    }
    Ok(acc)
}

impl RtspSrc {
    fn set_location(&self, uri: Option<&str>) -> Result<(), glib::Error> {
        if self.obj().current_state() > gst::State::Ready {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Changing the 'location' property on a started 'rtspsrc2' is not supported",
            ));
        }

        let mut settings = self.settings.lock().unwrap();

        let Some(uri) = uri else {
            settings.location = DEFAULT_LOCATION;
            return Ok(());
        };

        let uri = Url::parse(uri).map_err(|err| {
            glib::Error::new(
                gst::URIError::BadUri,
                &format!("Failed to parse URI '{uri}': {err:?}"),
            )
        })?;

        match (uri.host_str(), uri.port()) {
            (Some(_), Some(_)) | (Some(_), None) => Ok(()),
            _ => Err(glib::Error::new(gst::URIError::BadUri, "Invalid host")),
        }?;

        let protocols: &[RtspProtocol] = match uri.scheme() {
            "rtspu" => &[RtspProtocol::UdpMulticast, RtspProtocol::Udp],
            "rtspt" => &[RtspProtocol::Tcp],
            "rtsp" => &settings.protocols,
            "rtsps" => &settings.protocols,
            "rtsph" => &[RtspProtocol::Tcp],
            scheme => {
                return Err(glib::Error::new(
                    gst::URIError::UnsupportedProtocol,
                    &format!("Unsupported URI scheme '{scheme}'"),
                ));
            }
        };

        if !settings.protocols.iter().any(|p| protocols.contains(p)) {
            return Err(glib::Error::new(
                gst::URIError::UnsupportedProtocol,
                &format!(
                    "URI scheme '{}' does not match allowed protocols: {:?}",
                    uri.scheme(),
                    settings.protocols,
                ),
            ));
        }

        settings.protocols = protocols.to_vec();
        settings.location = Some(uri);

        Ok(())
    }

    fn set_protocols(&self, protocol_s: Option<&str>) -> Result<(), glib::Error> {
        if self.obj().current_state() > gst::State::Ready {
            return Err(glib::Error::new(
                gst::CoreError::Failed,
                "Changing the 'protocols' property on a started 'rtspsrc2' is not supported",
            ));
        }

        let mut settings = self.settings.lock().unwrap();

        settings.protocols = match protocol_s {
            Some(s) => parse_protocols_str(s)?,
            None => parse_protocols_str(DEFAULT_PROTOCOLS).unwrap(),
        };

        Ok(())
    }

    fn is_parent_streams_aware(&self) -> bool {
        if let Some(parent) = self
            .obj()
            .parent()
            .and_then(|parent| parent.downcast::<gst::Bin>().ok())
        {
            return parent.bin_flags().contains(gst::BinFlags::STREAMS_AWARE);
        }

        false
    }

    fn handle_event(&self, pad: &gst::GhostPad, event: gst::Event) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::SelectStreams(select_streams) => {
                gst::debug!(
                    CAT,
                    imp = self,
                    "Handling stream selection event on {pad:?}"
                );
                self.handle_select_streams_event(select_streams)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn proxy_pad_chain(
        &self,
        pad: &gst::ProxyPad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let ret = gst::ProxyPad::chain_default(pad, Some(&*self.obj()), buffer);
        let (ret, streams_aware, emitted_no_more_pads) = {
            let mut state = self.state.lock().unwrap();
            (
                state.flow_combiner.update_pad_flow(pad, ret),
                state.streams_aware,
                state.emitted_no_more_pads,
            )
        };

        if let Err(res) = ret
            && res == gst::FlowError::NotLinked
            && (streams_aware || !emitted_no_more_pads)
        {
            // Ignore not-linked errors until we've added all pads,
            // as downstream might only link one pad they are interested in.
            return Ok(gst::FlowSuccess::Ok);
        }

        ret
    }

    fn proxy_pad_chain_list(
        &self,
        pad: &gst::ProxyPad,
        bufferlist: gst::BufferList,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let ret = gst::ProxyPad::chain_list_default(pad, Some(&*self.obj()), bufferlist);
        let (ret, streams_aware, emitted_no_more_pads) = {
            let mut state = self.state.lock().unwrap();
            (
                state.flow_combiner.update_pad_flow(pad, ret),
                state.streams_aware,
                state.emitted_no_more_pads,
            )
        };

        if let Err(res) = ret
            && res == gst::FlowError::NotLinked
            && (streams_aware || !emitted_no_more_pads)
        {
            // Ignore not-linked errors until we've added all pads,
            // as downstream might only link one pad they are interested in.
            return Ok(gst::FlowSuccess::Ok);
        }

        ret
    }

    fn post_streams_selected(&self, num_src_pads: usize) {
        let mut state = self.state.lock().unwrap();

        if state.should_post_stream_selection(num_src_pads) {
            let Some(collection) = state.stream_collection.take() else {
                return;
            };
            let selected = state.retrieve_selected_streams();

            drop(state);

            if !selected.is_empty() {
                gst::debug!(CAT, imp = self, "Posting StreamsSelected {selected:?}");

                let msg = gst::message::StreamsSelected::builder(&collection)
                    .streams(&selected)
                    .src(&*self.obj())
                    .build();
                let _ = self.obj().post_message(msg);
            }
        }
    }
}

impl ObjectImpl for RtspSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("receive-mtu")
                    .nick("Receive packet size")
                    .blurb("Initial size of buffers to allocate in the buffer pool, will be increased if too small")
                    .default_value(DEFAULT_RECEIVE_MTU)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("RTSP server, credentials and media path, e.g. rtsp://user:p4ssw0rd@camera-5.local:8554/h264_1080p30")
                    .mutable_ready()
                    .build(),
                // We purposely use port-start instead of port-range (like in rtspsrc), because
                // there is no way for the user to know how many ports we actually need. It depends
                // on how many streams the media contains, and whether the server wants RTCP or
                // RTCP-mux, or no RTCP. This property can be used to specify the start of the
                // valid range, and if the user wants to know how many ports were used, we can
                // add API for that later.
                glib::ParamSpecUInt::builder("port-start")
                    .nick("Port start")
                    .blurb("Port number to start allocating client ports for receiving RTP and RTCP data, eg. 3000 (0 = automatic selection)")
                    .default_value(DEFAULT_PORT_START.into())
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("protocols")
                    .nick("Protocols")
                    .blurb("Allowed lower transport protocols, in order of preference")
                    .default_value("udp-mcast,udp,tcp")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("timeout")
                    .nick("Timeout")
                    .blurb("Timeout for network activity, in nanoseconds")
                    .maximum(gst::ClockTime::MAX.into())
                    .default_value(DEFAULT_TIMEOUT.into())
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("certificate-file")
                    .nick("Certificate file for client authentication")
                    .blurb("Path to certificate chain for the private key file in PEM format")
                    .build(),
                glib::ParamSpecString::builder("private-key-file")
                    .nick("Private key file for client authentication")
                    .blurb("Path to a PKCS1, PKCS8 or SEC1 private key file in PEM format")
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("extra-http-request-headers")
                    .nick("Extra Headers")
                    .blurb("Extra HTTP headers to send with requests")
                    .write_only()
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("do-rtsp-keep-alive")
                    .nick("Do RTSP Keep Alive")
                    .blurb("Send RTSP keep alive packets, disable for old incompatible server.")
                    .default_value(DEFAULT_DO_RTSP_KEEP_ALIVE)
                    .build(),
                glib::ParamSpecUInt::builder("latency")
                    .nick("Buffer latency in ms")
                    .blurb("Amount of ms to buffer")
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_LATENCY_MS)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecFlags::builder::<RtspSrc2TlsValidationFlags>("tls-validation-flags")
                    .nick("TLS certificate validation")
                    .blurb("TLS certificate validation flags used to validate the server certificate")
                    .default_value(DEFAULT_TLS_VALIDATION)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let res = match pspec.name() {
            "receive-mtu" => {
                let mut settings = self.settings.lock().unwrap();
                settings.receive_mtu = value.get::<u32>().expect("type checked upstream");
                Ok(())
            }
            "location" => {
                let location = value.get::<Option<&str>>().expect("type checked upstream");
                self.set_location(location)
            }
            "port-start" => {
                let mut settings = self.settings.lock().unwrap();
                let start = value.get::<u32>().expect("type checked upstream");
                match u16::try_from(start) {
                    Ok(start) => {
                        settings.port_start = start;
                        Ok(())
                    }
                    Err(err) => Err(glib::Error::new(
                        gst::CoreError::Failed,
                        &format!("Failed to set port start: {err:?}"),
                    )),
                }
            }
            "protocols" => {
                let protocols = value.get::<Option<&str>>().expect("type checked upstream");
                self.set_protocols(protocols)
            }
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let timeout = value.get().expect("type checked upstream");
                settings.timeout = timeout;
                Ok(())
            }
            "certificate-file" => {
                let mut settings = self.settings.lock().unwrap();
                let value: String = value.get().unwrap();
                settings.certificate_file = Some(value.into());
                Ok(())
            }
            "private-key-file" => {
                let mut settings = self.settings.lock().unwrap();
                let value: String = value.get().unwrap();
                settings.private_key_file = Some(value.into());
                Ok(())
            }
            "extra-headers" => {
                let mut settings = self.settings.lock().unwrap();
                let s = value
                    .get::<gst::Structure>()
                    .expect("type checked upstream");

                for (k, v) in s.iter() {
                    if let Ok(v) = v.get::<String>() {
                        settings.extra_headers.push((k.to_string(), v));
                    }
                }

                Ok(())
            }
            "do-rtsp-keep-alive" => {
                let mut settings = self.settings.lock().unwrap();
                settings.do_rtsp_keep_alive = value.get::<bool>().expect("type checked upstream");
                Ok(())
            }
            "latency" => {
                let mut settings = self.settings.lock().unwrap();
                settings.latency = value.get::<u32>().expect("type checked upstream");
                Ok(())
            }
            "tls-validation-flags" => {
                let mut settings = self.settings.lock().unwrap();
                settings.tls_validation = value
                    .get::<RtspSrc2TlsValidationFlags>()
                    .expect("type checked upstream");
                Ok(())
            }
            name => unimplemented!("Property '{name}'"),
        };

        if let Err(err) = res {
            gst::error!(
                CAT,
                imp = self,
                "Failed to set property `{}`: {:?}",
                pspec.name(),
                err
            );
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "receive-mtu" => {
                let settings = self.settings.lock().unwrap();
                settings.receive_mtu.to_value()
            }
            "location" => {
                let settings = self.settings.lock().unwrap();
                let location = settings.location.as_ref().map(Url::to_string);

                location.to_value()
            }
            "port-start" => {
                let settings = self.settings.lock().unwrap();
                (settings.port_start as u32).to_value()
            }
            "protocols" => {
                let settings = self.settings.lock().unwrap();
                (settings
                    .protocols
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","))
                .to_value()
            }
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            "certificate-file" => {
                let settings = self.settings.lock().unwrap();
                let certfile = settings.certificate_file.as_ref();
                certfile.to_value()
            }
            "private-key-file" => {
                let settings = self.settings.lock().unwrap();
                let privkey = settings.private_key_file.as_ref();
                privkey.to_value()
            }
            "do-rtsp-keep-alive" => {
                let settings = self.settings.lock().unwrap();
                settings.do_rtsp_keep_alive.to_value()
            }
            "latency" => {
                let settings = self.settings.lock().unwrap();
                settings.latency.to_value()
            }
            "tls-validation-flags" => {
                let settings = self.settings.lock().unwrap();
                settings.tls_validation.to_value()
            }
            name => unimplemented!("Property '{name}'"),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![
                // GstRtspSrc2::tls-client-auth:
                //
                // Returns: a #GstStructure. The returned #GstStructure should contain:
                //
                // "certificate-file" field of type string having path to certificate file
                // "private-key-file" field of type string having path to private key file
                glib::subclass::Signal::builder(SIGNAL_TLS_CLIENT_AUTH)
                    .return_type::<Option<gst::Structure>>()
                    .action()
                    .class_handler(|_| Some(None::<gst::Structure>.to_value()))
                    .build(),
                // GstRtspSrc2::get-parameter:
                //
                // @rtspsrc: a #GstRtspSrc2
                // @parameter: the parameter name
                // @parameter: the content type
                // @parameter: a pointer to #GstPromise
                //
                // Handle the GET_PARAMETER signal.
                //
                // The #GstPromise reply consists in the following fields:
                //
                // * 'rtsp-result': set to 0 if the RTSP request could be processed.
                // * 'rtsp-code': the HTTP status code returned by the server.
                // * 'rtsp-reason': a human-readable version of the HTTP status code.
                //
                // Returns: %TRUE when the command could be issued, %FALSE otherwise
                glib::subclass::Signal::builder(SIGNAL_GET_PARAMETER)
                    .param_types([
                        String::static_type(),
                        Option::<String>::static_type(),
                        gst::Promise::static_type(),
                    ])
                    .return_type::<bool>()
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::RtspSrc>().unwrap();
                        let parameter = args[1].get::<String>().unwrap();
                        let content_type = args[2].get::<Option<String>>().unwrap();
                        let promise = args[3].get::<gst::Promise>().unwrap();

                        Some((this.imp().get_parameter(parameter, content_type, promise)).into())
                    })
                    .build(),
                // GstRtspSrc2::get-parameters:
                //
                // @rtspsrc: a #GstRtspSrc2
                // @parameter: a NULL-terminated array of parameters
                // @parameter: the content type
                // @parameter: a pointer to #GstPromise
                //
                // Handle the GET_PARAMETERS signal.
                //
                // The #GstPromise reply consists in the following fields:
                //
                // * 'rtsp-result': set to 0 if the RTSP request could be processed.
                // * 'rtsp-code': the HTTP status code returned by the server.
                // * 'rtsp-reason': a human-readable version of the HTTP status code.
                //
                // Returns: %TRUE when the command could be issued, %FALSE otherwise
                glib::subclass::Signal::builder(SIGNAL_GET_PARAMETERS)
                    .param_types([
                        glib::StrV::static_type(),
                        Option::<String>::static_type(),
                        gst::Promise::static_type(),
                    ])
                    .return_type::<bool>()
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::RtspSrc>().unwrap();
                        let parameters = args[1].get::<Vec<String>>().unwrap();
                        let content_type = args[2].get::<Option<String>>().unwrap();
                        let promise = args[3].get::<gst::Promise>().unwrap();

                        Some((this.imp().get_parameters(parameters, content_type, promise)).into())
                    })
                    .build(),
                // GstRtspSrc2::set-parameter:
                //
                // @rtspsrc: a #GstRtspSrc2
                // @parameter: the parameter name
                // @parameter: the parameter value
                // @parameter: the content type
                // @parameter: a pointer to #GstPromise
                //
                // Handle the SET_PARAMETER signal.
                //
                // The #GstPromise reply consists in the following fields:
                //
                // * 'rtsp-result': set to 0 if the RTSP request could be processed.
                // * 'rtsp-code': the HTTP status code returned by the server.
                // * 'rtsp-reason': a human-readable version of the HTTP status code.
                //
                // Returns: %TRUE when the command could be issued, %FALSE otherwise
                glib::subclass::Signal::builder(SIGNAL_SET_PARAMETER)
                    .param_types([
                        String::static_type(),
                        String::static_type(),
                        Option::<String>::static_type(),
                        gst::Promise::static_type(),
                    ])
                    .return_type::<bool>()
                    .action()
                    .class_handler(move |args| {
                        let this = args[0].get::<super::RtspSrc>().unwrap();
                        let parameter_name = args[1].get::<String>().unwrap();
                        let parameter_value = args[2].get::<String>().unwrap();
                        let content_type = args[3].get::<Option<String>>().unwrap();
                        let promise = args[4].get::<gst::Promise>().unwrap();

                        Some(
                            (this.imp().set_parameter(
                                parameter_name,
                                parameter_value,
                                content_type,
                                promise,
                            ))
                            .into(),
                        )
                    })
                    .build(),
            ]
        });

        SIGNALS.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        obj.set_element_flags(gst::ElementFlags::SOURCE);
        obj.set_bin_flags(gst::BinFlags::STREAMS_AWARE);
    }
}

impl GstObjectImpl for RtspSrc {}

impl ElementImpl for RtspSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "RTSP Source",
                "Source/Network",
                "Receive audio or video from a network device via the Real Time Streaming Protocol (RTSP) (RFC 2326, 7826)",
                "Nirbheek Chauhan <nirbheek centricular com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "stream_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_empty_simple("application/x-rtp"),
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                self.start().map_err(|err_msg| {
                    self.post_error_message(err_msg);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.lock().unwrap();
                state.streams_aware = self.is_parent_streams_aware();
            }
            gst::StateChange::PausedToPlaying => {
                let cmd_queue = self.cmd_queue();
                //self.async_start().map_err(|_| gst::StateChangeError)?;
                RUNTIME.spawn(async move { cmd_queue.send(Commands::Play).await });
            }
            _ => {}
        }

        let mut ret = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PlayingToPaused => {
                ret = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                match tokio::runtime::Handle::try_current() {
                    Ok(_) => {
                        // If the app does set_state(NULL) from a block_on() inside its own tokio
                        // runtime, calling block_on() on our own runtime will cause a panic
                        // because of nested blocking calls. So, shutdown the task from another
                        // thread.
                        // The app's usage is also incorrect since they are blocking the runtime
                        // on I/O, so emit a warning.
                        gst::warning!(
                            CAT,
                            "Blocking I/O: state change to NULL called from an async \
                            tokio context, redirecting to another thread to prevent \
                            the tokio panic, but you should refactor your code to \
                            make use of gst::Element::call_async and set the state \
                            to NULL from there, without blocking the runtime"
                        );
                        let (tx, rx) = std::sync::mpsc::channel();
                        self.obj().call_async(move |element| {
                            tx.send(element.imp().stop()).unwrap();
                        });
                        rx.recv().unwrap()
                    }
                    Err(_) => self.stop(),
                }
                .map_err(|err_msg| {
                    self.post_error_message(err_msg);
                    gst::StateChangeError
                })?;
            }
            _ => (),
        }

        Ok(ret)
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Selectable(selectable) => {
                gst::debug!(CAT, imp = self, "Handling selectable query");
                selectable.set_selectable(true);
                true
            }
            _ => self.parent_query(query),
        }
    }

    fn send_event(&self, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::SelectStreams(e) => self.handle_select_streams_event(e),
            _ => self.parent_send_event(event),
        }
    }
}

impl BinImpl for RtspSrc {}

impl URIHandlerImpl for RtspSrc {
    const URI_TYPE: gst::URIType = gst::URIType::Src;

    fn protocols() -> &'static [&'static str] {
        &["rtsp", "rtspu", "rtspt", "rtsph", "rtsps"]
    }

    fn uri(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        settings.location.as_ref().map(Url::to_string)
    }

    fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
        self.set_location(Some(uri))
    }
}

type RtspStream =
    Pin<Box<dyn Stream<Item = Result<Message<Body>, super::tcp_message::ReadError>> + Send>>;
type RtspSink = Pin<Box<dyn Sink<Message<Body>, Error = std::io::Error> + Send>>;

impl RtspSrc {
    #[track_caller]
    fn cmd_queue(&self) -> mpsc::Sender<Commands> {
        self.command_queue.lock().unwrap().as_ref().unwrap().clone()
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let (url, tls_enabled, cert, key, tunnel_enabled, extra_headers, tls_validation) = {
            let settings = self.settings.lock().unwrap();
            let Some(url) = settings.location.clone() else {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["No location set"]
                ));
            };
            let tls_enabled = url.scheme() == "rtsps";
            let tunnel_enabled = url.scheme() == "rtsph";
            let certificate_file = settings.certificate_file.clone();
            let private_key_file = settings.private_key_file.clone();
            let extra_headers = settings.extra_headers.clone();
            let tls_validation = settings.tls_validation;

            (
                url,
                tls_enabled,
                certificate_file,
                private_key_file,
                tunnel_enabled,
                extra_headers,
                tls_validation,
            )
        };

        gst::info!(CAT, imp = self, "Location: {url}",);

        gst::info!(CAT, imp = self, "Starting RTSP connection thread.. ");

        let task_src = self.ref_counted();

        let mut task_handle = self.task_handle.lock().unwrap();

        let (tx, rx) = mpsc::channel(1);
        {
            let mut cmd_queue_opt = self.command_queue.lock().unwrap();
            debug_assert!(cmd_queue_opt.is_none());
            cmd_queue_opt.replace(tx);
        }

        let self_weak = self.obj().downgrade();

        let mut rng = rand::rng();
        let session_id = Alphanumeric
            .sample_iter(&mut rng)
            .take(24)
            .map(char::from)
            .collect::<String>();

        let join_handle = RUNTIME.spawn(async move {
            gst::info!(CAT, "Connecting to {url} ..");

            let s = if tunnel_enabled {
                gst::info!(CAT, "Using session id: {session_id}");

                let http_url = rtsp_http_url(url.clone());
                let tunnel = match TunnelStream::new(
                    http_url,
                    &extra_headers,
                    session_id,
                    DEFAULT_USER_AGENT,
                )
                .await
                {
                    Ok(s) => s,
                    Err(err) => {
                        gst::element_imp_error!(
                            task_src,
                            gst::ResourceError::OpenRead,
                            ["Failed to connect to RTSP server: {err:#?}"]
                        );
                        return;
                    }
                };

                TcpOrTlsStream::Http(tunnel)
            } else {
                let hostname = url.clone().host_str().unwrap().to_string();
                let port = url.port().unwrap_or(if tls_enabled { 322 } else { 554 });
                let hostname_port = format!("{hostname}:{port}");

                let stream = match TcpStream::connect(hostname_port).await {
                    Ok(s) => s,
                    Err(err) => {
                        gst::element_imp_error!(
                            task_src,
                            gst::ResourceError::OpenRead,
                            ["Failed to connect to RTSP server: {err:#?}"]
                        );
                        return;
                    }
                };
                let _ = stream.set_nodelay(true);

                if tls_enabled {
                    let dnsname = match ServerName::try_from(hostname.clone()) {
                        Ok(dnsname) => dnsname,
                        Err(err) => {
                            gst::element_imp_error!(
                                task_src,
                                gst::ResourceError::Write,
                                ["Server name failed for '{hostname}': {err}"]
                            );
                            return;
                        }
                    };

                    let connector = match create_tls_connector(self_weak, cert, key, tls_validation)
                    {
                        Ok(c) => c,
                        Err(err) => {
                            gst::element_imp_error!(
                                task_src,
                                gst::ResourceError::Read,
                                ["Failed to create TlsConnector: {err}"]
                            );
                            return;
                        }
                    };

                    let s = match connector.connect(dnsname, stream).await {
                        Ok(s) => s,
                        Err(err) => {
                            gst::element_imp_error!(
                                task_src,
                                gst::ResourceError::OpenWrite,
                                ["Failed TLS connect to server {hostname}:{port}: {err}"]
                            );
                            return;
                        }
                    };

                    TcpOrTlsStream::Tls(s)
                } else {
                    TcpOrTlsStream::Plain(stream)
                }
            };

            gst::info!(CAT, "Connected!");

            let (state, task_ret) = match s {
                TcpOrTlsStream::Plain(tcp_stream) => {
                    let (read, write) = tcp_stream.into_split();

                    let mut state = rtsp_task_state(url, read, write);
                    let task_ret = task_src.rtsp_task(&mut state, rx).await;

                    (state, task_ret)
                }
                TcpOrTlsStream::Tls(tls_stream) => {
                    let (read, write) = tokio::io::split(tls_stream);

                    let mut state = rtsp_task_state(url, read, write);
                    let task_ret = task_src.rtsp_task(&mut state, rx).await;

                    (state, task_ret)
                }
                TcpOrTlsStream::Http(tunnel_stream) => {
                    let (read, write) = tokio::io::split(tunnel_stream);

                    let mut state = rtsp_task_state(url, read, write);
                    let task_ret = task_src.rtsp_task(&mut state, rx).await;

                    (state, task_ret)
                }
            };

            gst::info!(CAT, "Exited rtsp_task");

            // Cleanup after stopping
            if let Some(h) = state.keep_alive_task {
                h.abort()
            }
            for h in &state.handles {
                h.abort();
            }
            for h in state.handles {
                let _ = h.await;
            }
            let obj = task_src.obj();
            for e in obj.iterate_sorted() {
                let Ok(e) = e else {
                    continue;
                };
                if let Err(err) = e.set_state(gst::State::Null) {
                    gst::warning!(CAT, "{} failed to go to Null state: {err:?}", e.name());
                }
            }
            for pad in obj.src_pads() {
                if let Err(err) = obj.remove_pad(&pad) {
                    gst::warning!(CAT, "Failed to remove pad {}: {err:?}", pad.name());
                }
            }
            for e in obj.iterate_sorted() {
                let Ok(e) = e else {
                    continue;
                };
                if let Err(err) = obj.remove(&e) {
                    gst::warning!(CAT, "Failed to remove element {}: {err:?}", e.name());
                }
            }

            // Post the element error after cleanup
            if let Err(err) = task_ret {
                gst::element_imp_error!(
                    task_src,
                    gst::CoreError::Failed,
                    ["RTSP task exited: {err:#?}"]
                );
            }
            gst::info!(CAT, "Cleanup complete");
        });

        debug_assert!(task_handle.is_none());
        task_handle.replace(join_handle);

        gst::info!(CAT, imp = self, "Started");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::info!(CAT, "Stopping...");
        let cmd_queue = self.cmd_queue();
        let task_handle = { self.task_handle.lock().unwrap().take() };

        RUNTIME.block_on(async {
            let (tx, rx) = oneshot::channel();
            if let Ok(()) = cmd_queue.send(Commands::Teardown(Some(tx))).await
                && let Err(_elapsed) = time::timeout(Duration::from_millis(500), rx).await
            {
                gst::warning!(
                    CAT,
                    "Timeout waiting for Teardown, going to NULL asynchronously"
                );
            }
        });

        if let Some(join_handle) = task_handle {
            gst::debug!(CAT, "Waiting for RTSP connection thread to shut down..");
            let _ = RUNTIME.block_on(join_handle);
        }

        self.command_queue.lock().unwrap().take();

        let mut state = self.state.lock().unwrap();
        state.flow_combiner.reset();

        state.srtpdec.clear();
        state.srtpenc.clear();

        gst::info!(CAT, imp = self, "Stopped");

        Ok(())
    }

    fn make_rtp_appsrc(
        &self,
        rtpsession_n: usize,
        caps: &gst::Caps,
        manager: &RtspManager,
    ) -> Result<gst_app::AppSrc> {
        let callbacks = gst_app::AppSrcCallbacks::builder()
            .enough_data(|appsrc| {
                gst::warning!(CAT, "appsrc {} is overrunning: enough data!", appsrc.name());
            })
            .build();
        let appsrc = gst_app::AppSrc::builder()
            .name(format!("rtp_appsrc_{rtpsession_n}"))
            .format(gst::Format::Time)
            .handle_segment_change(true)
            .caps(caps)
            .stream_type(gst_app::AppStreamType::Stream)
            .max_bytes(0)
            .max_buffers(0)
            .max_time(gst::ClockTime::from_seconds(2))
            .leaky_type(gst_app::AppLeakyType::Downstream)
            .callbacks(callbacks)
            .is_live(true)
            .build();
        let obj = self.obj();
        obj.add(&appsrc)?;
        appsrc
            .static_pad("src")
            .unwrap()
            .link(&manager.rtp_recv_sinkpad(rtpsession_n).unwrap())?;
        let templ = obj.pad_template("stream_%u").unwrap();
        let ghostpad = gst::GhostPad::builder_from_template(&templ)
            .name(format!("stream_{rtpsession_n}"))
            .proxy_pad_chain_function({
                move |pad, parent, buffer| {
                    let parent = parent.and_then(|p| p.parent());
                    RtspSrc::catch_panic_pad_function(
                        parent.as_ref(),
                        || Err(gst::FlowError::Error),
                        |imp| imp.proxy_pad_chain(pad, buffer),
                    )
                }
            })
            .proxy_pad_chain_list_function({
                move |pad, parent, bufferlist| {
                    let parent = parent.and_then(|p| p.parent());
                    RtspSrc::catch_panic_pad_function(
                        parent.as_ref(),
                        || Err(gst::FlowError::Error),
                        |imp| imp.proxy_pad_chain_list(pad, bufferlist),
                    )
                }
            })
            .event_function(move |pad, parent, event| {
                RtspSrc::catch_panic_pad_function(
                    parent,
                    || false,
                    |imp| imp.handle_event(pad, event),
                )
            })
            .build();
        gst::info!(CAT, "Adding ghost srcpad {}", ghostpad.name());
        obj.add_pad(&ghostpad)
            .expect("Adding a ghostpad should never fail");
        appsrc.sync_state_with_parent()?;

        // We do this here instead of when calling `remove_pad` to
        // avoid taking a reference to `self` in RTSP task. DO NOT
        // hold the `state` lock when calling `remove_pad`.
        obj.connect_pad_removed(glib::clone!(
            #[weak(rename_to = self_)]
            self,
            move |_, pad| {
                if pad.name().starts_with("stream_") {
                    let mut state = self_.state.lock().unwrap();
                    state.flow_combiner.remove_pad(pad);
                }
            }
        ));

        Ok(appsrc)
    }

    fn make_rtcp_appsrc(
        &self,
        rtpsession_n: usize,
        manager: &RtspManager,
        is_secure: bool,
    ) -> Result<gst_app::AppSrc> {
        let appsrc = gst_app::AppSrc::builder()
            .name(format!("rtcp_appsrc_{rtpsession_n}"))
            .format(gst::Format::Time)
            .handle_segment_change(true)
            .caps(if is_secure {
                &SRTP_RTCP_CAPS
            } else {
                &RTCP_CAPS
            })
            .stream_type(gst_app::AppStreamType::Stream)
            .is_live(true)
            .build();
        self.obj().add(&appsrc)?;
        appsrc
            .static_pad("src")
            .unwrap()
            .link(&manager.rtcp_recv_sinkpad(rtpsession_n).unwrap())?;
        appsrc.sync_state_with_parent()?;
        Ok(appsrc)
    }

    fn make_rtcp_appsink<
        F: FnMut(&gst_app::AppSink) -> Result<gst::FlowSuccess, gst::FlowError> + Send + 'static,
    >(
        &self,
        rtpsession_n: usize,
        manager: &RtspManager,
        on_rtcp: F,
    ) -> Result<()> {
        let cmd_tx_eos = self.cmd_queue();
        let cbs = gst_app::app_sink::AppSinkCallbacks::builder()
            .eos(move |_appsink| {
                let cmd_tx = cmd_tx_eos.clone();
                RUNTIME.spawn(async move {
                    let _ = cmd_tx.send(Commands::Teardown(None)).await;
                });
            })
            .new_sample(on_rtcp)
            .build();

        let rtcp_appsink = gst_app::AppSink::builder()
            .name(format!("rtcp_appsink_{rtpsession_n}"))
            .sync(false)
            .async_(false)
            .callbacks(cbs)
            .build();
        self.obj().add(&rtcp_appsink)?;
        manager
            .rtcp_send_srcpad(rtpsession_n)
            .unwrap()
            .link(&rtcp_appsink.static_pad("sink").unwrap())?;
        Ok(())
    }

    fn post_start(&self, code: &str, text: &str) {
        let obj = self.obj();
        let msg = gst::message::Progress::builder(gst::ProgressType::Start, code, text)
            .src(&*obj)
            .build();
        let _ = obj.post_message(msg);
    }

    fn post_complete(&self, code: &str, text: &str) {
        let obj = self.obj();
        let msg = gst::message::Progress::builder(gst::ProgressType::Complete, code, text)
            .src(&*obj)
            .build();
        let _ = obj.post_message(msg);
    }

    fn post_cancelled(&self, code: &str, text: &str) {
        let obj = self.obj();
        let msg = gst::message::Progress::builder(gst::ProgressType::Canceled, code, text)
            .src(&*obj)
            .build();
        let _ = obj.post_message(msg);
    }

    async fn rtsp_task(
        &self,
        task_state: &mut RtspTaskState,
        mut cmd_rx: mpsc::Receiver<Commands>,
    ) -> Result<()> {
        let cmd_tx = self.cmd_queue();

        let settings = { self.settings.lock().unwrap().clone() };

        // OPTIONS
        task_state.options().await?;

        // DESCRIBE
        task_state.describe().await?;

        task_state.set_aggregate_control();

        let streams_aware = {
            let streams = task_state.create_streams();

            let mut state = self.state.lock().unwrap();
            state.streams = streams;
            state.streams_aware
        };
        self.post_initial_collection();

        let self_weak = self.obj().downgrade();
        let mut session: Option<Session> = None;
        let mut session_timeout: Option<Duration> = None;

        // SETUP streams (TCP interleaved)
        task_state.setup_params = {
            task_state
                .setup(
                    self_weak,
                    &mut session,
                    &mut session_timeout,
                    settings.port_start,
                    &settings.protocols,
                    TransportMode::Play,
                )
                .await?
        };

        if settings.do_rtsp_keep_alive
            && let Some((_, timeout)) = session.as_ref().zip(session_timeout)
        {
            task_state.start_keep_alive_task(cmd_tx.clone(), timeout)
        }

        let using_rtp2 = std::env::var("USE_RTP2").is_ok_and(|s| s == "1");
        let manager = RtspManager::new(using_rtp2, settings.latency);

        if !manager.using_rtp2 {
            for (rtpsession_n, p) in task_state
                .setup_params
                .iter_mut()
                .enumerate()
                .filter(|(_, p)| is_secure_rtp_profile(&p.profile))
            {
                let stream_caps = p.caps.clone();

                gst::debug!(
                    CAT,
                    "Setting up RTP/RTCP decoder for SRTP for {rtpsession_n}"
                );

                for signal in ["request-rtp-decoder", "request-rtcp-decoder"] {
                    let stream_caps = p.caps.clone();

                    manager.recv.connect_closure(
                        signal,
                        false,
                        glib::closure!(
                            #[weak(rename_to = self_)]
                            self,
                            move |_rtpbin: gst::Element, session: u32| {
                                let srtpdec = {
                                    let mut state = self_.state.lock().unwrap();
                                    state
                                        .srtpdec
                                        .entry(session)
                                        .or_insert_with(|| {
                                            gst::ElementFactory::make_with_name(
                                                "srtpdec",
                                                Some(format!("srtpdec_{session}").as_str()),
                                            )
                                            .unwrap_or_else(|_| panic!("srtpdec not found"))
                                        })
                                        .clone()
                                };

                                let stream_caps = stream_caps.clone();

                                srtpdec.connect_closure(
                                    "request-key",
                                    false,
                                    glib::closure!(move |_srtpdec: gst::Element, _ssrc: u32| {
                                        stream_caps.clone()
                                    }),
                                );

                                srtpdec
                            }
                        ),
                    );
                }

                manager.recv.connect_closure(
                    "request-rtcp-encoder",
                    false,
                    glib::closure!(
                        #[weak(rename_to = self_)]
                        self,
                        move |_rtpbin: gst::Element, session: u32| {
                            let srtpenc = {
                                let mut state = self_.state.lock().unwrap();
                                state
                                    .srtpenc
                                    .entry(session)
                                    .or_insert_with(|| {
                                        gst::ElementFactory::make_with_name(
                                            "srtpenc",
                                            Some(format!("srtpenc_{session}").as_str()),
                                        )
                                        .unwrap_or_else(|_| panic!("srtpenc not found"))
                                    })
                                    .clone()
                            };

                            let s = stream_caps.structure(0).unwrap();

                            let srtp_key = s.get::<gst::Buffer>("srtp-key").unwrap();
                            srtpenc.set_property("key", srtp_key);

                            let srtp_auth = s.get::<String>("srtp-auth").unwrap();
                            srtpenc.set_property_from_str("rtp-auth", srtp_auth.as_str());

                            let srtp_cipher = s.get::<String>("srtp-cipher").unwrap();
                            srtpenc.set_property_from_str("rtp-cipher", srtp_cipher.as_str());

                            let srtcp_auth = s.get::<String>("srtcp-auth").unwrap();
                            srtpenc.set_property_from_str("rtcp-auth", srtcp_auth.as_str());

                            let srtcp_cipher = s.get::<String>("srtcp-cipher").unwrap();
                            srtpenc.set_property_from_str("rtcp-cipher", srtcp_cipher.as_str());

                            let _pad = srtpenc
                                .request_pad_simple(format!("rtcp_sink_{}", session).as_str());

                            srtpenc
                        }
                    ),
                );
            }
        }

        let obj = self.obj();
        manager
            .add_to(obj.upcast_ref::<gst::Bin>())
            .expect("Adding the manager cannot fail");

        let mut tcp_interleave_appsrcs = HashMap::new();
        for (rtpsession_n, p) in task_state.setup_params.iter_mut().enumerate() {
            let (tx, rx) = mpsc::channel(1);
            let on_rtcp = move |appsink: &_| on_rtcp_udp(appsink, tx.clone());
            let is_secure = is_secure_rtp_profile(&p.profile);

            match &mut p.transport {
                RtspTransportInfo::UdpMulticast {
                    dest,
                    port: (rtp_port, rtcp_port),
                    ttl,
                } => {
                    let rtp_socket = bind_port(*rtp_port, dest.is_ipv4())?;
                    let rtcp_socket = rtcp_port.and_then(|p| {
                        bind_port(p, dest.is_ipv4())
                            .map_err(|err| {
                                gst::warning!(CAT, "Could not bind to RTCP port: {err:?}");
                                err
                            })
                            .ok()
                    });

                    match &dest {
                        IpAddr::V4(addr) => {
                            rtp_socket.join_multicast_v4(*addr, Ipv4Addr::UNSPECIFIED)?;
                            if let Some(ttl) = ttl {
                                let _ = rtp_socket.set_multicast_ttl_v4(*ttl as u32);
                            }
                            let _ = rtp_socket.set_multicast_loop_v4(false);
                            if let Some(rtcp_socket) = &rtcp_socket {
                                if let Err(err) =
                                    rtcp_socket.join_multicast_v4(*addr, Ipv4Addr::UNSPECIFIED)
                                {
                                    gst::warning!(
                                        CAT,
                                        "Failed to join RTCP multicast address {addr}: {err:?}"
                                    );
                                } else {
                                    if let Some(ttl) = ttl {
                                        let _ = rtcp_socket.set_multicast_ttl_v4(*ttl as u32);
                                    }
                                    let _ = rtcp_socket.set_multicast_loop_v4(false);
                                }
                            }
                        }
                        IpAddr::V6(addr) => {
                            rtp_socket.join_multicast_v6(addr, 0)?;
                            let _ = rtp_socket.set_multicast_loop_v6(false);
                            if let Some(rtcp_socket) = &rtcp_socket {
                                if let Err(err) = rtcp_socket.join_multicast_v6(addr, 0) {
                                    gst::warning!(
                                        CAT,
                                        "Failed to join RTCP multicast address {addr}: {err:?}"
                                    );
                                } else {
                                    let _ = rtcp_socket.set_multicast_loop_v6(false);
                                }
                            }
                        }
                    };

                    let rtp_appsrc = self.make_rtp_appsrc(rtpsession_n, &p.caps, &manager)?;
                    p.rtp_appsrc = Some(rtp_appsrc.clone());
                    // Spawn RTP udp receive task
                    task_state.handles.push(RUNTIME.spawn(async move {
                        udp_rtp_task(
                            &rtp_socket,
                            rtp_appsrc,
                            settings.timeout,
                            settings.receive_mtu,
                            None,
                        )
                        .await
                    }));

                    // Spawn RTCP udp send/recv task
                    if let Some(rtcp_socket) = rtcp_socket {
                        let rtcp_dest = rtcp_port.and_then(|p| Some(SocketAddr::new(*dest, p)));
                        let rtcp_appsrc =
                            self.make_rtcp_appsrc(rtpsession_n, &manager, is_secure)?;
                        self.make_rtcp_appsink(rtpsession_n, &manager, on_rtcp)?;
                        task_state.handles.push(RUNTIME.spawn(async move {
                            udp_rtcp_task(&rtcp_socket, rtcp_appsrc, rtcp_dest, true, rx).await
                        }));
                    }
                }
                RtspTransportInfo::Udp {
                    source,
                    server_port,
                    client_port: _,
                    sockets,
                } => {
                    let Some((rtp_socket, rtcp_socket)) = sockets.take() else {
                        gst::warning!(
                            CAT,
                            "Skipping: no UDP sockets for {rtpsession_n}: {:#?}",
                            p.transport
                        );
                        continue;
                    };
                    let (rtp_sender_addr, rtcp_sender_addr) = match (source, server_port) {
                        (Some(ip), Some((rtp_port, Some(rtcp_port)))) => {
                            let ip = ip.parse().unwrap();
                            (
                                Some(SocketAddr::new(ip, *rtp_port)),
                                Some(SocketAddr::new(ip, *rtcp_port)),
                            )
                        }
                        (Some(ip), Some((rtp_port, None))) => {
                            (Some(SocketAddr::new(ip.parse().unwrap(), *rtp_port)), None)
                        }
                        _ => (None, None),
                    };

                    // Spawn RTP udp receive task
                    let rtp_appsrc = self.make_rtp_appsrc(rtpsession_n, &p.caps, &manager)?;
                    p.rtp_appsrc = Some(rtp_appsrc.clone());
                    task_state.handles.push(RUNTIME.spawn(async move {
                        udp_rtp_task(
                            &rtp_socket,
                            rtp_appsrc,
                            settings.timeout,
                            settings.receive_mtu,
                            rtp_sender_addr,
                        )
                        .await
                    }));

                    // Spawn RTCP udp send/recv task
                    if let Some(rtcp_socket) = rtcp_socket {
                        let rtcp_appsrc =
                            self.make_rtcp_appsrc(rtpsession_n, &manager, is_secure)?;
                        self.make_rtcp_appsink(rtpsession_n, &manager, on_rtcp)?;
                        task_state.handles.push(RUNTIME.spawn(async move {
                            udp_rtcp_task(&rtcp_socket, rtcp_appsrc, rtcp_sender_addr, false, rx)
                                .await
                        }));
                    }
                }
                RtspTransportInfo::Tcp {
                    channels: (rtp_channel, rtcp_channel),
                } => {
                    let rtp_appsrc = self.make_rtp_appsrc(rtpsession_n, &p.caps, &manager)?;
                    p.rtp_appsrc = Some(rtp_appsrc.clone());
                    tcp_interleave_appsrcs.insert(*rtp_channel, rtp_appsrc);

                    if let Some(rtcp_channel) = rtcp_channel {
                        // RTCP SR
                        let rtcp_appsrc =
                            self.make_rtcp_appsrc(rtpsession_n, &manager, is_secure)?;
                        tcp_interleave_appsrcs.insert(*rtcp_channel, rtcp_appsrc.clone());
                        // RTCP RR
                        let rtcp_channel = *rtcp_channel;
                        let cmd_tx = cmd_tx.clone();
                        self.make_rtcp_appsink(rtpsession_n, &manager, move |appsink| {
                            on_rtcp_tcp(appsink, cmd_tx.clone(), rtcp_channel)
                        })?;
                    }
                }
            }

            if is_secure {
                match manager.using_rtp2 {
                    true => {
                        return Err(RtspError::Fatal(
                            "SRTP not supported with USING_RTP2".to_string(),
                        )
                        .into());
                    }
                    false => {
                        let session = rtpsession_n as u32;
                        let rtp_profile = p.profile.as_str().to_lowercase();

                        if let Some(rtpsession) = manager
                            .recv
                            .emit_by_name::<Option<gst::Element>>("get-session", &[&session])
                        {
                            rtpsession.set_property_from_str("rtp-profile", &rtp_profile);
                        }
                    }
                }
            }
        }

        if !streams_aware {
            obj.no_more_pads();

            let mut state = self.state.lock().unwrap();
            state.emitted_no_more_pads = true;
        }

        // Expose RTP srcpads
        manager.recv.connect_pad_added(glib::clone!(
            #[weak(rename_to = self_)]
            self,
            move |manager, pad| {
                if pad.direction() != gst::PadDirection::Src {
                    return;
                }
                let Some(obj) = manager
                    .parent()
                    .and_then(|o| o.downcast::<gst::Element>().ok())
                else {
                    return;
                };
                let name = pad.name();
                match *name.split('_').collect::<Vec<_>>() {
                    // rtpbin and rtp2
                    ["recv", "rtp", "src", stream_id, ssrc, pt]
                    | ["rtp", "src", stream_id, ssrc, pt] => {
                        if stream_id.parse::<u32>().is_err() {
                            gst::info!(CAT, "Ignoring srcpad with invalid stream id: {name}");
                            return;
                        };
                        gst::info!(CAT, "Setting rtpbin pad {} as ghostpad target", name);
                        let srcpad = obj
                            .static_pad(&format!("stream_{stream_id}"))
                            .expect("ghostpad should've been available already");
                        let ghostpad = srcpad
                            .downcast::<gst::GhostPad>()
                            .expect("rtspsrc src pads are ghost pads");
                        if let Err(err) = ghostpad.set_target(Some(pad)) {
                            gst::element_error!(
                                obj,
                                gst::ResourceError::Failed,
                                (
                                    "Failed to set ghostpad {} target {}: {err:?}",
                                    ghostpad.name(),
                                    name
                                ),
                                ["pt: {pt}, ssrc: {ssrc}"]
                            );
                        }

                        {
                            let mut state = self_.state.lock().unwrap();
                            state.flow_combiner.add_pad(&ghostpad);
                        }
                    }
                    _ => {
                        gst::info!(CAT, "Ignoring unknown srcpad: {name}");
                    }
                }

                let num_src_pads = manager
                    .src_pads()
                    .into_iter()
                    .filter(|p| {
                        if using_rtp2 {
                            p.name().starts_with("rtp_src")
                        } else {
                            p.name().starts_with("recv_rtp_src")
                        }
                    })
                    .collect::<Vec<_>>()
                    .len();
                self_.post_streams_selected(num_src_pads);
            }
        ));

        let mut expected_response: Option<(Method, u32)> = None;
        loop {
            tokio::select! {
                msg = task_state.stream.next() => match msg {
                    Some(Ok(rtsp_types::Message::Data(data))) => {
                        let Some(appsrc) = tcp_interleave_appsrcs.get(&data.channel_id()) else {
                            gst::warning!(CAT,
                                "ignored data of size {}: unknown channel {}",
                                data.len(),
                                data.channel_id()
                            );
                            continue;
                        };
                        let t = appsrc.current_running_time();
                        let channel_id = data.channel_id();
                        gst::trace!(CAT, "Received data on channel {channel_id}");
                        // TODO: this should be from_mut_slice() after making the necessary
                        // modifications to Body
                        let mut buffer = gst::Buffer::from_slice(data.into_body());
                        let bufref = buffer.make_mut();
                        bufref.set_dts(t);
                        if let Err(err) = appsrc.push_buffer(buffer) {
                            gst::error!(CAT, "Failed to push buffer on pad {} for channel {}", appsrc.name(), channel_id);
                            return Err(err.into());
                        }
                    }
                    Some(Ok(rtsp_types::Message::Request(req))) => {
                        // TODO: implement incoming GET_PARAMETER requests
                        gst::debug!(CAT, "<-- {req:#?}");
                    }
                    Some(Ok(rtsp_types::Message::Response(rsp))) => {
                        gst::debug!(CAT, "<-- {rsp:#?}");
                        let Some((expected, cseq)) = &expected_response else {
                            continue;
                        };
                        let Some(s) = &session else {
                            return Err(RtspError::Fatal(format!("Can't handle {expected:?} response, no SETUP")).into());
                        };
                        let (resp_type, res) = match expected {
                            Method::Play => (Method::Play, task_state.play_response(&rsp, *cseq, s).await),
                            Method::Teardown => (Method::Teardown, task_state.teardown_response(&rsp, *cseq, s).await),
                            m => unreachable!("BUG: unexpected response method: {m:?}"),
                        };

                        match res {
                            Ok(_) => {
                                self.post_complete("request", format!("{:?} response received", resp_type).as_str());
                            }
                            Err(RtspError::AuthChallenge(method, challenge)) => {
                                task_state.auth_method = Some(method.clone());

                                if let Some(c) = challenge.as_ref() { task_state.update_digest(parse_digest_params(c)) }

                                // Retry the request with proper authentication
                                let cseq = match resp_type {
                                    Method::Play => task_state.play(s).await?,
                                    Method::Teardown => task_state.teardown(s).await?,
                                    _ => unreachable!("BUG: unexpected response method: {s:?}"),
                                };
                                expected_response = Some((resp_type, cseq));
                            }
                            Err(e) => return Err(e.into()),
                        }
                    }
                    Some(Err(e)) => {
                        // TODO: reconnect or ignore if UDP sockets are still receiving data
                        gst::error!(CAT, "I/O error: {e:?}, quitting");
                        return Err(gst::FlowError::Error.into());
                    }
                    None => {
                        // TODO: reconnect or ignore if UDP sockets are still receiving data
                        gst::error!(CAT, "TCP connection EOF, quitting");
                        return Err(gst::FlowError::Eos.into());
                    }
                },
                Some(cmd) = cmd_rx.recv() => match cmd {
                    Commands::Play => {
                        let Some(s) = &session else {
                            return Err(RtspError::InvalidMessage("Can't PLAY, no SETUP").into());
                        };
                        self.post_start("request", "PLAY request sent");
                        let cseq = task_state.play(s).await.inspect_err(|_err| {
                            self.post_cancelled("request", "PLAY request cancelled");
                        })?;
                        expected_response = Some((Method::Play, cseq));
                    },
                    Commands::Teardown(tx) => {
                        gst::info!(CAT, "Received Teardown command");
                        let Some(s) = &session else {
                            return Err(RtspError::InvalidMessage("Can't TEARDOWN, no SETUP").into());
                        };
                        let _ = task_state.teardown(s).await;
                        if let Some(tx) = tx {
                            let _ = tx.send(());
                        }
                        break;
                    }
                    Commands::Data(data) => {
                        // We currently only send RTCP RR as data messages, this will change when
                        // we support TCP ONVIF backchannels
                        task_state.sink.send(Message::Data(data)).await?;
                        gst::debug!(CAT, "Sent RTCP RR over TCP");
                    }
                    Commands::KeepAlive => {
                        if let Some(s) = &session {
                            task_state.keep_alive(s).await?;
                            expected_response = None;
                        };
                    }
                    Commands::GetParameter(param_req) => {
                        match &session {
                            Some(s) => task_state.send_parameter(s, param_req).await?,
                            None => session_not_found(param_req),
                        }
                        expected_response = None;
                    }
                    Commands::SetParameter(param_req) => {
                        match &session {
                            Some(s) => task_state.send_parameter(s, param_req).await?,
                            None => session_not_found(param_req),
                        }
                        expected_response = None;
                    }
                },
                else => {
                    gst::error!(CAT, "No select statement matched, breaking loop");
                    break;
                }
            }
        }
        Ok(())
    }

    fn handle_select_streams_event(&self, e: &gst::event::SelectStreams) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.streams_aware {
            gst::warning!(CAT, imp = self, "Ignoring stream selection {e:?}");
            return false;
        }

        // TODO: Handle stream selection after SETUP.
        if state.stream_selection_done {
            gst::debug!(
                CAT,
                imp = self,
                "Stream selection not supported after RTSP SETUP"
            );
            return false;
        }

        let seqnum = e.seqnum();

        if state.selection_seqnum == Some(seqnum) {
            gst::debug!(
                CAT,
                imp = self,
                "select-streams with {seqnum:?} already handled"
            );
            return true;
        }
        state.selection_seqnum = Some(seqnum);

        let selected_stream_ids = e
            .streams()
            .into_iter()
            .map(glib::GString::from)
            .collect::<Vec<_>>();

        if selected_stream_ids.iter().any(|id| {
            !state
                .streams
                .iter()
                .any(|s| *id == s.stream.stream_id().unwrap())
        }) {
            gst::warning!(
                CAT,
                imp = self,
                "Got unknown stream in select-streams event"
            );
            return false;
        }

        gst::debug!(
            CAT,
            imp = self,
            "Got select-streams event with streams {selected_stream_ids:?}",
        );

        state.set_selected_streams(Some(selected_stream_ids));

        true
    }

    fn post_initial_collection(&self) {
        let mut state = self.state.lock().unwrap();

        if !state.streams_aware {
            gst::debug!(CAT, imp = self, "Parent is not STREAMS_AWARE");
            state.set_selected_streams(None);
            return;
        }

        let our_seqnum = gst::Seqnum::next();
        state.selection_seqnum = Some(our_seqnum);
        let s = state.all_gst_streams();
        drop(state);

        gst::debug!(CAT, imp = self, "Posting initial StreamCollection");

        let collection = gst::StreamCollection::builder(None).streams(s).build();
        let _ = self.obj().post_message(
            gst::message::StreamCollection::builder(&collection)
                .src(&*self.obj())
                .build(),
        );

        state = self.state.lock().unwrap();
        state.stream_collection = Some(collection);

        if state.selection_seqnum == Some(our_seqnum) {
            gst::debug!(CAT, imp = self, "Using default selection");
            state.set_selected_streams(None);
        } else {
            gst::debug!(
                CAT,
                imp = self,
                "Stream selection handled while posting default StreamCollection"
            );
        }
    }

    fn get_parameter(
        &self,
        parameter: String,
        content_type: Option<String>,
        promise: gst::Promise,
    ) -> bool {
        if parameter.is_empty() {
            gst::error!(CAT, imp = self, "Invalid GET_PARAMETER input");
            return false;
        }

        let content_type = get_content_type(content_type);

        self.get_parameters(vec![parameter], Some(content_type), promise)
    }

    fn get_parameters(
        &self,
        parameters: Vec<String>,
        content_type: Option<String>,
        promise: gst::Promise,
    ) -> bool {
        if parameters.is_empty() {
            gst::error!(CAT, "Invalid GET_PARAMETERS input");
            return false;
        }

        gst::log!(CAT, imp = self, "get_parameters: {}", parameters.len());

        if !self.validate_get_set_parameters(&parameters) {
            return false;
        }

        let content_type = get_content_type(content_type);

        let command = {
            let body = parameters
                .iter()
                .flat_map(|param| format!("{}:\r\n", param).into_bytes())
                .collect::<Vec<u8>>();

            Commands::GetParameter(ParameterRequest {
                method: Method::GetParameter,
                body,
                content_type,
                promise,
            })
        };

        self.send_parameter(command)
    }

    fn set_parameter(
        &self,
        parameter_name: String,
        parameter_value: String,
        content_type: Option<String>,
        promise: gst::Promise,
    ) -> bool {
        if parameter_name.is_empty() || parameter_value.is_empty() {
            gst::error!(CAT, imp = self, "Invalid SET_PARAMETER input");
            return false;
        }

        if !self.validate_get_set_parameters(std::slice::from_ref(&parameter_name)) {
            return false;
        }

        let content_type = get_content_type(content_type);

        let command = {
            let body = {
                let s = format!("{}: {}\r\n", parameter_name, parameter_value);
                s.into_bytes()
            };

            Commands::SetParameter(ParameterRequest {
                method: Method::SetParameter,
                body,
                content_type,
                promise,
            })
        };

        self.send_parameter(command)
    }

    fn validate_get_set_parameters(&self, parameters: &[String]) -> bool {
        parameters.iter().all(|p| {
            let valid = !p
                .chars()
                .any(|c| c.is_ascii_whitespace() || c.is_ascii_control());

            if !valid {
                gst::debug!(CAT, imp = self, "invalid parameter name '{p}'");
            }

            valid
        })
    }

    fn send_parameter(&self, command: Commands) -> bool {
        if self.obj().current_state() != gst::State::Playing {
            return false;
        }

        let cmd_tx = self.cmd_queue();
        let (promise, reply) = match &command {
            Commands::GetParameter(param_request) => {
                (param_request.promise.clone(), GET_PARAMETER_REPLY)
            }
            Commands::SetParameter(param_request) => {
                (param_request.promise.clone(), SET_PARAMETER_REPLY)
            }
            _ => unreachable!(),
        };

        RUNTIME.spawn(async move {
            match cmd_tx.send(command).await {
                Ok(_) => {
                    gst::debug!(CAT, "Send Parameter successful");
                }
                Err(err) => {
                    gst::error!(CAT, "Send Parameter failed {err:?}");
                    reply_with_promise(
                        promise,
                        reply,
                        StatusCode::InternalServerError,
                        &err.to_string(),
                        None,
                    );
                }
            }
        });

        true
    }
}

struct RtspManager {
    recv: gst::Element,
    send: gst::Element,
    using_rtp2: bool,
}

impl RtspManager {
    fn new(rtp2: bool, latency: u32) -> Self {
        let (recv, send) = if rtp2 {
            let recv = gst::ElementFactory::make_with_name("rtprecv", None)
                .unwrap_or_else(|_| panic!("rtprecv not found"));
            recv.set_property("latency", latency);

            let send = gst::ElementFactory::make("rtpsend")
                .property("rtp-id", recv.property::<String>("rtp-id"))
                .build()
                .unwrap_or_else(|_| panic!("rtpsend not found"));
            (recv, send)
        } else {
            let e = gst::ElementFactory::make_with_name("rtpbin", None)
                .unwrap_or_else(|_| panic!("rtpbin not found"));
            e.set_property("latency", latency);

            (e.clone(), e)
        };
        if !rtp2 {
            let on_bye = |args: &[glib::Value]| {
                let m = args[0].get::<gst::Element>().unwrap();
                let obj = m.parent()?;
                let bin = obj.downcast::<gst::Bin>().unwrap();
                bin.send_event(gst::event::Eos::new());
                None
            };
            recv.connect("on-bye-ssrc", true, move |args| {
                gst::info!(CAT, "Received BYE packet");
                on_bye(args)
            });
            recv.connect("on-bye-timeout", true, move |args| {
                gst::info!(CAT, "BYE due to timeout");
                on_bye(args)
            });
        }
        RtspManager {
            recv,
            send,
            using_rtp2: rtp2,
        }
    }

    fn rtp_recv_sinkpad(&self, rtpsession: usize) -> Option<gst::Pad> {
        let name = if self.using_rtp2 {
            format!("rtp_sink_{rtpsession}")
        } else {
            format!("recv_rtp_sink_{rtpsession}")
        };
        gst::info!(CAT, "requesting {name} for receiving RTP");
        self.recv.request_pad_simple(&name)
    }

    fn rtcp_recv_sinkpad(&self, rtpsession: usize) -> Option<gst::Pad> {
        let name = if self.using_rtp2 {
            format!("rtcp_sink_{rtpsession}")
        } else {
            format!("recv_rtcp_sink_{rtpsession}")
        };
        gst::info!(CAT, "requesting {name} for receiving RTCP");
        self.recv.request_pad_simple(&name)
    }

    fn rtcp_send_srcpad(&self, rtpsession: usize) -> Option<gst::Pad> {
        let name = if self.using_rtp2 {
            format!("rtcp_src_{rtpsession}")
        } else {
            format!("send_rtcp_src_{rtpsession}")
        };
        gst::info!(CAT, "requesting {name} for sending RTCP");
        self.send.request_pad_simple(&name)
    }

    fn add_to<T: IsA<gst::Bin>>(&self, bin: &T) -> Result<(), glib::BoolError> {
        if self.using_rtp2 {
            bin.add_many([&self.recv, &self.send])?;
            self.recv.sync_state_with_parent()?;
            self.send.sync_state_with_parent()?;
        } else {
            bin.add_many([&self.recv])?;
            self.recv.sync_state_with_parent()?;
        }
        Ok(())
    }
}

struct RtspTaskState {
    stream_id_prefix: String,
    cseq: u32,
    url: Url,
    version: Version,
    content_base_or_location: Option<String>,
    aggregate_control: Option<Url>,
    sdp: Option<sdp_types::Session>,

    stream:
        Pin<Box<dyn Stream<Item = Result<Message<Body>, super::tcp_message::ReadError>> + Send>>,
    sink: Pin<Box<dyn Sink<Message<Body>, Error = std::io::Error> + Send>>,

    setup_params: Vec<RtspSetupParams>,
    handles: Vec<JoinHandle<()>>,
    username: String,
    password: String,
    auth_method: Option<AuthMethod>,
    digest_params: Option<DigestParams>,
    nonce_count: u32,
    methods: Option<Public>,
    keep_alive_task: Option<JoinHandle<()>>,
}

struct RtspSetupParams {
    control_url: Url,
    transport: RtspTransportInfo,
    rtp_appsrc: Option<gst_app::AppSrc>,
    caps: gst::Caps,
    profile: RtpProfile,
}

impl RtspTaskState {
    fn new(url: Url, stream: RtspStream, sink: RtspSink) -> Self {
        let stream_id_prefix = {
            // Ensure the hash is deterministic over the URL
            let mut hasher = Sha256::new();
            hasher.update(url.as_str().as_bytes());
            let finalize_result = hasher.finalize();
            let hash_128_bytes = &finalize_result[..16];
            hex::encode(hash_128_bytes)
        };
        let username = url.username().to_string();
        let password = url.password().unwrap_or("").to_string();
        let url = rtsp_url(url);

        RtspTaskState {
            stream_id_prefix,
            cseq: 0u32,
            url,
            version: Version::V1_0,
            content_base_or_location: None,
            aggregate_control: None,
            sdp: None,
            stream,
            sink,
            setup_params: Vec::new(),
            handles: Vec::new(),
            username,
            password,
            auth_method: None,
            digest_params: None,
            nonce_count: INITIAL_NONCE,
            methods: None,
            keep_alive_task: None,
        }
    }

    fn request_uri(&self) -> Url {
        let mut uri = self.url.clone();
        let _ = uri.set_username("");
        let _ = uri.set_password(None);
        uri
    }

    #[allow(clippy::result_large_err)]
    fn check_response(
        rsp: &Response<Body>,
        cseq: u32,
        req_name: Method,
        session: Option<&Session>,
    ) -> Result<(), RtspError> {
        let is_param_req = req_name == Method::GetParameter || req_name == Method::SetParameter;

        if rsp.status() == StatusCode::Unauthorized {
            if let Some(auth_header) = rsp.header(&WWW_AUTHENTICATE) {
                if auth_header.as_str().starts_with("Digest") {
                    return Err(RtspError::AuthChallenge(
                        AuthMethod::Digest,
                        Some(auth_header.to_string()),
                    ));
                } else if auth_header.as_str().starts_with("Basic") {
                    return Err(RtspError::AuthChallenge(
                        AuthMethod::Basic,
                        Some(auth_header.to_string()),
                    ));
                }
            }

            return Err(RtspError::Fatal(format!(
                "{req_name:?} request failed authentication"
            )));
        }

        if rsp.status() == StatusCode::NotFound {
            return Err(RtspError::AuthChallenge(AuthMethod::Basic, None));
        }

        // This is different to `rtspsrc` behaviour. `rtspsrc` does not
        // retry on authorization failure for GET/SET_PARAMETER requests.
        // `rtspsrc2` will retry the request with authorization before
        // failing here. Also see `RtspTaskState::send_parameter`.
        if is_param_req && rsp.status() != StatusCode::Ok {
            return Err(RtspError::Parameter(
                rsp.status(),
                rsp.reason_phrase().to_string(),
            ));
        }

        if rsp.status() != StatusCode::Ok {
            return Err(RtspError::Fatal(format!(
                "{req_name:?} request failed: {}",
                rsp.reason_phrase()
            )));
        }

        match rsp.typed_header::<CSeq>() {
            Ok(Some(v)) => {
                if *v != cseq {
                    return Err(RtspError::InvalidMessage("cseq does not match"));
                }
            }
            Ok(None) => {
                gst::warning!(
                    CAT,
                    "No cseq in response, continuing... {:#?}",
                    rsp.headers().collect::<Vec<_>>()
                );
            }
            Err(_) => {
                gst::warning!(
                    CAT,
                    "Invalid cseq in response, continuing... {:#?}",
                    rsp.headers().collect::<Vec<_>>()
                );
            }
        };
        if let Some(s) = session {
            if let Some(have_s) = rsp.typed_header::<Session>()? {
                if s.0 != have_s.0 {
                    return Err(RtspError::Fatal(format!(
                        "Session in header {} does not match our session {}",
                        s.0, have_s.0
                    )));
                }
            } else {
                gst::warning!(
                    CAT,
                    "No Session header in response, continuing... {:#?}",
                    rsp.headers().collect::<Vec<_>>()
                );
            }
        }
        Ok(())
    }

    async fn send_request_with_auth(
        &mut self,
        req: Request<Body>,
    ) -> Result<Response<Body>, RtspError> {
        let mut req = req;
        let mut auth_retries = 0;

        loop {
            if let Some(auth_method) = &self.auth_method {
                req = rebuild_request_with_auth(
                    req,
                    auth_method,
                    self.digest_params.clone(),
                    &self.username,
                    &self.password,
                    self.nonce_count,
                )?;
                self.nonce_count += 1;
            };

            gst::debug!(CAT, "-->> {req:#?}");
            self.sink.send(req.clone().into()).await?;

            let rsp = match self.stream.next().await {
                Some(Ok(rtsp_types::Message::Response(rsp))) => Ok(rsp),
                Some(Ok(m)) => Err(RtspError::UnexpectedMessage("Response", m)),
                Some(Err(e)) => Err(e.into()),
                None => {
                    Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "response").into())
                }
            }?;
            gst::debug!(CAT, "-->> {rsp:#?}");

            match Self::check_response(&rsp, self.cseq, req.method().clone(), None) {
                Ok(_) => return Ok(rsp),
                Err(RtspError::AuthChallenge(auth_method, challenge)) => {
                    if auth_retries >= MAX_AUTH_RETRIES {
                        return Err(RtspError::Fatal(
                            "Too many authentication retries".to_string(),
                        ));
                    }
                    auth_retries += 1;

                    self.auth_method = Some(auth_method.clone());

                    if let Some(c) = challenge.as_ref() {
                        self.update_digest(parse_digest_params(c))
                    }

                    req = rebuild_request_with_auth(
                        req,
                        &auth_method,
                        self.digest_params.clone(),
                        &self.username,
                        &self.password,
                        self.nonce_count,
                    )?;
                    self.nonce_count += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn options(&mut self) -> Result<(), RtspError> {
        self.cseq += 1;
        let req = Request::builder(Method::Options, self.version)
            .typed_header::<CSeq>(&self.cseq.into())
            .request_uri(self.request_uri())
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .build(Body::default());
        let rsp = self.send_request_with_auth(req).await?;

        let Ok(Some(methods)) = rsp.typed_header::<Public>() else {
            return Err(RtspError::InvalidMessage(
                "OPTIONS response does not contain a valid Public header",
            ));
        };

        let needed = [
            Method::Describe,
            Method::Setup,
            Method::Play,
            Method::Teardown,
        ];
        let mut unsupported = Vec::new();
        for method in &needed {
            if !methods.contains(method) {
                unsupported.push(format!("{method:?}"));
            }
        }
        if !unsupported.is_empty() {
            Err(RtspError::Fatal(format!(
                "Server doesn't support the required method{} {}",
                if unsupported.len() == 1 { "" } else { "s:" },
                unsupported.join(",")
            )))
        } else {
            self.methods = Some(methods);
            Ok(())
        }
    }

    async fn describe(&mut self) -> Result<(), RtspError> {
        self.cseq += 1;
        let req = Request::builder(Method::Describe, self.version)
            .typed_header::<CSeq>(&self.cseq.into())
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .header(ACCEPT, "application/sdp")
            .request_uri(self.request_uri())
            .build(Body::default());
        let rsp = self.send_request_with_auth(req).await?;

        gst::debug!(
            CAT,
            "<<-- Response {:#?}",
            rsp.headers().collect::<Vec<_>>()
        );

        self.content_base_or_location = rsp
            .header(&CONTENT_BASE)
            .or(rsp.header(&CONTENT_LOCATION))
            .map(|v| v.to_string());

        gst::info!(CAT, "{}", std::str::from_utf8(rsp.body()).unwrap());
        // TODO: read range attribute from SDP for VOD use-cases
        let sdp = sdp_types::Session::parse(rsp.body())?;
        gst::debug!(CAT, "{sdp:#?}");

        self.sdp.replace(sdp);
        Ok(())
    }

    #[allow(clippy::result_large_err)]
    fn parse_setup_transports(
        transports: &Transports,
        s: &mut gst::Structure,
        protocols: &[RtspProtocol],
        mode: &TransportMode,
    ) -> Result<RtspTransportInfo, RtspError> {
        let mut last_error =
            RtspError::Fatal("No matching transport found matching selected protocols".to_string());
        let mut parsed_transports = Vec::new();
        for transport in transports.iter() {
            let Transport::Rtp(t) = transport else {
                last_error =
                    RtspError::Fatal(format!("Expected RTP transport, got {transports:#?}"));
                continue;
            };
            // RTSP 2 specifies that we can have multiple SSRCs in the response
            // Transport header, but it's not clear why, so we don't support it
            if let Some(ssrc) = t.params.ssrc.first() {
                s.set("ssrc", ssrc)
            }
            if !t.params.mode.is_empty() && !t.params.mode.contains(mode) {
                last_error = RtspError::Fatal(format!(
                    "Requested mode {:?} doesn't match server modes: {:?}",
                    mode, t.params.mode
                ));
                continue;
            }
            let parsed = match RtspTransportInfo::try_from(t) {
                Ok(p) => p,
                Err(err) => {
                    last_error = err;
                    continue;
                }
            };
            parsed_transports.push(parsed);
        }
        for protocol in protocols {
            for n in 0..parsed_transports.len() {
                if parsed_transports[n].to_protocol() == *protocol {
                    let t = parsed_transports.swap_remove(n);
                    return Ok(t);
                }
            }
        }
        Err(last_error)
    }

    fn create_stream(&mut self, s: gst::Structure, media_idx: usize, media: &str) -> gst::Stream {
        let stream_type = match media {
            "audio" => gst::StreamType::AUDIO,
            "video" => gst::StreamType::VIDEO,
            "application" => {
                #[cfg(feature = "v1_28")]
                {
                    let encoding = s.get::<&str>("encoding-name").unwrap_or_default();
                    match encoding.to_ascii_lowercase().as_str() {
                        "vnd.onvif.metadata" | "smpte291" => gst::StreamType::METADATA,
                        _ => gst::StreamType::UNKNOWN,
                    }
                }
                #[cfg(not(feature = "v1_28"))]
                gst::StreamType::UNKNOWN
            }
            _ => gst::StreamType::UNKNOWN,
        };

        let stream_id = format!("{}/{}", self.stream_id_prefix, media_idx);
        let caps = gst::Caps::from(s);

        gst::debug!(
            CAT,
            "Creating stream with id: {stream_id}, media index: {media_idx}, caps: {caps:?}, type: {stream_type}"
        );

        gst::Stream::new(
            Some(&stream_id),
            Some(&caps),
            stream_type,
            gst::StreamFlags::empty(),
        )
    }

    fn create_streams(&mut self) -> Vec<RtspGstStream> {
        let sdp = self.sdp.clone().expect("Must have SDP by now");
        let mut b = gst::Structure::builder("application/x-rtp");

        // TODO: parse range for VOD
        let skip_attrs = ["control", "range", "ssrc"];
        for sdp_types::Attribute { attribute, value } in &sdp.attributes {
            if skip_attrs.contains(&attribute.as_str()) {
                continue;
            }
            b = b.field(format!("a-{attribute}"), value);
        }
        // TODO: parse global extmap

        let structure = b.build();
        let mut streams: Vec<RtspGstStream> = Vec::new();

        for (media_idx, m) in sdp.medias.iter().enumerate() {
            let mut has_mikey = false;
            // RTP caps
            let Ok(pt) = m.fmt.parse::<u8>() else {
                gst::error!(CAT, "Could not parse pt: {}, ignoring media", m.fmt);
                continue;
            };

            let mut s = structure.clone();
            let media = m.media.to_ascii_lowercase();

            s.set("media", &media);
            s.set("payload", pt as i32);

            if let Err(err) =
                sdp::parse_media_attributes(&m.attributes, pt, &media, &mut s, &mut has_mikey)
            {
                gst::warning!(
                    CAT,
                    "Skipping media {} {}, no rtpmap: {err:?}",
                    m.media,
                    m.fmt
                );
                continue;
            }

            streams.push(RtspGstStream {
                stream: self.create_stream(s, media_idx, &media),
                media_idx,
                has_mikey,
            });
        }

        streams
    }

    fn set_aggregate_control(&mut self) {
        let sdp = self.sdp.as_ref().expect("Must have SDP by now");
        let base = self
            .content_base_or_location
            .as_ref()
            .and_then(|s| Url::parse(s).ok())
            .unwrap_or_else(|| self.request_uri());

        self.aggregate_control = sdp
            .get_first_attribute_value("control")
            // No attribute and no value have the same meaning for us
            .ok()
            .flatten()
            .and_then(|v| sdp::parse_control_path(v, &base));
    }

    #[allow(clippy::too_many_arguments)]
    async fn setup(
        &mut self,
        rtsp_src: gst::glib::WeakRef<super::RtspSrc>,
        session: &mut Option<Session>,
        session_timeout: &mut Option<Duration>,
        port_start: u16,
        protocols: &[RtspProtocol],
        mode: TransportMode,
    ) -> Result<Vec<RtspSetupParams>, RtspError> {
        let Some(self_) = rtsp_src.upgrade() else {
            return Err(RtspError::Fatal("Invalid reference".to_string()));
        };

        let streams = {
            let mut state = self_.imp().state.lock().unwrap();
            state.stream_selection_done = true;
            state.selected_streams.as_ref().unwrap().clone()
        };

        let base = self
            .content_base_or_location
            .as_ref()
            .and_then(|s| Url::parse(s).ok())
            .unwrap_or_else(|| self.request_uri());

        let mut port_next = port_start;
        let mut stream_num = 0;
        let mut setup_params: Vec<RtspSetupParams> = Vec::new();
        let sdp = self.sdp.clone().expect("Must have SDP by now");

        for stream in &streams {
            let m: &sdp_types::Media = sdp.medias.get(stream.media_idx).unwrap();
            let caps = stream.stream.caps().unwrap();
            let mut s = caps.structure(0).unwrap().to_owned();

            let media_control = m
                .get_first_attribute_value("control")
                // No attribute and no value have the same meaning for us
                .ok()
                .flatten()
                .and_then(|v| sdp::parse_control_path(v, &base));
            let aggregate_control = self.aggregate_control.clone();

            let Some(control_url) = media_control.as_ref().or(aggregate_control.as_ref()) else {
                gst::warning!(
                    CAT,
                    "No session control or media control for {} fmt {}, ignoring",
                    m.media,
                    m.fmt
                );
                continue;
            };

            // SETUP
            let mut rtp_socket: Option<UdpSocket> = None;
            let mut rtcp_socket: Option<UdpSocket> = None;
            let mut transports = Vec::new();
            let (conn_protocols, is_ipv4) = sdp::parse_connections(&m.connections);
            let rtp_profile = sdp::parse_rtp_profile(m);

            let protocols = if !conn_protocols.is_empty() {
                let p = protocols.iter().cloned().collect::<BTreeSet<_>>();
                p.intersection(&conn_protocols).cloned().collect::<Vec<_>>()
            } else {
                protocols.to_owned()
            };

            if protocols.is_empty() {
                gst::error!(CAT, "No available protocols left, skipping media");
                continue;
            }

            if protocols.contains(&RtspProtocol::UdpMulticast) {
                let params = RtpTransportParameters {
                    mode: vec![mode.clone()],
                    multicast: true,
                    ..Default::default()
                };
                transports.push(Transport::Rtp(RtpTransport {
                    profile: rtp_profile.clone(),
                    lower_transport: Some(RtpLowerTransport::Udp),
                    params,
                }));
            }
            if protocols.contains(&RtspProtocol::Udp) {
                let (sock1, rtp_port) = bind_start_port(port_next, is_ipv4).await;
                // Get the actual port that was successfully bound
                port_next = rtp_port;
                let (sock2, rtcp_port) = bind_start_port(rtp_port + 1, is_ipv4).await;
                rtp_socket = Some(sock1);
                rtcp_socket = Some(sock2);
                let params = RtpTransportParameters {
                    mode: vec![mode.clone()],
                    unicast: true,
                    client_port: Some((rtp_port, Some(rtcp_port))),
                    ..Default::default()
                };
                transports.push(Transport::Rtp(RtpTransport {
                    profile: rtp_profile.clone(),
                    lower_transport: Some(RtpLowerTransport::Udp),
                    params,
                }));
            }
            if protocols.contains(&RtspProtocol::Tcp) {
                let params = RtpTransportParameters {
                    mode: vec![mode.clone()],
                    interleaved: Some((stream_num, Some(stream_num + 1))),
                    ..Default::default()
                };
                transports.push(Transport::Rtp(RtpTransport {
                    profile: rtp_profile.clone(),
                    lower_transport: Some(RtpLowerTransport::Tcp),
                    params,
                }));
            }

            self.cseq += 1;
            let transports: Transports = transports.as_slice().into();
            let req = Request::builder(Method::Setup, self.version)
                .typed_header::<CSeq>(&self.cseq.into())
                .header(USER_AGENT, DEFAULT_USER_AGENT)
                .typed_header::<Transports>(&transports)
                .request_uri(control_url.clone());
            let req = if let Some(s) = session {
                req.typed_header::<Session>(s)
            } else {
                req
            };
            let req = req.build(Body::default());

            // RTSP 2 supports pipelining of SETUP requests, so this
            // ping-pong would have to be reworked if we want to support it.
            let rsp = self.send_request_with_auth(req).await?;

            let new_session = rsp
                .typed_header::<Session>()?
                .ok_or(RtspError::InvalidMessage("No session in SETUP response"))?;

            if let Some(timeout) = new_session.1.or(Some(60)) {
                // See `gst_rtsp_connection_next_timeout_usec` in gstrtspconnection.c.
                let timeout = match timeout {
                    // Because we should act before the timeout we timeout 5
                    // seconds in advance.
                    t if t >= 20 => t - 5,
                    // else timeout 20% earlier
                    t if t >= 5 => t - (t / 5),
                    // else timeout 1 second earlier
                    t if t >= 1 => t - 1,
                    _ => timeout,
                };

                *session_timeout = Some(Duration::from_secs(timeout));
            }

            // Manually strip timeout field: https://github.com/sdroege/rtsp-types/issues/24
            session.replace(Session(new_session.0, None));
            let mut parsed_transport = if let Some(transports) = rsp.typed_header::<Transports>()? {
                Self::parse_setup_transports(&transports, &mut s, &protocols, &mode)
            } else {
                // Transport header in response is optional if only one transport was offered
                // https://datatracker.ietf.org/doc/html/rfc2326#section-12.39
                if transports.len() == 1 {
                    Self::parse_setup_transports(&transports, &mut s, &protocols, &mode)
                } else {
                    Err(RtspError::InvalidMessage(
                        "No transport header in SETUP response",
                    ))
                }
            }?;

            let conn_source = sdp
                .connection
                .as_ref()
                .map(|c| c.connection_address.as_str())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| base.host_str().unwrap());

            match &mut parsed_transport {
                RtspTransportInfo::UdpMulticast { .. } => {}
                RtspTransportInfo::Udp {
                    source,
                    server_port: _,
                    client_port,
                    sockets,
                } => {
                    if source.is_none() {
                        *source = Some(conn_source.to_string());
                    }
                    if let Some((rtp_port, rtcp_port)) = client_port {
                        // There is no reason for the server to reject the client ports WE
                        // selected, so if it does, just ignore it.
                        if *rtp_port != port_next {
                            gst::warning!(
                                CAT,
                                "RTP port changed: {port_next} -> {rtp_port}, ignoring"
                            );
                            *rtp_port = port_next;
                        }
                        port_next += 1;
                        *sockets = if let Some(rtcp_port) = rtcp_port {
                            if *rtcp_port != port_next {
                                gst::warning!(
                                    CAT,
                                    "RTCP port changed: {port_next} -> {rtcp_port}, ignoring"
                                );
                                *rtcp_port = port_next;
                            }
                            port_next += 1;
                            Some((rtp_socket.unwrap(), rtcp_socket))
                        } else {
                            Some((rtp_socket.unwrap(), None))
                        }
                    };
                }
                RtspTransportInfo::Tcp {
                    channels: (rtp_ch, rtcp_ch),
                } => {
                    if *rtp_ch != stream_num {
                        gst::info!(CAT, "RTP channel changed: {stream_num} -> {rtp_ch}");
                    }
                    stream_num += 1;
                    if let Some(rtcp_ch) = rtcp_ch {
                        if *rtcp_ch != stream_num {
                            gst::info!(CAT, "RTCP channel changed: {stream_num} -> {rtcp_ch}");
                        }
                        stream_num += 1;
                    }
                }
            };

            if is_secure_rtp_profile(&rtp_profile) {
                if !stream.has_mikey {
                    return Err(RtspError::Fatal("No MiKey".to_string()));
                }

                s.set_name("application/x-srtp");
            }

            let caps = gst::Caps::from(s);
            stream.stream.set_caps(Some(&caps));

            gst::debug!(
                CAT,
                "Caps for media {}: {caps}, secure: {}",
                m.media,
                stream.has_mikey
            );

            setup_params.push(RtspSetupParams {
                control_url: control_url.clone(),
                transport: parsed_transport,
                rtp_appsrc: None,
                caps,
                profile: rtp_profile,
            });
        }
        Ok(setup_params)
    }

    async fn play(&mut self, session: &Session) -> Result<u32, RtspError> {
        self.cseq += 1;
        let request_uri = self
            .aggregate_control
            .as_ref()
            .unwrap_or(&self.request_uri())
            .clone();
        let req = Request::builder(Method::Play, self.version)
            .typed_header::<CSeq>(&self.cseq.into())
            .typed_header::<Range>(&Range::Npt(NptRange::From(NptTime::Now)))
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .request_uri(request_uri)
            .typed_header::<Session>(session);

        let req = req.build(Body::default());
        self.send_request_with_auth(req).await?;

        Ok(self.cseq)
    }

    async fn play_response(
        &mut self,
        rsp: &Response<Body>,
        cseq: u32,
        session: &Session,
    ) -> Result<(), RtspError> {
        Self::check_response(rsp, cseq, Method::Play, Some(session))?;

        if let Some(RtpInfos::V1(rtpinfos)) = rsp.typed_header::<RtpInfos>()? {
            for rtpinfo in rtpinfos {
                for params in self.setup_params.iter_mut() {
                    if params.control_url == rtpinfo.uri {
                        let mut changed = false;
                        let mut caps = params.rtp_appsrc.as_ref().unwrap().caps().unwrap();
                        let capsref = caps.make_mut();
                        if let Some(v) = rtpinfo.seq {
                            capsref.set("seqnum-base", v as u32);
                            changed = true;
                        }
                        if let Some(v) = rtpinfo.rtptime {
                            capsref.set("clock-base", v);
                            changed = true;
                        }
                        if changed {
                            params.rtp_appsrc.as_ref().unwrap().set_caps(Some(&caps));
                        }
                    }
                }
            }
        } else {
            gst::warning!(CAT, "No RTPInfos V1 header in PLAY response");
        };
        Ok(())
    }

    async fn teardown(&mut self, session: &Session) -> Result<u32, RtspError> {
        self.cseq += 1;
        let request_uri = self
            .aggregate_control
            .as_ref()
            .unwrap_or(&self.request_uri())
            .clone();
        let req = Request::builder(Method::Teardown, self.version)
            .typed_header::<CSeq>(&self.cseq.into())
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .request_uri(request_uri)
            .typed_header::<Session>(session);

        let req = req.build(Body::default());
        self.send_request_with_auth(req).await?;

        Ok(self.cseq)
    }

    async fn keep_alive(&mut self, session: &Session) -> Result<(), RtspError> {
        self.cseq += 1;
        let request_uri = self
            .aggregate_control
            .as_ref()
            .unwrap_or(&self.request_uri())
            .clone();
        let method = self
            .methods
            .as_ref()
            .and_then(|m| {
                if m.contains(&Method::SetParameter) {
                    Some(Method::SetParameter)
                } else if m.contains(&Method::GetParameter) {
                    Some(Method::GetParameter)
                } else {
                    None
                }
            })
            .unwrap_or(Method::Options);
        let mut req = Request::builder(method, self.version)
            .typed_header::<CSeq>(&self.cseq.into())
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .request_uri(request_uri)
            .typed_header::<Session>(session)
            .build(Body::default());

        if let Some(auth_method) = &self.auth_method {
            req = rebuild_request_with_auth(
                req,
                auth_method,
                self.digest_params.clone(),
                &self.username,
                &self.password,
                self.nonce_count,
            )?;
            self.nonce_count += 1;
        };

        gst::debug!(CAT, "-->> {req:#?}");
        self.sink.send(req.into()).await?;

        Ok(())
    }

    async fn teardown_response(
        &mut self,
        rsp: &Response<Body>,
        cseq: u32,
        session: &Session,
    ) -> Result<(), RtspError> {
        Self::check_response(rsp, cseq, Method::Teardown, Some(session))?;
        Ok(())
    }

    fn update_digest(&mut self, new_params: Option<DigestParams>) {
        if reset_nonce(
            new_params.as_ref(),
            self.digest_params.as_ref(),
            self.nonce_count,
        ) {
            self.nonce_count = 1;
        }

        self.digest_params = new_params;
    }

    fn start_keep_alive_task(&mut self, cmd_tx: mpsc::Sender<Commands>, timeout: Duration) {
        let task = RUNTIME.spawn(async move {
            let mut interval = tokio::time::interval(timeout);
            interval.tick().await;

            loop {
                interval.tick().await;

                match cmd_tx.send(Commands::KeepAlive).await {
                    Ok(_) => {
                        gst::debug!(CAT, "Keep-alive successful");
                    }
                    Err(err) => {
                        gst::error!(CAT, "Keep-alive failed {err:?}");
                        break;
                    }
                }
            }
        });

        self.keep_alive_task = Some(task);
    }

    async fn send_parameter(
        &mut self,
        session: &Session,
        param_req: ParameterRequest,
    ) -> Result<(), RtspError> {
        let method = param_req.method.clone();
        let reply = parameter_method_to_reply(&method);

        if let Some(m) = self.methods.as_ref()
            && !m.contains(&param_req.method)
        {
            reply_with_promise(
                param_req.promise,
                reply,
                StatusCode::MethodNotAllowed,
                &StatusCode::MethodNotAllowed.to_string(),
                None,
            );
            return Ok(());
        };

        self.cseq += 1;
        let request_uri = self
            .aggregate_control
            .as_ref()
            .unwrap_or(&self.request_uri())
            .clone();

        let req = Request::builder(param_req.method, self.version)
            .typed_header::<CSeq>(&self.cseq.into())
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .header(CONTENT_TYPE, param_req.content_type.clone())
            .request_uri(request_uri)
            .typed_header::<Session>(session)
            .build(param_req.body.into());

        match self.send_request_with_auth(req).await {
            Ok(rsp) => {
                reply_with_promise(
                    param_req.promise,
                    reply,
                    rsp.status(),
                    rsp.reason_phrase(),
                    String::from_utf8(rsp.body().to_vec()).ok(),
                );
            }
            Err(err) => match err {
                RtspError::Parameter(status_code, reason) => {
                    reply_with_promise(param_req.promise, reply, status_code, &reason, None);
                }
                e => {
                    reply_with_promise(
                        param_req.promise,
                        reply,
                        StatusCode::InternalServerError,
                        &e.to_string(),
                        None,
                    );
                }
            },
        }

        Ok(())
    }
}

fn bind_port(port: u16, is_ipv4: bool) -> Result<UdpSocket, std::io::Error> {
    let domain = if is_ipv4 {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let sock = Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    let _ = sock.set_reuse_address(true);
    #[cfg(unix)]
    let _ = sock.set_reuse_port(true);
    sock.set_nonblocking(true)?;
    let addr: SocketAddr = if is_ipv4 {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
    } else {
        SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0))
    };
    sock.bind(&addr.into())?;
    let bound_port = if is_ipv4 {
        sock.local_addr()?.as_socket_ipv4().unwrap().port()
    } else {
        sock.local_addr()?.as_socket_ipv6().unwrap().port()
    };
    gst::debug!(CAT, "Bound to UDP port {bound_port}");

    UdpSocket::from_std(sock.into())
}

async fn bind_start_port(port: u16, is_ipv4: bool) -> (UdpSocket, u16) {
    let mut next_port = port;
    loop {
        match bind_port(next_port, is_ipv4) {
            Ok(socket) => {
                if next_port != 0 {
                    return (socket, next_port);
                }
                let addr = socket
                    .local_addr()
                    .expect("Newly-bound port should not fail");
                return (socket, addr.port());
            }
            Err(err) => {
                gst::debug!(CAT, "Failed to bind to {next_port}: {err:?}, trying next");
                next_port += 1;
                // If we fail too much, panic instead of forever doing a hot-loop
                if (next_port - MAX_BIND_PORT_RETRY) > port {
                    panic!("Failed to allocate any ports from {port} to {next_port}");
                }
            }
        };
    }
}

fn on_rtcp_udp(
    appsink: &gst_app::AppSink,
    tx: mpsc::Sender<MappedBuffer<Readable>>,
) -> Result<gst::FlowSuccess, gst::FlowError> {
    let Ok(sample) = appsink.pull_sample() else {
        return Err(gst::FlowError::Error);
    };
    let Some(buffer) = sample.buffer_owned() else {
        return Ok(gst::FlowSuccess::Ok);
    };
    let map = buffer.into_mapped_buffer_readable();
    match map {
        Ok(map) => match tx.try_send(map) {
            Ok(_) => Ok(gst::FlowSuccess::Ok),
            Err(mpsc::error::TrySendError::Full(_)) => {
                gst::error!(CAT, "Could not send RTCP, channel is full");
                Err(gst::FlowError::Error)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(gst::FlowError::Eos),
        },
        Err(err) => {
            gst::error!(CAT, "Failed to map buffer: {err:?}");
            Err(gst::FlowError::Error)
        }
    }
}

fn on_rtcp_tcp(
    appsink: &gst_app::AppSink,
    cmd_tx: mpsc::Sender<Commands>,
    rtcp_channel: u8,
) -> Result<gst::FlowSuccess, gst::FlowError> {
    let Ok(sample) = appsink.pull_sample() else {
        return Err(gst::FlowError::Error);
    };
    let Some(buffer) = sample.buffer_owned() else {
        return Ok(gst::FlowSuccess::Ok);
    };
    let map = buffer.into_mapped_buffer_readable();
    match map {
        Ok(map) => {
            let data: rtsp_types::Data<Body> =
                rtsp_types::Data::new(rtcp_channel, Body::mapped(map));
            let cmd_tx = cmd_tx.clone();
            RUNTIME.spawn(async move { cmd_tx.send(Commands::Data(data)).await });
            Ok(gst::FlowSuccess::Ok)
        }
        Err(err) => {
            gst::error!(CAT, "Failed to map buffer: {err:?}");
            Err(gst::FlowError::Error)
        }
    }
}

async fn udp_rtp_task(
    socket: &UdpSocket,
    appsrc: gst_app::AppSrc,
    timeout: gst::ClockTime,
    receive_mtu: u32,
    sender_addr: Option<SocketAddr>,
) {
    let t = Duration::from_secs(timeout.into());
    let sender_addr = match sender_addr {
        Some(addr) => addr,
        // Server didn't give us a Transport header or its Transport header didn't specify the
        // server port, so we don't know the sender port from which we will get data till we get
        // the first packet here.
        None => {
            let ret = match time::timeout(t, socket.peek_sender()).await {
                Ok(Ok(addr)) => Ok(addr),
                Ok(Err(_elapsed)) => Err(format!(
                    "No data after {} seconds, exiting",
                    timeout.seconds()
                )),
                Err(err) => Err(format!("UDP socket was closed: {err:?}")),
            };
            match ret {
                Ok(addr) => addr,
                Err(err) => {
                    gst::element_error!(
                        appsrc,
                        gst::ResourceError::Failed,
                        ("{}", err),
                        ["{:#?}", socket]
                    );
                    return;
                }
            }
        }
    };
    gst::info!(CAT, "Receiving from address {sender_addr:?}");
    let gio_addr = {
        let inet_addr: gio::InetAddress = sender_addr.ip().into();
        gio::InetSocketAddress::new(&inet_addr, sender_addr.port())
    };
    let mut size = receive_mtu;
    let caps = appsrc.caps();
    let mut pool = gst::BufferPool::new();
    let mut config = pool.config();
    config.set_params(caps.as_ref(), size, 2, 0);
    pool.set_config(config).unwrap();
    pool.set_active(true).unwrap();
    let error = loop {
        let Ok(buffer) = pool.acquire_buffer(None) else {
            break "Failed to acquire buffer".to_string();
        };
        let Ok(mut map) = buffer.into_mapped_buffer_writable() else {
            break "Failed to map buffer writable".to_string();
        };
        match time::timeout(t, socket.recv_from(map.as_mut_slice())).await {
            Ok(Ok((len, addr))) => {
                // Ignore packets from the wrong sender
                if addr != sender_addr {
                    continue;
                }
                if size < UDP_PACKET_MAX_SIZE && len == size as usize {
                    gst::warning!(
                        CAT,
                        "Data maybe lost: UDP buffer size {size} filled, doubling"
                    );
                    size = (size * 2).min(UDP_PACKET_MAX_SIZE);
                    if let Err(err) = pool.set_active(false) {
                        break format!("Failed to deactivate buffer pool: {err:?}");
                    }
                    pool = gst::BufferPool::new();
                    let mut config = pool.config();
                    config.set_params(caps.as_ref(), size, 2, 0);
                    pool.set_config(config).unwrap();
                    if let Err(err) = pool.set_active(true) {
                        break format!("Failed to reallocate buffer pool: {err:?}");
                    }
                }
                let t = appsrc.current_running_time();
                let mut buffer = map.into_buffer();
                let bufref = buffer.make_mut();
                bufref.set_size(len);
                bufref.set_dts(t);
                gst_net::NetAddressMeta::add(bufref, &gio_addr);
                gst::trace!(CAT, "received RTP packet from {addr:?}");
                if let Err(err) = appsrc.push_buffer(buffer) {
                    break format!("UDP buffer push failed: {err:?}");
                }
            }
            Ok(Err(_elapsed)) => {
                break format!("No data after {} seconds, exiting", timeout.seconds());
            }
            Err(err) => break format!("UDP socket was closed: {err:?}"),
        };
    };
    gst::element_error!(
        appsrc,
        gst::ResourceError::Failed,
        ("{}", error),
        ["{:#?}", socket]
    );
}

async fn udp_rtcp_task(
    socket: &UdpSocket,
    appsrc: gst_app::AppSrc,
    mut sender_addr: Option<SocketAddr>,
    is_multicast: bool,
    mut rx: mpsc::Receiver<MappedBuffer<Readable>>,
) {
    let mut buf = vec![0; UDP_PACKET_MAX_SIZE as usize];
    let mut cache: LruCache<_, _> = LruCache::new(NonZeroUsize::new(RTCP_ADDR_CACHE_SIZE).unwrap());
    let error = loop {
        tokio::select! {
            send_rtcp = rx.recv() => match send_rtcp {
                // The server either didn't specify a server_port for RTCP, or if the server didn't
                // send a Transport header in the SETUP response at all.
                Some(data) => if let Some(addr) = sender_addr.as_ref() {
                    match socket.send_to(data.as_ref(), addr).await {
                        Ok(_) => gst::debug!(CAT, "Sent RTCP RR packet"),
                        Err(err) => {
                            rx.close();
                            break format!("RTCP send error: {err:?}, stopping task");
                        }
                    }
                } else {
                    gst::warning!(CAT, "Can't send RTCP yet: don't have dest addr");
                },
                None => {
                    rx.close();
                    break format!("UDP socket {socket:?} closed, no more RTCP will be sent");
                }
            },
            recv_rtcp = socket.recv_from(&mut buf) => match recv_rtcp {
                Ok((len, addr)) => {
                    gst::debug!(CAT, "Received RTCP packet");
                    if let Some(sender_addr) = sender_addr {
                        // Ignore RTCP from the wrong sender
                        if !is_multicast && addr != sender_addr {
                            continue;
                        }
                    } else {
                        sender_addr.replace(addr);
                        gst::info!(CAT, "Delayed RTCP UDP send address: {addr:?}");
                    };
                    let t = appsrc.current_running_time();
                    let mut buffer = gst::Buffer::from_slice(buf[..len].to_owned());
                    let bufref = buffer.make_mut();
                    bufref.set_dts(t);
                    let gio_addr = cache.get_or_insert(addr, || {
                        let inet_addr: gio::InetAddress = addr.ip().into();
                        gio::InetSocketAddress::new(&inet_addr, addr.port())
                    });
                    gst_net::NetAddressMeta::add(bufref, gio_addr);
                    if let Err(err) = appsrc.push_buffer(buffer) {
                        break format!("UDP buffer push failed: {err:?}");
                    }
                }
                Err(err) => break format!("UDP socket was closed: {err:?}"),
            },
        }
    };
    gst::element_error!(
        appsrc,
        gst::ResourceError::Failed,
        ("{}", error),
        ["{:#?}", socket]
    );
}

#[glib::object_subclass]
impl ObjectSubclass for RtspSrc {
    const NAME: &'static str = "GstRtspSrc2";
    type Type = super::RtspSrc;
    type ParentType = gst::Bin;
    type Interfaces = (gst::URIHandler,);
}
