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
use std::sync::atomic::{AtomicBool, Ordering};

use utils::*;
use rssource::*;

#[derive(Debug)]
pub struct HttpSrc {
    controller: SourceController,
    url: Mutex<Option<Url>>,
    client: Client,
    response: Option<Response>,
    seekable: AtomicBool,
    position: u64,
    size: u64,
    start: u64,
    stop: u64,
}

unsafe impl Sync for HttpSrc {}
unsafe impl Send for HttpSrc {}

impl HttpSrc {
    pub fn new(controller: SourceController) -> HttpSrc {
        HttpSrc {
            controller: controller,
            url: Mutex::new(None),
            client: Client::new(),
            response: None,
            seekable: AtomicBool::new(false),
            position: 0,
            size: u64::MAX,
            start: 0,
            stop: u64::MAX,
        }
    }

    pub fn new_boxed(controller: SourceController) -> Box<Source> {
        Box::new(HttpSrc::new(controller))
    }

    pub fn do_request(&mut self, start: u64, stop: u64) -> bool {
        self.response = None;
        self.seekable.store(false, Ordering::Relaxed);
        self.position = 0;
        self.size = u64::MAX;

        let url = self.url.lock().unwrap();
        match *url {
            None => return false,
            Some(ref url) => {
                let mut req = self.client.get(url.clone());

                if start != 0 || stop != u64::MAX {
                    req = if stop == u64::MAX {
                        req.header(Range::Bytes(vec![ByteRangeSpec::AllFrom(start)]))
                    } else {
                        req.header(Range::Bytes(vec![ByteRangeSpec::FromTo(start, stop)]))
                    };
                }

                match req.send() {
                    Ok(response) => {
                        if response.status.is_success() {
                            self.size = if let Some(&ContentLength(content_length)) =
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

                            self.seekable.store(self.size != u64::MAX && accept_byte_ranges,
                                                Ordering::Relaxed);

                            self.start = start;
                            self.stop = stop;

                            self.position = if let Some(&ContentRange(ContentRangeSpec::Bytes{range: Some((range_start, _)), ..})) = response.headers.get() {
                                range_start
                            } else {
                                start
                            };

                            if self.position != start {
                                println_err!("Failed to seek to {}: Got {}", start, self.position);
                                return false;
                            }

                            self.response = Some(response);

                            return true;
                        } else {
                            println_err!("Failed to fetch {}: {}", url, response.status);
                            return false;
                        }
                    }
                    Err(err) => {
                        println_err!("Failed to fetch {}: {}", url, err.to_string());
                        return false;
                    }
                }
            }
        }
    }
}

impl Source for HttpSrc {
    fn set_uri(&mut self, uri: Option<Url>) -> bool {
        if self.response.is_some() {
            println_err!("Can't set URI after starting");
            return false;
        }

        match uri {
            None => {
                let mut url = self.url.lock().unwrap();
                *url = None;
                return true;
            }
            Some(uri) => {
                if uri.scheme() == "http" || uri.scheme() == "https" {
                    let mut url = self.url.lock().unwrap();
                    *url = Some(uri);
                    return true;
                } else {
                    let mut url = self.url.lock().unwrap();
                    *url = None;
                    println_err!("Unsupported URI '{}'", uri.as_str());
                    return false;
                }
            }
        }
    }

    fn get_uri(&self) -> Option<Url> {
        let url = self.url.lock().unwrap();
        (*url).as_ref().map(|u| u.clone())
    }

    fn is_seekable(&self) -> bool {
        self.seekable.load(Ordering::Relaxed)
    }

    fn get_size(&self) -> u64 {
        self.size
    }

    fn start(&mut self) -> bool {
        self.seekable.store(false, Ordering::Relaxed);
        return self.do_request(0, u64::MAX);
    }

    fn stop(&mut self) -> bool {
        self.seekable.store(false, Ordering::Relaxed);
        self.position = 0;
        self.size = u64::MAX;
        match self.response {
            Some(ref mut response) => drop(response),
            None => (),
        }
        self.response = None;

        return true;
    }

    fn do_seek(&mut self, start: u64, stop: u64) -> bool {
        return self.do_request(start, stop);
    }

    fn fill(&mut self, offset: u64, data: &mut [u8]) -> Result<usize, GstFlowReturn> {
        if self.position != offset || self.response.is_none() {
            let stop = self.stop; // FIXME: Borrow checker fail
            if !self.do_request(offset, stop) {
                println_err!("Failed to seek to {}", offset);
                return Err(GstFlowReturn::Error);
            }
        }

        match self.response {
            None => return Err(GstFlowReturn::Error),
            Some(ref mut r) => {
                match r.read(data) {
                    Ok(size) => {
                        if size == 0 {
                            return Err(GstFlowReturn::Eos);
                        }

                        self.position += size as u64;
                        return Ok(size);
                    }
                    Err(err) => {
                        println_err!("Failed to read at {}: {}", offset, err.to_string());
                        return Err(GstFlowReturn::Error);
                    }
                }
            }
        }
    }
}
