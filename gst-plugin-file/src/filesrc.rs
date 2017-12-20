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
use gst_plugin_simple::source::*;
use gst_plugin_simple::UriValidator;

use gst;

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started { file: File, position: u64 },
}

#[derive(Debug)]
pub struct FileSrc {
    streaming_state: StreamingState,
    cat: gst::DebugCategory,
}

impl FileSrc {
    pub fn new(_src: &BaseSrc) -> FileSrc {
        FileSrc {
            streaming_state: StreamingState::Stopped,
            cat: gst::DebugCategory::new(
                "rsfilesrc",
                gst::DebugColorFlags::empty(),
                "Rust file source",
            ),
        }
    }

    pub fn new_boxed(src: &BaseSrc) -> Box<SourceImpl> {
        Box::new(FileSrc::new(src))
    }
}

fn validate_uri(uri: &Url) -> Result<(), UriError> {
    let _ = try!(uri.to_file_path().or_else(|_| Err(UriError::new(
        gst::URIError::UnsupportedProtocol,
        format!("Unsupported file URI '{}'", uri.as_str()),
    ))));
    Ok(())
}

impl SourceImpl for FileSrc {
    fn uri_validator(&self) -> Box<UriValidator> {
        Box::new(validate_uri)
    }

    fn is_seekable(&self, _src: &BaseSrc) -> bool {
        true
    }

    fn get_size(&self, _src: &BaseSrc) -> Option<u64> {
        if let StreamingState::Started { ref file, .. } = self.streaming_state {
            file.metadata().ok().map(|m| m.len())
        } else {
            None
        }
    }

    fn start(&mut self, src: &BaseSrc, uri: Url) -> Result<(), gst::ErrorMessage> {
        if let StreamingState::Started { .. } = self.streaming_state {
            return Err(gst_error_msg!(
                gst::LibraryError::Failed,
                ["Source already started"]
            ));
        }

        let location = try!(uri.to_file_path().or_else(|_| {
            gst_error!(
                self.cat,
                obj: src,
                "Unsupported file URI '{}'",
                uri.as_str()
            );
            Err(gst_error_msg!(
                gst::LibraryError::Failed,
                ["Unsupported file URI '{}'", uri.as_str()]
            ))
        }));

        let file = try!(File::open(location.as_path()).or_else(|err| {
            gst_error!(
                self.cat,
                obj: src,
                "Could not open file for reading: {}",
                err.to_string()
            );
            Err(gst_error_msg!(
                gst::ResourceError::OpenRead,
                [
                    "Could not open file for reading '{}': {}",
                    location.to_str().unwrap_or("Non-UTF8 path"),
                    err.to_string()
                ]
            ))
        }));

        gst_debug!(self.cat, obj: src, "Opened file {:?}", file);

        self.streaming_state = StreamingState::Started {
            file: file,
            position: 0,
        };

        Ok(())
    }

    fn stop(&mut self, _src: &BaseSrc) -> Result<(), gst::ErrorMessage> {
        self.streaming_state = StreamingState::Stopped;

        Ok(())
    }

    fn fill(
        &mut self,
        src: &BaseSrc,
        offset: u64,
        _: u32,
        buffer: &mut gst::BufferRef,
    ) -> Result<(), FlowError> {
        let cat = self.cat;
        let streaming_state = &mut self.streaming_state;

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

        if *position != offset {
            try!(file.seek(SeekFrom::Start(offset)).or_else(|err| {
                gst_error!(cat, obj: src, "Failed to seek to {}: {:?}", offset, err);
                Err(FlowError::Error(gst_error_msg!(
                    gst::ResourceError::Seek,
                    ["Failed to seek to {}: {}", offset, err.to_string()]
                )))
            }));
            *position = offset;
        }

        let size = {
            let mut map = match buffer.map_writable() {
                None => {
                    return Err(FlowError::Error(gst_error_msg!(
                        gst::LibraryError::Failed,
                        ["Failed to map buffer"]
                    )));
                }
                Some(map) => map,
            };

            let data = map.as_mut_slice();

            try!(file.read(data).or_else(|err| {
                gst_error!(cat, obj: src, "Failed to read: {:?}", err);
                Err(FlowError::Error(gst_error_msg!(
                    gst::ResourceError::Read,
                    ["Failed to read at {}: {}", offset, err.to_string()]
                )))
            }))
        };

        *position += size as u64;

        buffer.set_size(size);

        Ok(())
    }

    fn seek(&mut self, _src: &BaseSrc, _: u64, _: Option<u64>) -> Result<(), gst::ErrorMessage> {
        Ok(())
    }
}
