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
use std::sync::Mutex;

use utils::*;
use rssink::*;

#[derive(Debug)]
struct Settings {
    location: Option<PathBuf>,
}

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started { file: File, position: u64 },
}

#[derive(Debug)]
pub struct FileSink {
    controller: SinkController,
    settings: Mutex<Settings>,
    streaming_state: Mutex<StreamingState>,
}

unsafe impl Sync for FileSink {}
unsafe impl Send for FileSink {}

impl FileSink {
    pub fn new(controller: SinkController) -> FileSink {
        FileSink {
            controller: controller,
            settings: Mutex::new(Settings { location: None }),
            streaming_state: Mutex::new(StreamingState::Stopped),
        }
    }

    pub fn new_boxed(controller: SinkController) -> Box<Sink> {
        Box::new(FileSink::new(controller))
    }
}

impl Sink for FileSink {
    fn set_uri(&self, uri: Option<Url>) -> Result<(), (UriError, String)> {
        let location = &mut self.settings.lock().unwrap().location;

        match uri {
            None => {
                *location = None;
                Ok(())
            }
            Some(ref uri) => {
                match uri.to_file_path().ok() {
                    Some(p) => {
                        *location = Some(p);
                        Ok(())
                    }
                    None => {
                        *location = None;
                        Err((UriError::UnsupportedProtocol,
                             format!("Unsupported file URI '{}'", uri.as_str())))
                    }
                }
            }
        }
    }

    fn get_uri(&self) -> Option<Url> {
        let location = &self.settings.lock().unwrap().location;
        location.as_ref()
            .map(|l| Url::from_file_path(l).ok())
            .and_then(|i| i) // join()
    }

    fn start(&self) -> bool {
        let location = &self.settings.lock().unwrap().location;
        let mut streaming_state = self.streaming_state.lock().unwrap();

        if let StreamingState::Started { .. } = *streaming_state {
            return false;
        }

        match *location {
            None => false,
            Some(ref location) => {
                match File::create(location.as_path()) {
                    Ok(file) => {
                        *streaming_state = StreamingState::Started {
                            file: file,
                            position: 0,
                        };
                        true
                    }
                    Err(err) => {
                        println_err!("Could not open file for writing '{}': {}",
                                     location.to_str().unwrap_or("Non-UTF8 path"),
                                     err.to_string());
                        false
                    }
                }
            }
        }
    }

    fn stop(&self) -> bool {
        let mut streaming_state = self.streaming_state.lock().unwrap();
        *streaming_state = StreamingState::Stopped;

        true
    }

    fn render(&self, data: &[u8]) -> GstFlowReturn {
        let mut streaming_state = self.streaming_state.lock().unwrap();

        if let StreamingState::Started { ref mut file, ref mut position } = *streaming_state {
            match file.write_all(data) {
                Ok(_) => {
                    *position += data.len() as u64;
                    GstFlowReturn::Ok
                }
                Err(err) => {
                    println_err!("Failed to write: {}", err);
                    GstFlowReturn::Error
                }
            }
        } else {
            GstFlowReturn::Error
        }
    }
}
