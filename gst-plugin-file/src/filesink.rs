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
use url::Url;

use std::io::Write;
use std::convert::From;

use gst_plugin::error::*;
use gst_plugin::sink::*;
use gst_plugin::buffer::*;
use gst_plugin::utils::*;
use gst_plugin::log::*;

use slog::*;

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started { file: File, position: u64 },
}

#[derive(Debug)]
pub struct FileSink {
    streaming_state: StreamingState,
    logger: Logger,
}

impl FileSink {
    pub fn new(element: Element) -> FileSink {
        FileSink {
            streaming_state: StreamingState::Stopped,
            logger: Logger::root(GstDebugDrain::new(Some(&element),
                                                    "rsfilesink",
                                                    0,
                                                    "Rust file sink"),
                                 None),
        }
    }

    pub fn new_boxed(element: Element) -> Box<Sink> {
        Box::new(FileSink::new(element))
    }
}

fn validate_uri(uri: &Url) -> Result<(), UriError> {
    let _ = try!(uri.to_file_path()
        .or_else(|_| {
            Err(UriError::new(UriErrorKind::UnsupportedProtocol,
                              Some(format!("Unsupported file URI '{}'", uri.as_str()))))
        }));
    Ok(())
}

impl Sink for FileSink {
    fn uri_validator(&self) -> Box<UriValidator> {
        Box::new(validate_uri)
    }

    fn start(&mut self, uri: Url) -> Result<(), ErrorMessage> {
        if let StreamingState::Started { .. } = self.streaming_state {
            return Err(error_msg!(SinkError::Failure, ["Sink already started"]));
        }

        let location = try!(uri.to_file_path()
            .or_else(|_| {
                error!(self.logger, "Unsupported file URI '{}'", uri.as_str());
                Err(error_msg!(SinkError::Failure,
                               ["Unsupported file URI '{}'", uri.as_str()]))
            }));


        let file = try!(File::create(location.as_path()).or_else(|err| {
            error!(self.logger,
                   "Could not open file for writing: {}",
                   err.to_string());
            Err(error_msg!(SinkError::OpenFailed,
                           ["Could not open file for writing '{}': {}",
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

    fn render(&mut self, buffer: &Buffer) -> Result<(), FlowError> {
        // FIXME: Because we borrow streaming state mutably below
        let logger = self.logger.clone();

        trace!(logger, "Rendering {:?}", buffer);

        let (file, position) = match self.streaming_state {
            StreamingState::Started { ref mut file, ref mut position } => (file, position),
            StreamingState::Stopped => {
                return Err(FlowError::Error(error_msg!(SinkError::Failure, ["Not started yet"])));
            }
        };

        let map = match buffer.map_read() {
            None => {
                return Err(FlowError::Error(error_msg!(SinkError::Failure,
                                                       ["Failed to map buffer"])));
            }
            Some(map) => map,
        };
        let data = map.as_slice();

        try!(file.write_all(data).or_else(|err| {
            error!(logger, "Failed to write: {}", err);
            Err(FlowError::Error(error_msg!(SinkError::WriteFailed, ["Failed to write: {}", err])))
        }));

        *position += data.len() as u64;

        Ok(())
    }
}
