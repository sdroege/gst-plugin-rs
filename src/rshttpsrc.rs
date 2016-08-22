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

use std::io::Write;
use std::sync::Mutex;

use utils::*;
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

    fn do_request(&self, start: u64, stop: u64) -> StreamingState {
        let ref url = self.settings.lock().unwrap().url;

        match *url {
            None => StreamingState::Stopped,
            Some(ref url) => {
                let mut req = self.client.get(url.clone());

                if start != 0 || stop != u64::MAX {
                    req = if stop == u64::MAX {
                        req.header(Range::Bytes(vec![ByteRangeSpec::AllFrom(start)]))
                    } else {
                        req.header(Range::Bytes(vec![ByteRangeSpec::FromTo(start, stop - 1)]))
                    };
                }

                match req.send() {
                    Ok(response) => {
                        if response.status.is_success() {
                            let size = if let Some(&ContentLength(content_length)) =
                                              response.headers.get() {
                                content_length + start
                            } else {
                                u64::MAX
                            };
                            let accept_byte_ranges = if let Some(&AcceptRanges(ref ranges)) =
                                                            response.headers.get() {
                                ranges.iter().any(|u| *u == RangeUnit::Bytes)
                            } else {
                                false
                            };

                            let seekable = size != u64::MAX && accept_byte_ranges;

                            let position = if let Some(&ContentRange(ContentRangeSpec::Bytes{range: Some((range_start, _)), ..})) = response.headers.get() {
                                range_start
                            } else {
                                start
                            };

                            if position != start {
                                println_err!("Failed to seek to {}: Got {}", start, position);
                                StreamingState::Stopped
                            } else {
                                StreamingState::Started {
                                    response: response,
                                    seekable: seekable,
                                    position: 0,
                                    size: size,
                                    start: start,
                                    stop: stop,
                                }
                            }
                        } else {
                            println_err!("Failed to fetch {}: {}", url, response.status);
                            StreamingState::Stopped
                        }
                    }
                    Err(err) => {
                        println_err!("Failed to fetch {}: {}", url, err.to_string());
                        StreamingState::Stopped
                    }
                }
            }
        }
    }
}

impl Source for HttpSrc {
    fn set_uri(&self, uri: Option<Url>) -> bool {
        let ref mut url = self.settings.lock().unwrap().url;

        match uri {
            None => {
                *url = None;
                return true;
            }
            Some(uri) => {
                if uri.scheme() == "http" || uri.scheme() == "https" {
                    *url = Some(uri);
                    return true;
                } else {
                    *url = None;
                    println_err!("Unsupported URI '{}'", uri.as_str());
                    return false;
                }
            }
        }
    }

    fn get_uri(&self) -> Option<Url> {
        let ref url = self.settings.lock().unwrap().url;
        url.as_ref().map(|u| u.clone())
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

    fn start(&self) -> bool {
        let mut streaming_state = self.streaming_state.lock().unwrap();
        *streaming_state = self.do_request(0, u64::MAX);

        if let StreamingState::Stopped = *streaming_state {
            false
        } else {
            true
        }
    }

    fn stop(&self) -> bool {
        let mut streaming_state = self.streaming_state.lock().unwrap();
        *streaming_state = StreamingState::Stopped;

        true
    }

    fn do_seek(&self, start: u64, stop: u64) -> bool {
        let mut streaming_state = self.streaming_state.lock().unwrap();
        *streaming_state = self.do_request(start, stop);

        if let StreamingState::Stopped = *streaming_state {
            false
        } else {
            true
        }
    }

    fn fill(&self, offset: u64, data: &mut [u8]) -> Result<usize, GstFlowReturn> {
        let mut streaming_state = self.streaming_state.lock().unwrap();

        if let StreamingState::Stopped = *streaming_state {
            return Err(GstFlowReturn::Error);
        }

        if let StreamingState::Started { position, stop, .. } = *streaming_state {
            if position != offset {
                *streaming_state = self.do_request(offset, stop);
                if let StreamingState::Stopped = *streaming_state {
                    println_err!("Failed to seek to {}", offset);
                    return Err(GstFlowReturn::Error);
                }
            }
        }

        if let StreamingState::Started { ref mut response, ref mut position, .. } =
               *streaming_state {
            match response.read(data) {
                Ok(size) => {
                    if size == 0 {
                        return Err(GstFlowReturn::Eos);
                    }

                    *position += size as u64;
                    Ok(size)
                }
                Err(err) => {
                    println_err!("Failed to read at {}: {}", offset, err.to_string());
                    Err(GstFlowReturn::Error)
                }
            }
        } else {
            Err(GstFlowReturn::Error)
        }
    }
}
