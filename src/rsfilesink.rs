use std::u64;
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
        match self.location {
            None => None,
            Some(ref location) => Url::from_file_path(&location).map(|u| u.into_string()).ok()
        }
    }

    fn start(&mut self) -> bool {
        self.file = None;
        self.position = 0;

        match self.location {
            None => return false,
            Some(ref location) => {
                return true;
            },
        }
    }

    fn stop(&mut self) -> bool {
        self.file = None;
        self.position = 0;

        true
    }

    fn render(&mut self) -> Result<usize, GstFlowReturn> {
        println!("Got a buffer!");
        Err(GstFlowReturn::Ok)
    }
}
