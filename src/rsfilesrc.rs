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

use error::*;
use rssource::*;

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
pub struct FileSrc {
    controller: SourceController,
    settings: Mutex<Settings>,
    streaming_state: Mutex<StreamingState>,
}

unsafe impl Sync for FileSrc {}
unsafe impl Send for FileSrc {}

impl FileSrc {
    pub fn new(controller: SourceController) -> FileSrc {
        FileSrc {
            controller: controller,
            settings: Mutex::new(Settings { location: None }),
            streaming_state: Mutex::new(StreamingState::Stopped),
        }
    }

    pub fn new_boxed(controller: SourceController) -> Box<Source> {
        Box::new(FileSrc::new(controller))
    }
}

impl Source for FileSrc {
    fn get_controller(&self) -> &SourceController {
        &self.controller
    }

    fn set_uri(&self, uri: Option<Url>) -> Result<(), UriError> {
        let location = &mut self.settings.lock().unwrap().location;

        match uri {
            None => {
                *location = None;
                Ok(())
            }
            Some(ref uri) => {
                *location = Some(try!(uri.to_file_path()
                    .or_else(|_| {
                        Err(UriError::new(UriErrorKind::UnsupportedProtocol,
                                          Some(format!("Unsupported file URI '{}'", uri.as_str()))))
                    })));
                Ok(())
            }
        }
    }

    fn get_uri(&self) -> Option<Url> {
        let location = &self.settings.lock().unwrap().location;

        location.as_ref()
            .map(|l| Url::from_file_path(l).ok())
            .and_then(|i| i) // join()
    }

    fn is_seekable(&self) -> bool {
        true
    }

    fn get_size(&self) -> u64 {
        let streaming_state = self.streaming_state.lock().unwrap();

        if let StreamingState::Started { ref file, .. } = *streaming_state {
            file.metadata()
                .ok()
                .map_or(u64::MAX, |m| m.len())
        } else {
            u64::MAX
        }
    }

    fn start(&self) -> Result<(), ErrorMessage> {
        let location = &self.settings.lock().unwrap().location;
        let mut streaming_state = self.streaming_state.lock().unwrap();

        if let StreamingState::Started { .. } = *streaming_state {
            return Err(error_msg!(SourceError::Failure, ["Source already started"]));
        }

        let location = &try!(location.as_ref()
            .ok_or_else(|| error_msg!(SourceError::Failure, ["No URI provided"])));

        let file = try!(File::open(location.as_path()).or_else(|err| {
            Err(error_msg!(SourceError::OpenFailed,
                           ["Could not open file for reading '{}': {}",
                            location.to_str().unwrap_or("Non-UTF8 path"),
                            err.to_string()]))
        }));

        *streaming_state = StreamingState::Started {
            file: file,
            position: 0,
        };

        Ok(())
    }

    fn stop(&self) -> Result<(), ErrorMessage> {
        let mut streaming_state = self.streaming_state.lock().unwrap();
        *streaming_state = StreamingState::Stopped;

        Ok(())
    }

    fn fill(&self, offset: u64, data: &mut [u8]) -> Result<usize, FlowError> {
        let mut streaming_state = self.streaming_state.lock().unwrap();


        let (file, position) = match *streaming_state {
            StreamingState::Started { ref mut file, ref mut position } => (file, position),
            StreamingState::Stopped => {
                return Err(FlowError::Error(error_msg!(SourceError::Failure, ["Not started yet"])));
            }
        };

        if *position != offset {
            try!(file.seek(SeekFrom::Start(offset)).or_else(|err| {
                Err(FlowError::Error(error_msg!(SourceError::SeekFailed,
                                                ["Failed to seek to {}: {}",
                                                 offset,
                                                 err.to_string()])))
            }));
            *position = offset;
        }

        let size = try!(file.read(data).or_else(|err| {
            Err(FlowError::Error(error_msg!(SourceError::ReadFailed,
                                            ["Failed to read at {}: {}", offset, err.to_string()])))
        }));

        *position += size as u64;

        Ok(size)
    }

    fn do_seek(&self, _: u64, _: u64) -> Result<(), ErrorMessage> {
        Ok(())
    }
}
