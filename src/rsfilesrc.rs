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
use url::Url;

use error::*;
use rssource::*;

#[derive(Debug)]
enum StreamingState {
    Stopped,
    Started { file: File, position: u64 },
}

#[derive(Debug)]
pub struct FileSrc {
    streaming_state: StreamingState,
}

impl FileSrc {
    pub fn new() -> FileSrc {
        FileSrc { streaming_state: StreamingState::Stopped }
    }

    pub fn new_boxed() -> Box<Source> {
        Box::new(FileSrc::new())
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

impl Source for FileSrc {
    fn uri_validator(&self) -> Box<UriValidator> {
        Box::new(validate_uri)
    }

    fn is_seekable(&self) -> bool {
        true
    }

    fn get_size(&self) -> Option<u64> {
        if let StreamingState::Started { ref file, .. } = self.streaming_state {
            file.metadata()
                .ok()
                .map(|m| m.len())
        } else {
            None
        }
    }

    fn start(&mut self, uri: &Url) -> Result<(), ErrorMessage> {
        if let StreamingState::Started { .. } = self.streaming_state {
            return Err(error_msg!(SourceError::Failure, ["Source already started"]));
        }

        let location = try!(uri.to_file_path()
            .or_else(|_| {
                Err(error_msg!(SourceError::Failure,
                               ["Unsupported file URI '{}'", uri.as_str()]))
            }));

        let file = try!(File::open(location.as_path()).or_else(|err| {
            Err(error_msg!(SourceError::OpenFailed,
                           ["Could not open file for reading '{}': {}",
                            location.to_str().unwrap_or("Non-UTF8 path"),
                            err.to_string()]))
        }));

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

    fn fill(&mut self, offset: u64, data: &mut [u8]) -> Result<usize, FlowError> {
        let (file, position) = match self.streaming_state {
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

    fn seek(&mut self, _: u64, _: Option<u64>) -> Result<(), ErrorMessage> {
        Ok(())
    }
}
