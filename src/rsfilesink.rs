//  Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
//                2016 Luis de Bethencourt <luisbg@osg.samsung.com>
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

use std::fs::File;
use std::path::PathBuf;
use url::Url;

use std::io::Write;

use utils::*;
use rssink::*;

#[derive(Debug)]
pub struct FileSink {
    location: Option<PathBuf>,
    file: Option<File>,
    position: u64,
}

unsafe impl Sync for FileSink {}
unsafe impl Send for FileSink {}

impl FileSink {
    fn new() -> FileSink {
        FileSink { location: None, file: None, position: 0 }
    }

    fn new_source() -> Box<Sink> {
        Box::new(FileSink::new())
    }
    pub extern "C" fn new_ptr() -> *mut Box<Sink> {
        let instance = Box::new(FileSink::new_source());
        return Box::into_raw(instance);
    }
}

impl Sink for FileSink {
    fn set_uri(&mut self, uri_str: &Option<String>) -> bool {
        match *uri_str {
            None => {
                self.location = None;
                return true;
            },
            Some(ref uri_str) => {
                let uri_parsed = Url::parse(uri_str.as_str());
                match uri_parsed {
                    Ok(u) => {
                        match u.to_file_path().ok() {
                            Some(p) => {
                                self.location = Some(p);
                                return true;
                            },
                            None => {
                                self.location = None;
                                println_err!("Unsupported file URI '{}'", uri_str);
                                return false;
                            }
                        }
                    },
                    Err(err) => {
                        self.location = None;
                        println_err!("Failed to parse URI '{}': {}", uri_str, err);
                        return false;
                    }
                }
            }
        }
    }

    fn get_uri(&self) -> Option<String> {
        self.location.as_ref()
            .map(|l| Url::from_file_path(l).ok())
            .and_then(|i| i) // join()
            .map(|u| u.into_string())
    }

    fn start(&mut self) -> bool {
        self.file = None;
        self.position = 0;

        match self.location {
            None => return false,
            Some(ref location) => {
                match File::create(location.as_path()) {
                    Ok(file) => {
                        self.file = Some(file);
                        return true;
                    },
                    Err(err) => {
                        println_err!("Could not open file for writing '{}': {}", location.to_str().unwrap_or("Non-UTF8 path"), err.to_string());
                        return false;
                    }
                }
            },
        }
    }

    fn stop(&mut self) -> bool {
        self.file = None;
        self.position = 0;

        true
    }

    fn render(&mut self, data: &[u8]) -> GstFlowReturn {
        match self.file {
            None => return GstFlowReturn::Error,
            Some(ref mut f) => {
                match f.write_all(data) {
                    Ok(_) => {
                        return GstFlowReturn::Ok
                    },
                    Err(err) => {
                        println_err!("Failed to write: {}", err);
                        return GstFlowReturn::Error
                    },
                }
            },
        }
    }
}
