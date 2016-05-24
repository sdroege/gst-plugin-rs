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
use std::io::{Read, Seek, SeekFrom};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;
use url::Url;

use std::io::Write;

use utils::*;
use rssource::*;

#[derive(Debug)]
pub struct FileSrc {
    location: Mutex<Option<PathBuf>>,
    file: Option<File>,
    position: u64,
}

unsafe impl Sync for FileSrc {}
unsafe impl Send for FileSrc {}

impl FileSrc {
    fn new() -> FileSrc {
        FileSrc { location: Mutex::new(None), file: None, position: 0 }
    }

    fn new_source() -> Box<Source> {
        Box::new(FileSrc::new())
    }
    pub extern "C" fn new_ptr() -> *mut Box<Source> {
        let instance = Box::new(FileSrc::new_source());
        return Box::into_raw(instance);
    }
}

impl Source for FileSrc {
    fn set_uri(&mut self, uri: Option<Url>) -> bool {
        match uri {
            None => {
                let mut location = self.location.lock().unwrap();
                *location = None;
                return true;
            },
            Some(uri) => {
                match uri.to_file_path().ok() {
                    Some(p) => {
                        let mut location = self.location.lock().unwrap();
                        *location = Some(p);
                        return true;
                    },
                    None => {
                        let mut location = self.location.lock().unwrap();
                        *location = None;
                        println_err!("Unsupported file URI '{}'", uri.as_str());
                        return false;
                    }
                }
            }
        }
    }

    fn get_uri(&self) -> Option<Url> {
        let location = self.location.lock().unwrap();
        (*location).as_ref()
            .map(|l| Url::from_file_path(l).ok())
            .and_then(|i| i) // join()
    }

    fn is_seekable(&self) -> bool {
        true
    }

    fn get_size(&self) -> u64 {
        self.file.as_ref()
            .map(|f| f.metadata().ok())
            .and_then(|i| i) // join()
            .map(|m| m.len())
            .unwrap_or(u64::MAX)
    }

    fn start(&mut self) -> bool {
        self.file = None;
        self.position = 0;
        let location = self.location.lock().unwrap();

        match *location {
            None => return false,
            Some(ref location) => {
                match File::open(location.as_path()) {
                    Ok(file) => {
                        self.file = Some(file);
                        return true;
                    },
                    Err(err) => {
                        println_err!("Failed to open file '{}': {}", location.to_str().unwrap_or("Non-UTF8 path"), err.to_string());
                        return false;
                    },
                }
            },
        }
    }

    fn stop(&mut self) -> bool {
        self.file = None;
        self.position = 0;

        true
    }

    fn fill(&mut self, offset: u64, data: &mut [u8]) -> Result<usize, GstFlowReturn> {
        match self.file {
            None => return Err(GstFlowReturn::Error),
            Some(ref mut f) => {
                if self.position != offset {
                    match f.seek(SeekFrom::Start(offset)) {
                        Ok(_) => {
                            self.position = offset;
                        },
                        Err(err) => {
                            println_err!("Failed to seek to {}: {}", offset, err.to_string());
                            return Err(GstFlowReturn::Error);
                        }
                    }
                }

                match f.read(data) {
                    Ok(size) => {
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

    fn do_seek(&mut self, _: u64, _: u64) -> bool {
        true
    }
}

