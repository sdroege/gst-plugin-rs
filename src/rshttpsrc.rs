use std::u64;
use std::io::{Read, Seek, SeekFrom};
use url::Url;
use hyper::header::{ContentLength, ContentRange, ContentRangeSpec, Range, ByteRangeSpec, AcceptRanges, RangeUnit};
use hyper::client::Client;
use hyper::client::response::Response;

use std::io::Write;

use utils::*;
use rssource::*;

#[derive(Debug)]
pub struct HttpSrc {
    url: Option<Url>,
    client: Client,
    response: Option<Response>,
    seekable: bool,
    position: u64,
    size: u64,
    start: u64,
    stop: u64,
}

unsafe impl Sync for HttpSrc {}
unsafe impl Send for HttpSrc {}

impl HttpSrc {
    fn new() -> HttpSrc {
        HttpSrc { url: None, client: Client::new(), response: None, seekable: false, position: 0, size: u64::MAX, start: 0, stop: u64::MAX }
    }

    fn new_source() -> Box<Source> {
        Box::new(HttpSrc::new())
    }
    pub extern "C" fn new_ptr() -> *mut Box<Source> {
        let instance = Box::new(HttpSrc::new_source());
        return Box::into_raw(instance);
    }

    pub fn do_request(&mut self, start: u64, stop: u64) -> bool {
        self.response = None;
        self.seekable = false;
        self.position = 0;
        self.size = u64::MAX;

        match self.url {
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
                            self.size = if let Some(&ContentLength(content_length)) = response.headers.get() {
                                content_length + start
                            } else {
                                u64::MAX
                            };
                            let accept_byte_ranges = if let Some(&AcceptRanges(ref ranges)) = response.headers.get() {
                                ranges.iter().any(|u| *u == RangeUnit::Bytes)
                            } else {
                                false
                            };

                            self.seekable = self.size != u64::MAX && accept_byte_ranges;

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
                    },
                    Err(err) => {
                        println_err!("Failed to fetch {}: {}", url, err.to_string());
                        return false;
                    }
                }
            },
        }
    }
}

impl Source for HttpSrc {
    fn set_uri(&mut self, uri_str: &Option<String>) -> bool {
        if self.response.is_some() {
            println_err!("Can't set URI after starting");
            return false;
        }

        match *uri_str {
            None => {
                self.url = None;
                return true;
            },
            Some(ref uri_str) => {
                let uri_parsed = Url::parse(uri_str.as_str());
                match uri_parsed {
                    Ok(u) => {
                        if u.scheme() == "http" ||
                           u.scheme() == "https" {
                            self.url = Some(u);
                            return true;
                        } else {
                            self.url = None;
                            println_err!("Unsupported file URI '{}'", uri_str);
                            return false;
                        }
                    },
                    Err(err) => {
                        self.url = None;
                        println_err!("Failed to parse URI '{}': {}", uri_str, err);
                        return false;
                    }
                }
            }
        }
    }

    fn get_uri(&self) -> Option<String> {
        self.url.as_ref().map(|u| String::from(u.as_str()))
    }

    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn get_size(&self) -> u64 {
        self.size
    }

    fn start(&mut self) -> bool {
        self.seekable = false;
        return self.do_request(0, u64::MAX);
    }

    fn stop(&mut self) -> bool {
        self.seekable = false;
        self.position = 0;
        self.size = u64::MAX;
        match self.response {
            Some(ref mut response) => drop(response),
            None => ()
        }
        self.response = None;

        return true;
    }

    fn do_seek(&mut self, start: u64, stop: u64) -> bool {
        return self.do_request(start, stop);
    }

    fn fill(&mut self, offset: u64, data: &mut [u8]) -> Result<usize, GstFlowReturn> {
        if self.position != offset || self.response.is_none() {
            let stop = self.stop;
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
                        return Ok(size)
                    },
                    Err(err) => {
                        println_err!("Failed to read at {}: {}", offset, err.to_string());
                        return Err(GstFlowReturn::Error);
                    },
                }
            },
        }
    }
}

