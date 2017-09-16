// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::u64;
use std::io::Read;
use url::Url;
use reqwest::{Client, Response};
use reqwest::header::{AcceptRanges, ByteRangeSpec, ContentLength, ContentRange, ContentRangeSpec,
                      Range, RangeUnit};

use gst_plugin::error::*;
use gst_plugin::source::*;

use gst;
use gst::prelude::*;

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started {
        uri: Url,
        response: Response,
        seekable: bool,
        position: u64,
        size: Option<u64>,
        start: u64,
        stop: Option<u64>,
    },
}

#[derive(Debug)]
pub struct HttpSrc {
    streaming_state: StreamingState,
    cat: gst::DebugCategory,
    client: Client,
}

impl HttpSrc {
    pub fn new(_src: &RsSrcWrapper) -> HttpSrc {
        HttpSrc {
            streaming_state: StreamingState::Stopped,
            cat: gst::DebugCategory::new(
                "rshttpsrc",
                gst::DebugColorFlags::empty(),
                "Rust HTTP source",
            ),
            client: Client::new().unwrap(),
        }
    }

    pub fn new_boxed(src: &RsSrcWrapper) -> Box<Source> {
        Box::new(HttpSrc::new(src))
    }

    fn do_request(
        &self,
        src: &RsSrcWrapper,
        uri: Url,
        start: u64,
        stop: Option<u64>,
    ) -> Result<StreamingState, ErrorMessage> {
        let cat = self.cat;
        let mut req = self.client.get(uri.clone()).unwrap();

        match (start != 0, stop) {
            (false, None) => (),
            (true, None) => {
                req.header(Range::Bytes(vec![ByteRangeSpec::AllFrom(start)]));
            }
            (_, Some(stop)) => {
                req.header(Range::Bytes(vec![ByteRangeSpec::FromTo(start, stop - 1)]));
            }
        }

        gst_debug!(cat, obj: src, "Doing new request {:?}", req);

        let response = try!(req.send().or_else(|err| {
            gst_error!(cat, obj: src, "Request failed: {:?}", err);
            Err(error_msg!(
                gst::ResourceError::Read,
                ["Failed to fetch {}: {}", uri, err.to_string()]
            ))
        }));

        if !response.status().is_success() {
            gst_error!(cat, obj: src, "Request status failed: {:?}", response);
            return Err(error_msg!(
                gst::ResourceError::Read,
                ["Failed to fetch {}: {}", uri, response.status()]
            ));
        }

        let size = response
            .headers()
            .get()
            .map(|&ContentLength(cl)| cl + start);

        let accept_byte_ranges = if let Some(&AcceptRanges(ref ranges)) = response.headers().get() {
            ranges.iter().any(|u| *u == RangeUnit::Bytes)
        } else {
            false
        };

        let seekable = size.is_some() && accept_byte_ranges;

        let position = if let Some(
            &ContentRange(ContentRangeSpec::Bytes {
                range: Some((range_start, _)),
                ..
            }),
        ) = response.headers().get()
        {
            range_start
        } else {
            start
        };

        if position != start {
            return Err(error_msg!(
                gst::ResourceError::Seek,
                ["Failed to seek to {}: Got {}", start, position]
            ));
        }

        gst_debug!(cat, obj: src, "Request successful: {:?}", response);

        Ok(StreamingState::Started {
            uri: uri,
            response: response,
            seekable: seekable,
            position: 0,
            size: size,
            start: start,
            stop: stop,
        })
    }
}

fn validate_uri(uri: &Url) -> Result<(), UriError> {
    if uri.scheme() != "http" && uri.scheme() != "https" {
        return Err(UriError::new(
            gst::URIError::UnsupportedProtocol,
            format!("Unsupported URI '{}'", uri.as_str()),
        ));
    }

    Ok(())
}

impl Source for HttpSrc {
    fn uri_validator(&self) -> Box<UriValidator> {
        Box::new(validate_uri)
    }

    fn is_seekable(&self, _src: &RsSrcWrapper) -> bool {
        match self.streaming_state {
            StreamingState::Started { seekable, .. } => seekable,
            _ => false,
        }
    }

    fn get_size(&self, _src: &RsSrcWrapper) -> Option<u64> {
        match self.streaming_state {
            StreamingState::Started { size, .. } => size,
            _ => None,
        }
    }

    fn start(&mut self, src: &RsSrcWrapper, uri: Url) -> Result<(), ErrorMessage> {
        self.streaming_state = StreamingState::Stopped;
        self.streaming_state = try!(self.do_request(src, uri, 0, None));

        Ok(())
    }

    fn stop(&mut self, _src: &RsSrcWrapper) -> Result<(), ErrorMessage> {
        self.streaming_state = StreamingState::Stopped;

        Ok(())
    }

    fn seek(
        &mut self,
        src: &RsSrcWrapper,
        start: u64,
        stop: Option<u64>,
    ) -> Result<(), ErrorMessage> {
        let (position, old_stop, uri) = match self.streaming_state {
            StreamingState::Started {
                position,
                stop,
                ref uri,
                ..
            } => (position, stop, uri.clone()),
            StreamingState::Stopped => {
                return Err(error_msg!(gst::LibraryError::Failed, ["Not started yet"]));
            }
        };

        if position == start && old_stop == stop {
            return Ok(());
        }

        self.streaming_state = StreamingState::Stopped;
        self.streaming_state = try!(self.do_request(src, uri, start, stop));

        Ok(())
    }

    fn fill(
        &mut self,
        src: &RsSrcWrapper,
        offset: u64,
        _: u32,
        buffer: &mut gst::BufferRef,
    ) -> Result<(), FlowError> {
        let cat = self.cat;

        let (response, position) = match self.streaming_state {
            StreamingState::Started {
                ref mut response,
                ref mut position,
                ..
            } => (response, position),
            StreamingState::Stopped => {
                return Err(FlowError::Error(
                    error_msg!(gst::LibraryError::Failed, ["Not started yet"]),
                ));
            }
        };

        if *position != offset {
            return Err(FlowError::Error(error_msg!(
                gst::ResourceError::Seek,
                ["Got unexpected offset {}, expected {}", offset, position]
            )));
        }

        let size = {
            let mut map = match buffer.map_writable() {
                None => {
                    return Err(FlowError::Error(error_msg!(
                        gst::LibraryError::Failed,
                        ["Failed to map buffer"]
                    )));
                }
                Some(map) => map,
            };

            let data = map.as_mut_slice();

            try!(response.read(data).or_else(|err| {
                gst_error!(cat, obj: src, "Failed to read: {:?}", err);
                Err(FlowError::Error(error_msg!(
                    gst::ResourceError::Read,
                    ["Failed to read at {}: {}", offset, err.to_string()]
                )))
            }))
        };

        if size == 0 {
            return Err(FlowError::Eos);
        }

        *position += size as u64;

        buffer.set_size(size);

        Ok(())
    }
}
