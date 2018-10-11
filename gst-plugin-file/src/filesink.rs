// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2016 Luis de Bethencourt <luisbg@osg.samsung.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::fs::File;
use url::Url;

use std::io::Write;

use gst_plugin::error::*;
use gst_plugin_simple::error::*;
use gst_plugin_simple::sink::*;
use gst_plugin_simple::UriValidator;

use gst;

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started { file: File, position: u64 },
}

#[derive(Debug)]
pub struct FileSink {
    streaming_state: StreamingState,
    cat: gst::DebugCategory,
}

impl FileSink {
    pub fn new(_sink: &BaseSink) -> FileSink {
        FileSink {
            streaming_state: StreamingState::Stopped,
            cat: gst::DebugCategory::new(
                "rsfilesink",
                gst::DebugColorFlags::empty(),
                "Rust file source",
            ),
        }
    }

    pub fn new_boxed(sink: &BaseSink) -> Box<SinkImpl> {
        Box::new(FileSink::new(sink))
    }
}

fn validate_uri(uri: &Url) -> Result<(), UriError> {
    let _ = try!(uri.to_file_path().or_else(|_| Err(UriError::new(
        gst::URIError::UnsupportedProtocol,
        format!("Unsupported file URI '{}'", uri.as_str()),
    ))));
    Ok(())
}

impl SinkImpl for FileSink {
    fn uri_validator(&self) -> Box<UriValidator> {
        Box::new(validate_uri)
    }

    fn start(&mut self, sink: &BaseSink, uri: Url) -> Result<(), gst::ErrorMessage> {
        if let StreamingState::Started { .. } = self.streaming_state {
            return Err(gst_error_msg!(
                gst::LibraryError::Failed,
                ["Sink already started"]
            ));
        }

        let location = try!(uri.to_file_path().or_else(|_| {
            gst_error!(
                self.cat,
                obj: sink,
                "Unsupported file URI '{}'",
                uri.as_str()
            );
            Err(gst_error_msg!(
                gst::LibraryError::Failed,
                ["Unsupported file URI '{}'", uri.as_str()]
            ))
        }));

        let file = try!(File::create(location.as_path()).or_else(|err| {
            gst_error!(
                self.cat,
                obj: sink,
                "Could not open file for writing: {}",
                err.to_string()
            );
            Err(gst_error_msg!(
                gst::ResourceError::OpenWrite,
                [
                    "Could not open file for writing '{}': {}",
                    location.to_str().unwrap_or("Non-UTF8 path"),
                    err.to_string(),
                ]
            ))
        }));

        gst_debug!(self.cat, obj: sink, "Opened file {:?}", file);

        self.streaming_state = StreamingState::Started {
            file,
            position: 0,
        };

        Ok(())
    }

    fn stop(&mut self, _sink: &BaseSink) -> Result<(), gst::ErrorMessage> {
        self.streaming_state = StreamingState::Stopped;

        Ok(())
    }

    fn render(&mut self, sink: &BaseSink, buffer: &gst::BufferRef) -> Result<(), FlowError> {
        let cat = self.cat;
        let streaming_state = &mut self.streaming_state;

        gst_trace!(cat, obj: sink, "Rendering {:?}", buffer);

        let (file, position) = match *streaming_state {
            StreamingState::Started {
                ref mut file,
                ref mut position,
            } => (file, position),
            StreamingState::Stopped => {
                return Err(FlowError::Error(gst_error_msg!(
                    gst::LibraryError::Failed,
                    ["Not started yet"]
                )));
            }
        };

        let map = match buffer.map_readable() {
            None => {
                return Err(FlowError::Error(gst_error_msg!(
                    gst::LibraryError::Failed,
                    ["Failed to map buffer"]
                )));
            }
            Some(map) => map,
        };
        let data = map.as_slice();

        try!(file.write_all(data).or_else(|err| {
            gst_error!(cat, obj: sink, "Failed to write: {}", err);
            Err(FlowError::Error(gst_error_msg!(
                gst::ResourceError::Write,
                ["Failed to write: {}", err]
            )))
        }));

        *position += data.len() as u64;

        Ok(())
    }
}
