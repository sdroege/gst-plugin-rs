// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::u64;
use std::io::{Read, Seek, SeekFrom};
use std::fs::File;
use url::Url;

use gst_plugin::error::*;
use gst_plugin::source::*;
use gst_plugin::buffer::*;
use gst_plugin::log::*;
use gst_plugin::utils::*;

use slog::Logger;

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started { file: File, position: u64 },
}

#[derive(Debug)]
pub struct FileSrc {
    streaming_state: StreamingState,
    logger: Logger,
}

impl FileSrc {
    pub fn new(element: Element) -> FileSrc {
        FileSrc {
            streaming_state: StreamingState::Stopped,
            logger: Logger::root(GstDebugDrain::new(Some(&element),
                                                    "rsfilesrc",
                                                    0,
                                                    "Rust file source"),
                                 o!()),
        }
    }

    pub fn new_boxed(element: Element) -> Box<Source> {
        Box::new(FileSrc::new(element))
    }
}

fn validate_uri(uri: &Url) -> Result<(), UriError> {
    let _ = try!(uri.to_file_path().or_else(|_| {
                                                Err(UriError::new(UriErrorKind::UnsupportedProtocol,
                              Some(format!("Unsupported file URI '{}'", uri.as_str()))))
                                            }));
    Ok(())
}

impl Source for FileSrc {
    fn uri_validator(&self) -> Box<UriValidator> {
        Box::new(validate_uri)
    }

    fn is_seekable(&self) -> bool {
        true
    }

    fn get_size(&self) -> Option<u64> {
        if let StreamingState::Started { ref file, .. } = self.streaming_state {
            file.metadata().ok().map(|m| m.len())
        } else {
            None
        }
    }

    fn start(&mut self, uri: Url) -> Result<(), ErrorMessage> {
        if let StreamingState::Started { .. } = self.streaming_state {
            return Err(error_msg!(SourceError::Failure, ["Source already started"]));
        }

        let location = try!(uri.to_file_path().or_else(|_| {
            error!(self.logger, "Unsupported file URI '{}'", uri.as_str());
            Err(error_msg!(SourceError::Failure,
                           ["Unsupported file URI '{}'", uri.as_str()]))
        }));

        let file = try!(File::open(location.as_path()).or_else(|err| {
            error!(self.logger,
                   "Could not open file for reading: {}",
                   err.to_string());
            Err(error_msg!(SourceError::OpenFailed,
                           ["Could not open file for reading '{}': {}",
                            location.to_str().unwrap_or("Non-UTF8 path"),
                            err.to_string()]))
        }));

        debug!(self.logger, "Opened file {:?}", file);

        self.streaming_state = StreamingState::Started {
            file: file,
            position: 0,
        };

        Ok(())
    }

    fn stop(&mut self) -> Result<(), ErrorMessage> {
        self.streaming_state = StreamingState::Stopped;

        Ok(())
    }

    fn fill(&mut self, offset: u64, _: u32, buffer: &mut Buffer) -> Result<(), FlowError> {
        // FIXME: Because we borrow streaming state mutably below
        let logger = self.logger.clone();

        let (file, position) = match self.streaming_state {
            StreamingState::Started { ref mut file, ref mut position } => (file, position),
            StreamingState::Stopped => {
                return Err(FlowError::Error(error_msg!(SourceError::Failure, ["Not started yet"])));
            }
        };

        if *position != offset {
            try!(file.seek(SeekFrom::Start(offset)).or_else(|err| {
                error!(logger, "Failed to seek to {}: {:?}", offset, err);
                Err(FlowError::Error(error_msg!(SourceError::SeekFailed,
                                                ["Failed to seek to {}: {}",
                                                 offset,
                                                 err.to_string()])))
            }));
            *position = offset;
        }

        let size = {
            let mut map = match buffer.map_readwrite() {
                None => {
                    return Err(FlowError::Error(error_msg!(SourceError::Failure,
                                                           ["Failed to map buffer"])));
                }
                Some(map) => map,
            };

            let data = map.as_mut_slice();

            try!(file.read(data).or_else(|err| {
                error!(logger, "Failed to read: {:?}", err);
                Err(FlowError::Error(error_msg!(SourceError::ReadFailed,
                                                ["Failed to read at {}: {}",
                                                 offset,
                                                 err.to_string()])))
            }))
        };

        *position += size as u64;

        buffer.set_size(size);

        Ok(())
    }

    fn seek(&mut self, _: u64, _: Option<u64>) -> Result<(), ErrorMessage> {
        Ok(())
    }
}
