// Copyright (C) 2016-2018 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
use bytes::Bytes;
use futures::future::Either;
use futures::sync::oneshot;
use futures::{Future, Stream};
use reqwest::r#async::{Client, Decoder};
use reqwest::StatusCode;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::u64;
use tokio::prelude::*;
use tokio::runtime;
use url::Url;

use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

const DEFAULT_LOCATION: Option<Url> = None;
const DEFAULT_USER_AGENT: &str = concat!(
    "GStreamer reqwesthttpsrc ",
    env!("CARGO_PKG_VERSION"),
    "-",
    env!("COMMIT_ID")
);
const DEFAULT_IS_LIVE: bool = false;
const DEFAULT_TIMEOUT: u32 = 15;
const DEFAULT_COMPRESS: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    location: Option<Url>,
    user_agent: String,
    user_id: Option<String>,
    user_pw: Option<String>,
    timeout: u32,
    compress: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            location: DEFAULT_LOCATION,
            user_agent: DEFAULT_USER_AGENT.into(),
            user_id: None,
            user_pw: None,
            timeout: DEFAULT_TIMEOUT,
            compress: DEFAULT_COMPRESS,
        }
    }
}

static PROPERTIES: [subclass::Property; 7] = [
    subclass::Property("location", |name| {
        glib::ParamSpec::string(
            name,
            "Location",
            "URL to read from",
            None,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("user-agent", |name| {
        glib::ParamSpec::string(
            name,
            "User-Agent",
            "Value of the User-Agent HTTP request header field",
            DEFAULT_USER_AGENT.into(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("is-live", |name| {
        glib::ParamSpec::boolean(
            name,
            "Is Live",
            "Act like a live source",
            DEFAULT_IS_LIVE,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("user-id", |name| {
        glib::ParamSpec::string(
            name,
            "User-id",
            "HTTP location URI user id for authentication",
            None,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("user-pw", |name| {
        glib::ParamSpec::string(
            name,
            "User-pw",
            "HTTP location URI user password for authentication",
            None,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("timeout", |name| {
        glib::ParamSpec::uint(
            name,
            "Timeout",
            "Value in seconds to timeout a blocking I/O (0 = No timeout).",
            0,
            3600,
            DEFAULT_TIMEOUT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("compress", |name| {
        glib::ParamSpec::boolean(
            name,
            "Compress",
            "Allow compressed content encodings",
            DEFAULT_COMPRESS,
            glib::ParamFlags::READWRITE,
        )
    }),
];

const REQWEST_CLIENT_CONTEXT: &str = "gst.request.client";

#[derive(Clone, Debug)]
struct ClientContext(Arc<ClientContextInner>);

#[derive(Debug)]
struct ClientContextInner {
    client: Client,
}

impl glib::subclass::boxed::BoxedType for ClientContext {
    const NAME: &'static str = "ReqwestClientContext";

    glib_boxed_type!();
}

glib_boxed_derive_traits!(ClientContext);

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum State {
    Stopped,
    Started {
        uri: Url,
        body: Option<Decoder>,
        seekable: bool,
        position: u64,
        size: Option<u64>,
        start: u64,
        stop: Option<u64>,
    },
}

impl Default for State {
    fn default() -> Self {
        State::Stopped
    }
}

#[derive(Debug)]
pub struct ReqwestHttpSrc {
    cat: gst::DebugCategory,
    client: Mutex<Option<ClientContext>>,
    external_client: Mutex<Option<ClientContext>>,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    runtime: runtime::Runtime,
    canceller: Mutex<Option<oneshot::Sender<Bytes>>>,
}

impl ReqwestHttpSrc {
    fn set_location(
        &self,
        _element: &gst_base::BaseSrc,
        uri: Option<&str>,
    ) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Changing the `location` property on a started `reqwesthttpsrc` is not supported",
            ));
        }

        let mut settings = self.settings.lock().unwrap();

        if uri.is_none() {
            settings.location = DEFAULT_LOCATION;
            return Ok(());
        }

        let uri = uri.unwrap();
        let uri = Url::parse(uri).map_err(|err| {
            glib::Error::new(
                gst::URIError::BadUri,
                format!("Failed to parse URI '{}': {:?}", uri, err,).as_str(),
            )
        })?;

        if uri.scheme() != "http" && uri.scheme() != "https" {
            return Err(glib::Error::new(
                gst::URIError::UnsupportedProtocol,
                format!("Unsupported URI scheme '{}'", uri.scheme(),).as_str(),
            ));
        }

        settings.location = Some(uri);

        Ok(())
    }

    fn do_request(
        &self,
        src: &gst_base::BaseSrc,
        uri: Url,
        start: u64,
        stop: Option<u64>,
    ) -> Result<State, gst::ErrorMessage> {
        use hyperx::header::{
            qitem, AcceptEncoding, AcceptRanges, ByteRangeSpec, Connection, ContentLength,
            ContentRange, ContentRangeSpec, Encoding, Headers, Range, RangeUnit, UserAgent,
        };

        let cat = self.cat;
        let req = {
            let mut client_guard = self.client.lock().unwrap();
            if client_guard.is_none() {
                let srcpad = src.get_static_pad("src").unwrap();
                let mut q = gst::Query::new_context(REQWEST_CLIENT_CONTEXT);
                if srcpad.peer_query(&mut q) {
                    if let Some(context) = q.get_context_owned() {
                        src.set_context(&context);
                    }
                } else {
                    let _ = src.post_message(
                        &gst::Message::new_need_context(REQWEST_CLIENT_CONTEXT)
                            .src(Some(src))
                            .build(),
                    );
                }

                if let Some(client) = {
                    // FIXME: Is there a simpler way to ensure the lock is not hold
                    // after this block anymore?
                    let external_client = self.external_client.lock().unwrap();
                    let client = external_client.as_ref().map(|c| c.clone());
                    drop(external_client);
                    client
                } {
                    gst_debug!(cat, obj: src, "Using shared client");
                    *client_guard = Some(client);
                } else {
                    gst_debug!(cat, obj: src, "Creating new client");
                    let client = ClientContext(Arc::new(ClientContextInner {
                        client: Client::builder()
                            .cookie_store(true)
                            .gzip(true)
                            .build()
                            .map_err(|err| {
                                gst_error_msg!(
                                    gst::ResourceError::OpenRead,
                                    ["Failed to create Client for {}: {}", uri, err]
                                )
                            })?,
                    }));

                    gst_debug!(cat, obj: src, "Sharing new client with other elements");
                    let mut context = gst::Context::new(REQWEST_CLIENT_CONTEXT, true);
                    {
                        let context = context.get_mut().unwrap();
                        let s = context.get_mut_structure();
                        s.set("client", &client);
                    }
                    src.set_context(&context);
                    let _ = src.post_message(
                        &gst::Message::new_have_context(context)
                            .src(Some(src))
                            .build(),
                    );

                    *client_guard = Some(client);
                }
            } else {
                gst_debug!(cat, obj: src, "Using already configured client");
            }

            client_guard.as_ref().unwrap().0.client.get(uri.clone())
        };
        let settings = self.settings.lock().unwrap().clone();

        let mut headers = Headers::new();

        headers.set(Connection::keep_alive());

        match (start != 0, stop) {
            (false, None) => (),
            (true, None) => {
                headers.set(Range::Bytes(vec![ByteRangeSpec::AllFrom(start)]));
            }
            (_, Some(stop)) => {
                headers.set(Range::Bytes(vec![ByteRangeSpec::FromTo(start, stop - 1)]));
            }
        }

        headers.set(UserAgent::new(settings.user_agent.to_owned()));

        if !settings.compress {
            // Compression is the default
            headers.set(AcceptEncoding(vec![qitem(Encoding::Identity)]));
        };

        // Add all headers for the request here
        let req = req.headers(headers.into());

        let req = if let Some(user_id) = settings.user_id {
            // HTTP auth available
            req.basic_auth(user_id, settings.user_pw)
        } else {
            req
        };

        gst_debug!(cat, obj: src, "Doing new request {:?}", req);

        let src_clone = src.clone();
        let response_fut = req.send().and_then(move |res| {
            gst_debug!(cat, obj: &src_clone, "Response received: {:?}", res);
            Ok(res)
        });

        let uri_clone = uri.clone();
        let mut response = self
            .wait(response_fut.map_err(move |err| {
                gst_error_msg!(
                    gst::ResourceError::Read,
                    ["Failed to fetch {}: {:?}", uri_clone, err]
                )
            }))
            .map_err(|err| {
                err.unwrap_or_else(|| {
                    gst_error_msg!(gst::LibraryError::Failed, ["Interrupted during start"])
                })
            })?;

        if !response.status().is_success() {
            match response.status() {
                StatusCode::NOT_FOUND => {
                    gst_error!(cat, obj: src, "Request status failed: {:?}", response);
                    return Err(gst_error_msg!(
                        gst::ResourceError::NotFound,
                        ["Request status failed for {}: {}", uri, response.status()]
                    ));
                }
                StatusCode::UNAUTHORIZED
                | StatusCode::PAYMENT_REQUIRED
                | StatusCode::FORBIDDEN
                | StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
                    gst_error!(cat, obj: src, "Request status failed: {:?}", response);
                    return Err(gst_error_msg!(
                        gst::ResourceError::NotAuthorized,
                        ["Request status failed for {}: {}", uri, response.status()]
                    ));
                }
                _ => {
                    gst_error!(cat, obj: src, "Request status failed: {:?}", response);
                    return Err(gst_error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Request status failed for {}: {}", uri, response.status()]
                    ));
                }
            }
        }

        let headers = Headers::from(response.headers());
        let size = headers.get().map(|&ContentLength(cl)| cl + start);

        let accept_byte_ranges = if let Some(&AcceptRanges(ref ranges)) = headers.get() {
            ranges.iter().any(|u| *u == RangeUnit::Bytes)
        } else {
            false
        };

        let seekable = size.is_some() && accept_byte_ranges;

        let position = if let Some(&ContentRange(ContentRangeSpec::Bytes {
            range: Some((range_start, _)),
            ..
        })) = headers.get()
        {
            range_start
        } else {
            0
        };

        if position != start {
            return Err(gst_error_msg!(
                gst::ResourceError::Seek,
                ["Failed to seek to {}: Got {}", start, position]
            ));
        }

        gst_debug!(cat, obj: src, "Request successful: {:?}", response);

        let body = mem::replace(response.body_mut(), Decoder::empty());

        Ok(State::Started {
            uri,
            body: Some(body),
            seekable,
            position,
            size,
            start,
            stop,
        })
    }

    fn wait<F>(&self, future: F) -> Result<F::Item, Option<gst::ErrorMessage>>
    where
        F: Send + Future<Error = gst::ErrorMessage> + 'static,
        F::Item: Send,
    {
        let timeout = self.settings.lock().unwrap().timeout;
        let mut canceller = self.canceller.lock().unwrap();
        let (sender, receiver) = oneshot::channel::<Bytes>();

        canceller.replace(sender);

        // wrapping timeout around future
        let future_timeout = if timeout == 0 {
            Either::A(future)
        } else {
            Either::B(
                future
                    .timeout(Duration::from_secs(timeout.into()))
                    .map_err(|_err| gst_error_msg!(gst::ResourceError::Read, ["Request timeout"])),
            )
        };

        let unlock_error = gst_error_msg!(gst::ResourceError::Busy, ["unlock"]);

        let res = oneshot::spawn(future_timeout, &self.runtime.executor())
            .select(receiver.then(|_| Err(unlock_error.clone())))
            .wait()
            .map(|v| v.0)
            .map_err(|err| {
                if err.0 == unlock_error {
                    None
                } else {
                    Some(err.0)
                }
            });

        /* Clear out the canceller */
        *canceller = None;

        res
    }
}

impl ObjectImpl for ReqwestHttpSrc {
    glib_object_impl!();

    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let element = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();

                let location = value.get::<&str>().expect("type checked upstream");
                if let Err(err) = self.set_location(element, location) {
                    gst_error!(
                        self.cat,
                        obj: element,
                        "Failed to set property `location`: {:?}",
                        err
                    );
                }
            }
            subclass::Property("user-agent", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let user_agent = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_USER_AGENT.into());
                settings.user_agent = user_agent;
            }
            subclass::Property("is-live", ..) => {
                let element = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();
                let is_live = value.get_some().expect("type checked upstream");
                element.set_live(is_live);
            }
            subclass::Property("user-id", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let user_id = value.get().expect("type checked upstream");
                settings.user_id = user_id;
            }
            subclass::Property("user-pw", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let user_pw = value.get().expect("type checked upstream");
                settings.user_pw = user_pw;
            }
            subclass::Property("timeout", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let timeout = value.get_some().expect("type checked upstream");
                settings.timeout = timeout;
            }
            subclass::Property("compress", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let compress = value.get_some().expect("type checked upstream");
                settings.compress = compress;
            }
            _ => unimplemented!(),
        };
    }

    fn get_property(&self, obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let settings = self.settings.lock().unwrap();
                let location = settings.location.as_ref().map(Url::to_string);

                Ok(location.to_value())
            }
            subclass::Property("user-agent", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.user_agent.to_value())
            }
            subclass::Property("is-live", ..) => {
                let element = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();
                Ok(element.is_live().to_value())
            }
            subclass::Property("user-id", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.user_id.to_value())
            }
            subclass::Property("user-pw", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.user_pw.to_value())
            }
            subclass::Property("timeout", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.timeout.to_value())
            }
            subclass::Property("compress", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.compress.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);
        let element = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();
        element.set_automatic_eos(false);
        element.set_format(gst::Format::Bytes);
    }
}

impl ElementImpl for ReqwestHttpSrc {
    fn set_context(&self, element: &gst::Element, context: &gst::Context) {
        if context.get_context_type() == REQWEST_CLIENT_CONTEXT {
            let mut external_client = self.external_client.lock().unwrap();
            let s = context.get_structure();
            *external_client = s
                .get_some::<&ClientContext>("client")
                .map(|c| Some(c.clone()))
                .unwrap_or(None);
        }

        self.parent_set_context(element, context);
    }

    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::ReadyToNull => {
                *self.client.lock().unwrap() = None;
            }
            _ => (),
        }

        self.parent_change_state(element, transition)
    }
}

impl BaseSrcImpl for ReqwestHttpSrc {
    fn is_seekable(&self, _src: &gst_base::BaseSrc) -> bool {
        match *self.state.lock().unwrap() {
            State::Started { seekable, .. } => seekable,
            _ => false,
        }
    }

    fn get_size(&self, _src: &gst_base::BaseSrc) -> Option<u64> {
        match *self.state.lock().unwrap() {
            State::Started { size, .. } => size,
            _ => None,
        }
    }

    fn start(&self, src: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        *state = State::Stopped;

        let uri = self
            .settings
            .lock()
            .unwrap()
            .location
            .as_ref()
            .ok_or_else(|| {
                gst_error_msg!(gst::CoreError::StateChange, ["Can't start without an URI"])
            })
            .map(|uri| uri.clone())?;

        *state = self.do_request(src, uri, 0, None)?;

        Ok(())
    }

    fn stop(&self, _src: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = State::Stopped;

        Ok(())
    }

    fn do_seek(&self, src: &gst_base::BaseSrc, segment: &mut gst::Segment) -> bool {
        let segment = segment.downcast_mut::<gst::format::Bytes>().unwrap();

        let mut state = self.state.lock().unwrap();

        let (position, old_stop, uri) = match *state {
            State::Started {
                position,
                stop,
                ref uri,
                ..
            } => (position, stop, uri.clone()),
            State::Stopped => {
                gst_element_error!(src, gst::LibraryError::Failed, ["Not started yet"]);

                return false;
            }
        };

        let start = segment.get_start().expect("No start position given");
        let stop = segment.get_stop();

        if position == start && old_stop == stop.0 {
            return true;
        }

        *state = State::Stopped;
        match self.do_request(src, uri, start, stop.0) {
            Ok(s) => {
                *state = s;
                true
            }
            Err(err) => {
                src.post_error_message(&err);
                false
            }
        }
    }

    fn create(
        &self,
        src: &gst_base::BaseSrc,
        offset: u64,
        _length: u32,
    ) -> Result<gst::Buffer, gst::FlowError> {
        let cat = self.cat;
        let mut state = self.state.lock().unwrap();

        let (body, position) = match *state {
            State::Started {
                ref mut body,
                ref mut position,
                ..
            } => (body, position),
            State::Stopped => {
                gst_element_error!(src, gst::LibraryError::Failed, ["Not started yet"]);

                return Err(gst::FlowError::Error);
            }
        };

        if *position != offset {
            gst_element_error!(
                src,
                gst::ResourceError::Seek,
                ["Got unexpected offset {}, expected {}", offset, position]
            );

            return Err(gst::FlowError::Error);
        }

        let current_body = match body.take() {
            Some(body) => body,
            None => {
                gst_error!(self.cat, obj: src, "Don't have a response body");
                gst_element_error!(
                    src,
                    gst::ResourceError::Read,
                    ["Don't have a response body"]
                );

                return Err(gst::FlowError::Error);
            }
        };

        drop(state);
        let res = self.wait(current_body.into_future().map_err(|(err, _body)| {
            gst_error_msg!(
                gst::ResourceError::Read,
                ["Failed to read chunk: {:?}", err]
            )
        }));

        let mut state = self.state.lock().unwrap();
        let (body, position) = match *state {
            State::Started {
                ref mut body,
                ref mut position,
                ..
            } => (body, position),
            State::Stopped => {
                gst_element_error!(src, gst::LibraryError::Failed, ["Not started yet"]);

                return Err(gst::FlowError::Error);
            }
        };

        match res {
            Ok((Some(chunk), current_body)) => {
                /* do something with the chunk and store the body again in the state */

                gst_debug!(cat, obj: src, "Chunk of {} bytes received", chunk.len());
                let size = chunk.len();
                assert_ne!(chunk.len(), 0);

                *position += size as u64;

                let mut buffer = gst::Buffer::from_slice(chunk);

                *body = Some(current_body);

                {
                    let buffer = buffer.get_mut().unwrap();
                    buffer.set_offset(offset);
                }

                Ok(buffer)
            }
            Ok((None, current_body)) => {
                /* No further data, end of stream */
                gst_debug!(cat, obj: src, "End of stream");
                *body = Some(current_body);
                Err(gst::FlowError::Eos)
            }
            Err(err) => {
                /* error */

                gst_error!(self.cat, obj: src, "Failed to read: {:?}", err);
                gst_element_error!(
                    src,
                    gst::ResourceError::Read,
                    ["Failed to read at {}: {:?}", offset, err]
                );

                Err(gst::FlowError::Error)
            }
        }
    }
}

impl URIHandlerImpl for ReqwestHttpSrc {
    fn get_uri(&self, _element: &gst::URIHandler) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        settings.location.as_ref().map(Url::to_string)
    }

    fn set_uri(&self, element: &gst::URIHandler, uri: &str) -> Result<(), glib::Error> {
        let element = element.dynamic_cast_ref::<gst_base::BaseSrc>().unwrap();

        self.set_location(&element, Some(uri))
    }

    fn get_uri_type() -> gst::URIType {
        gst::URIType::Src
    }

    fn get_protocols() -> Vec<String> {
        vec!["http".to_string(), "https".to_string()]
    }
}

impl ObjectSubclass for ReqwestHttpSrc {
    const NAME: &'static str = "ReqwestHttpSrc";
    type ParentType = gst_base::BaseSrc;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            cat: gst::DebugCategory::new(
                "reqwesthttpsrc",
                gst::DebugColorFlags::empty(),
                Some("Rust HTTP source"),
            ),
            client: Mutex::new(None),
            external_client: Mutex::new(None),
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
            runtime: runtime::Builder::new()
                .core_threads(1)
                .name_prefix("gst-http-tokio")
                .build()
                .unwrap(),
            canceller: Mutex::new(None),
        }
    }

    fn type_init(type_: &mut subclass::InitializingType<Self>) {
        type_.add_interface::<gst::URIHandler>();
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "HTTP Source",
            "Source/Network/HTTP",
            "Read stream from an HTTP/HTTPS location",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "reqwesthttpsrc",
        gst::Rank::Marginal,
        ReqwestHttpSrc::get_type(),
    )
}
