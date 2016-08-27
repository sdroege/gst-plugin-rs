//  Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
//
//  This library is free software; you can redistribute it and/or
//  modify it under the terms of the GNU Library General Public
//  License as published by the Free Software Foundation; either
//  version 2 of the License, or (at your option) any later version.
//
//  This library is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
//  Library General Public License for more details.
//
//  You should have received a copy of the GNU Library General Public
//  License along with this library; if not, write to the
//  Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
//  Boston, MA 02110-1301, USA.

use std::u64;
use std::io::Read;
use url::Url;
use hyper::header::{ContentLength, ContentRange, ContentRangeSpec, Range, ByteRangeSpec,
                    AcceptRanges, RangeUnit};
use hyper::client::Client;
use hyper::client::response::Response;

use std::sync::Mutex;

use error::*;
use rssource::*;

#[derive(Debug)]
struct Settings {
    url: Option<Url>,
}

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started {
        response: Response,
        seekable: bool,
        position: u64,
        size: u64,
        start: u64,
        stop: u64,
    },
}

#[derive(Debug)]
pub struct HttpSrc {
    controller: SourceController,
    settings: Mutex<Settings>,
    streaming_state: Mutex<StreamingState>,
    client: Client,
}

unsafe impl Sync for HttpSrc {}
unsafe impl Send for HttpSrc {}

impl HttpSrc {
    pub fn new(controller: SourceController) -> HttpSrc {
        HttpSrc {
            controller: controller,
            settings: Mutex::new(Settings { url: None }),
            streaming_state: Mutex::new(StreamingState::Stopped),
            client: Client::new(),
        }
    }

    pub fn new_boxed(controller: SourceController) -> Box<Source> {
        Box::new(HttpSrc::new(controller))
    }

    fn do_request(&self, start: u64, stop: u64) -> Result<StreamingState, ErrorMessage> {
        let url = &self.settings.lock().unwrap().url;

        let url = try!(url.as_ref()
            .ok_or_else(|| error_msg!(SourceError::Failure, ["No URI provided"])));


        let mut req = self.client.get(url.clone());

        if start != 0 || stop != u64::MAX {
            req = if stop == u64::MAX {
                req.header(Range::Bytes(vec![ByteRangeSpec::AllFrom(start)]))
            } else {
                req.header(Range::Bytes(vec![ByteRangeSpec::FromTo(start, stop - 1)]))
            };
        }

        let response = try!(req.send().or_else(|err| {
            Err(error_msg!(SourceError::ReadFailed,
                           ["Failed to fetch {}: {}", url, err.to_string()]))
        }));

        if !response.status.is_success() {
            return Err(error_msg!(SourceError::ReadFailed,
                                  ["Failed to fetch {}: {}", url, response.status]));
        }

        let size = if let Some(&ContentLength(content_length)) = response.headers.get() {
            content_length + start
        } else {
            u64::MAX
        };

        let accept_byte_ranges = if let Some(&AcceptRanges(ref ranges)) = response.headers
            .get() {
            ranges.iter().any(|u| *u == RangeUnit::Bytes)
        } else {
            false
        };

        let seekable = size != u64::MAX && accept_byte_ranges;

        let position = if let Some(&ContentRange(ContentRangeSpec::Bytes { range: Some((range_start,
                                                                                 _)),
                                                                           .. })) = response.headers
            .get() {
            range_start
        } else {
            start
        };

        if position != start {
            return Err(error_msg!(SourceError::SeekFailed,
                                  ["Failed to seek to {}: Got {}", start, position]));
        }

        Ok(StreamingState::Started {
            response: response,
            seekable: seekable,
            position: 0,
            size: size,
            start: start,
            stop: stop,
        })
    }
}

impl Source for HttpSrc {
    fn get_controller(&self) -> &SourceController {
        &self.controller
    }

    fn set_uri(&self, uri: Option<Url>) -> Result<(), UriError> {
        let url = &mut self.settings.lock().unwrap().url;

        match uri {
            None => {
                *url = None;
                Ok(())
            }
            Some(uri) => {
                if uri.scheme() != "http" && uri.scheme() != "https" {
                    *url = None;
                    return Err(UriError::new(UriErrorKind::UnsupportedProtocol,
                                             Some(format!("Unsupported URI '{}'", uri.as_str()))));
                }

                *url = Some(uri);
                Ok(())
            }
        }
    }

    fn get_uri(&self) -> Option<Url> {
        let url = &self.settings.lock().unwrap().url;
        url.as_ref().cloned()
    }

    fn is_seekable(&self) -> bool {
        let streaming_state = self.streaming_state.lock().unwrap();

        match *streaming_state {
            StreamingState::Started { seekable, .. } => seekable,
            _ => false,
        }
    }

    fn get_size(&self) -> u64 {
        let streaming_state = self.streaming_state.lock().unwrap();
        match *streaming_state {
            StreamingState::Started { size, .. } => size,
            _ => u64::MAX,
        }
    }

    fn start(&self) -> Result<(), ErrorMessage> {
        let mut streaming_state = self.streaming_state.lock().unwrap();
        *streaming_state = StreamingState::Stopped;

        let new_state = try!(self.do_request(0, u64::MAX));

        *streaming_state = new_state;
        Ok(())
    }

    fn stop(&self) -> Result<(), ErrorMessage> {
        let mut streaming_state = self.streaming_state.lock().unwrap();
        *streaming_state = StreamingState::Stopped;

        Ok(())
    }

    fn do_seek(&self, start: u64, stop: u64) -> Result<(), ErrorMessage> {
        let mut streaming_state = self.streaming_state.lock().unwrap();
        *streaming_state = StreamingState::Stopped;

        let new_state = try!(self.do_request(start, stop));

        *streaming_state = new_state;
        Ok(())
    }

    fn fill(&self, offset: u64, data: &mut [u8]) -> Result<usize, FlowError> {
        let mut streaming_state = self.streaming_state.lock().unwrap();

        if let StreamingState::Started { position, stop, .. } = *streaming_state {
            if position != offset {
                *streaming_state = StreamingState::Stopped;
                let new_state = try!(self.do_request(offset, stop)
                    .or_else(|err| Err(FlowError::Error(err))));

                *streaming_state = new_state;
            }
        }

        let (response, position) = match *streaming_state {
            StreamingState::Started { ref mut response, ref mut position, .. } => {
                (response, position)
            }
            StreamingState::Stopped => {
                return Err(FlowError::Error(error_msg!(SourceError::Failure, ["Not started yet"])));
            }
        };

        let size = try!(response.read(data).or_else(|err| {
            Err(FlowError::Error(error_msg!(SourceError::ReadFailed,
                                            ["Failed to read at {}: {}", offset, err.to_string()])))
        }));

        if size == 0 {
            return Err(FlowError::Eos);
        }

        *position += size as u64;
        Ok(size)
    }
}
