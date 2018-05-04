// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::error::Error;
use std::fmt::Error as FmtError;
use std::fmt::{Display, Formatter};

use glib;
use gst;

#[derive(Debug, PartialEq, Eq)]
pub struct UriError {
    error: gst::URIError,
    message: String,
}

impl UriError {
    pub fn new<T: Into<String>>(error: gst::URIError, message: T) -> UriError {
        UriError {
            error: error,
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn error(&self) -> gst::URIError {
        self.error
    }
}

impl Into<glib::Error> for UriError {
    fn into(self) -> glib::Error {
        (&self).into()
    }
}

impl<'a> Into<glib::Error> for &'a UriError {
    fn into(self) -> glib::Error {
        glib::Error::new(self.error, &self.message)
    }
}

impl Display for UriError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        f.write_fmt(format_args!("{}: {}", self.description(), self.message))
    }
}

impl Error for UriError {
    fn description(&self) -> &str {
        match self.error {
            gst::URIError::UnsupportedProtocol => "Unsupported protocol",
            gst::URIError::BadUri => "Bad URI",
            gst::URIError::BadState => "Bad State",
            gst::URIError::BadReference => "Bad Reference",
            _ => "Unknown",
        }
    }
}
