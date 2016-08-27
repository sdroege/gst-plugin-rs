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
use std::convert::From;

use error::*;
use rssink::*;

macro_rules! error_msg(
// Format strings
    ($err:expr, ($($msg:tt)*), [$($dbg:tt)*]) =>  { {
        ErrorMessage::new(&$err, Some(From::from(format!($($msg)*))),
                          From::from(Some(format!($($dbg)*))),
                          file!(), module_path!(), line!())
    }};
    ($err:expr, ($($msg:tt)*)) =>  { {
        ErrorMessage::new(&$err, Some(From::from(format!($($msg)*))),
                          None,
                          file!(), module_path!(), line!())
    }};

    ($err:expr, [$($dbg:tt)*]) =>  { {
        ErrorMessage::new(&$err, None,
                          Some(From::from(format!($($dbg)*))),
                          file!(), module_path!(), line!())
    }};

// Plain strings
    ($err:expr, ($msg:expr), [$dbg:expr]) =>  {
        ErrorMessage::new(&$err, Some(From::from($msg)),
                          Some(From::from($dbg)),
                          file!(), module_path!(), line!())
    };
    ($err:expr, ($msg:expr)) => {
        ErrorMessage::new(&$err, Some(From::from($msg)),
                          None,
                          file!(), module_path!(), line!())
    };
    ($err:expr, [$dbg:expr]) => {
        ErrorMessage::new(&$err, None,
                          Some(From::from($dbg)),
                          file!(), module_path!(), line!())
    };
);

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
    fn get_controller(&self) -> &SinkController {
        &self.controller
    }

    fn set_uri(&self, uri: Option<Url>) -> Result<(), UriError> {
        let location = &mut self.settings.lock().unwrap().location;

        *location = None;
        match uri {
            None => Ok(()),
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

    fn start(&self) -> Result<(), ErrorMessage> {
        let location = &self.settings.lock().unwrap().location;
        let mut streaming_state = self.streaming_state.lock().unwrap();

        if let StreamingState::Started { .. } = *streaming_state {
            return Err(error_msg!(SinkError::Failure, ["Sink already started"]));
        }

        let location = &try!(location.as_ref().ok_or_else(|| {
            error_msg!(SinkError::Failure, ["No URI provided"])
        }));

        let file = try!(File::create(location.as_path()).or_else(|err| {
            Err(error_msg!(SinkError::OpenFailed,
                           ["Could not open file for writing '{}': {}",
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

    fn render(&self, data: &[u8]) -> Result<(), FlowError> {
        let mut streaming_state = self.streaming_state.lock().unwrap();

        let (file, position) = match *streaming_state {
            StreamingState::Started { ref mut file, ref mut position } => (file, position),
            StreamingState::Stopped => {
                return Err(FlowError::Error(error_msg!(SinkError::Failure, ["Not started yet"])));
            }
        };

        try!(file.write_all(data).or_else(|err| {
            Err(FlowError::Error(error_msg!(SinkError::WriteFailed, ["Failed to write: {}", err])))
        }));

        *position += data.len() as u64;
        Ok(())
    }
}
