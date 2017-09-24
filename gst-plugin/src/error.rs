// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fmt::Error as FmtError;
use std::borrow::Cow;

use url::Url;

use glib_ffi;
use gst_ffi;

use glib;
use glib::translate::ToGlibPtr;
use gst;
use gst::prelude::*;

#[macro_export]
macro_rules! error_msg(
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
);

#[derive(Debug, PartialEq, Eq)]
pub struct ErrorMessage {
    error_domain: glib_ffi::GQuark,
    error_code: i32,
    message: Option<String>,
    debug: Option<String>,
    filename: &'static str,
    function: &'static str,
    line: u32,
}

impl ErrorMessage {
    pub fn new<T: gst::MessageErrorDomain>(
        error: &T,
        message: Option<Cow<str>>,
        debug: Option<Cow<str>>,
        filename: &'static str,
        function: &'static str,
        line: u32,
    ) -> ErrorMessage {
        let domain = T::domain();
        let code = error.code();

        ErrorMessage {
            error_domain: domain,
            error_code: code,
            message: message.map(|m| m.into_owned()),
            debug: debug.map(|d| d.into_owned()),
            filename: filename,
            function: function,
            line: line,
        }
    }

    pub fn post<E: IsA<gst::Element>>(&self, element: &E) {
        let ErrorMessage {
            error_domain,
            error_code,
            ref message,
            ref debug,
            filename,
            function,
            line,
        } = *self;

        unsafe {
            gst_ffi::gst_element_message_full(
                element.to_glib_none().0,
                gst_ffi::GST_MESSAGE_ERROR,
                error_domain,
                error_code,
                message.to_glib_full(),
                debug.to_glib_full(),
                filename.to_glib_none().0,
                function.to_glib_none().0,
                line as i32,
            );
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FlowError {
    Flushing,
    Eos,
    NotNegotiated(ErrorMessage),
    Error(ErrorMessage),
}

impl FlowError {
    pub fn to_native(&self) -> gst::FlowReturn {
        match *self {
            FlowError::Flushing => gst::FlowReturn::Flushing,
            FlowError::Eos => gst::FlowReturn::Eos,
            FlowError::NotNegotiated(..) => gst::FlowReturn::NotNegotiated,
            FlowError::Error(..) => gst::FlowReturn::Error,
        }
    }
}

impl Display for FlowError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        match *self {
            FlowError::Flushing | FlowError::Eos => f.write_str(self.description()),
            FlowError::NotNegotiated(ref m) => f.write_fmt(format_args!(
                "{}: {} ({})",
                self.description(),
                m.message.as_ref().map_or("None", |s| s.as_str()),
                m.debug.as_ref().map_or("None", |s| s.as_str())
            )),
            FlowError::Error(ref m) => f.write_fmt(format_args!(
                "{}: {} ({})",
                self.description(),
                m.message.as_ref().map_or("None", |s| s.as_str()),
                m.debug.as_ref().map_or("None", |s| s.as_str())
            )),
        }
    }
}

impl Error for FlowError {
    fn description(&self) -> &str {
        match *self {
            FlowError::Flushing => "Flushing",
            FlowError::Eos => "Eos",
            FlowError::NotNegotiated(..) => "Not Negotiated",
            FlowError::Error(..) => "Error",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UriError {
    error: gst::URIError,
    message: String,
}

impl UriError {
    pub fn new(error: gst::URIError, message: String) -> UriError {
        UriError {
            error: error,
            message: message,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn error(&self) -> gst::URIError {
        self.error
    }

    pub fn into_error(self) -> glib::Error {
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

pub type UriValidator = Fn(&Url) -> Result<(), UriError> + Send + Sync + 'static;

#[macro_export]
macro_rules! panic_to_error(
    ($element:expr, $panicked:expr, $ret:expr, $code:block) => {{
        use std::panic::{self, AssertUnwindSafe};
        use std::sync::atomic::Ordering;
        use $crate::error::ErrorMessage;

        if $panicked.load(Ordering::Relaxed) {
            error_msg!(gst::LibraryError::Failed, ["Panicked"]).post($element);
            $ret
        } else {
            let result = panic::catch_unwind(AssertUnwindSafe(|| $code));

            match result {
                Ok(result) => result,
                Err(err) => {
                    $panicked.store(true, Ordering::Relaxed);
                    if let Some(cause) = err.downcast_ref::<&str>() {
                        error_msg!(gst::LibraryError::Failed, ["Panicked: {}", cause]).post($element);
                    } else if let Some(cause) = err.downcast_ref::<String>() {
                        error_msg!(gst::LibraryError::Failed, ["Panicked: {}", cause]).post($element);
                    } else {
                        error_msg!(gst::LibraryError::Failed, ["Panicked"]).post($element);
                    }
                    $ret
                }
            }
        }
    }};
);
