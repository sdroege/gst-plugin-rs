use std::u64;
use std::io::{Read, Seek, SeekFrom};
use url::Url;
use hyper::header::ContentLength;
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
    position: u64,
    size: u64,
}

impl HttpSrc {
    fn new() -> HttpSrc {
        HttpSrc { url: None, client: Client::new(), response: None, position: 0, size: u64::MAX }
    }

    fn new_source() -> Box<Source> {
        Box::new(HttpSrc::new())
    }
    pub extern "C" fn new_ptr() -> *mut Box<Source> {
        let instance = Box::new(HttpSrc::new_source());
        return Box::into_raw(instance);
    }
}

impl Source for HttpSrc {
    fn set_uri(&mut self, uri_str: &Option<String>) -> bool {
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
        self.url.clone().map(|u| u.into_string())
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn get_size(&self) -> u64 {
        self.size
    }

    fn start(&mut self) -> bool {
        self.response = None;
        self.position = 0;
        self.size = u64::MAX;

        match self.url {
            None => return false,
            Some(ref url) => {
                match self.client.get(url.clone()).send() {
                    Ok(response) => {
                        if response.status.is_success() {
                            self.size = match response.headers.get::<ContentLength>() {
                                Some(&ContentLength(size)) => size,
                                _ => u64::MAX
                            };
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

    fn stop(&mut self) -> bool {
        self.position = 0;
        self.size = u64::MAX;
        match self.response {
            Some(ref mut response) => drop(response),
            None => ()
        }
        self.response = None;

        true
    }

    fn fill(&mut self, offset: u64, data: &mut [u8]) -> Result<usize, GstFlowReturn> {
        match self.response {
            None => return Err(GstFlowReturn::Error),
            Some(ref mut r) => {
                if self.position != offset {
                       println_err!("Failed to seek to {}", offset);
                       return Err(GstFlowReturn::Error);
                }

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

