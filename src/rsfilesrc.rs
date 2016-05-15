use std::u64;
use std::io::{Read, Seek, SeekFrom};
use std::fs::File;
use std::path::PathBuf;
use url::Url;

use std::io::Write;

use utils::*;
use rssource::*;

#[derive(Debug)]
pub struct FileSrc {
    location: Option<PathBuf>,
    file: Option<File>,
    position: u64,
}

unsafe impl Sync for FileSrc {}
unsafe impl Send for FileSrc {}

impl FileSrc {
    fn new() -> FileSrc {
        FileSrc { location: None, file: None, position: 0 }
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
        match self.location {
            None => None,
            Some(ref location) => Url::from_file_path(&location).map(|u| u.into_string()).ok()
        }
    }

    fn is_seekable(&self) -> bool {
        true
    }

    fn get_size(&self) -> u64 {
        match self.file {
            None => return u64::MAX,
            Some(ref f) => {
                return f.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
            },
        }
    }

    fn start(&mut self) -> bool {
        self.file = None;
        self.position = 0;

        match self.location {
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
}

