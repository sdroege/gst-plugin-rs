// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2018 François Laignel <fengalin@free.fr>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_info};
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

use url::Url;

use crate::file_location::FileLocation;

const DEFAULT_LOCATION: Option<FileLocation> = None;

#[derive(Debug)]
struct Settings {
    location: Option<FileLocation>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            location: DEFAULT_LOCATION,
        }
    }
}

enum State {
    Stopped,
    Started { file: File, position: u64 },
}

impl Default for State {
    fn default() -> State {
        State::Stopped
    }
}

#[derive(Default)]
pub struct FileSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

use once_cell::sync::Lazy;
static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rsfilesrc",
        gst::DebugColorFlags::empty(),
        Some("File Source"),
    )
});

impl FileSrc {
    fn set_location(
        &self,
        element: &super::FileSrc,
        location: Option<FileLocation>,
    ) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Changing the `location` property on a started `filesrc` is not supported",
            ));
        }

        let mut settings = self.settings.lock().unwrap();
        settings.location = match location {
            Some(location) => {
                if !location.exists() {
                    return Err(glib::Error::new(
                        gst::URIError::BadReference,
                        format!("{} doesn't exist", location).as_str(),
                    ));
                }

                if !location.is_file() {
                    return Err(glib::Error::new(
                        gst::URIError::BadReference,
                        format!("{} is not a file", location).as_str(),
                    ));
                }

                match settings.location {
                    Some(ref location_cur) => {
                        gst_info!(
                            CAT,
                            obj: element,
                            "Changing `location` from {:?} to {}",
                            location_cur,
                            location,
                        );
                    }
                    None => {
                        gst_info!(CAT, obj: element, "Setting `location to {}", location,);
                    }
                }
                Some(location)
            }
            None => {
                gst_info!(CAT, obj: element, "Resetting `location` to None",);
                None
            }
        };

        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FileSrc {
    const NAME: &'static str = "RsFileSrc";
    type Type = super::FileSrc;
    type ParentType = gst_base::BaseSrc;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for FileSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpec::new_string(
                "location",
                "File Location",
                "Location of the file to read from",
                None,
                glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
            )]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "location" => {
                let res = match value.get::<Option<String>>() {
                    Ok(Some(location)) => FileLocation::try_from_path_str(location)
                        .and_then(|file_location| self.set_location(obj, Some(file_location))),
                    Ok(None) => self.set_location(obj, None),
                    Err(_) => unreachable!("type checked upstream"),
                };

                if let Err(err) = res {
                    gst_error!(CAT, obj: obj, "Failed to set property `location`: {}", err);
                }
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "location" => {
                let settings = self.settings.lock().unwrap();
                let location = settings
                    .location
                    .as_ref()
                    .map(|location| location.to_string());

                location.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.set_format(gst::Format::Bytes);
    }
}

impl ElementImpl for FileSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "File Source",
                "Source/File",
                "Read stream from a file",
                "François Laignel <fengalin@free.fr>, Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseSrcImpl for FileSrc {
    fn is_seekable(&self, _src: &Self::Type) -> bool {
        true
    }

    fn size(&self, _src: &Self::Type) -> Option<u64> {
        let state = self.state.lock().unwrap();
        if let State::Started { ref file, .. } = *state {
            file.metadata().ok().map(|m| m.len())
        } else {
            None
        }
    }

    fn start(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            unreachable!("FileSrc already started");
        }

        let settings = self.settings.lock().unwrap();
        let location = settings.location.as_ref().ok_or_else(|| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ["File location is not defined"]
            )
        })?;

        let file = File::open(location).map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                [
                    "Could not open file {} for reading: {}",
                    location,
                    err.to_string(),
                ]
            )
        })?;

        gst_debug!(CAT, obj: element, "Opened file {:?}", file);

        *state = State::Started { file, position: 0 };

        gst_info!(CAT, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if let State::Stopped = *state {
            return Err(gst::error_msg!(
                gst::ResourceError::Settings,
                ["FileSrc not started"]
            ));
        }

        *state = State::Stopped;

        gst_info!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn fill(
        &self,
        element: &Self::Type,
        offset: u64,
        _length: u32,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        let (file, position) = match *state {
            State::Started {
                ref mut file,
                ref mut position,
            } => (file, position),
            State::Stopped => {
                gst::element_error!(element, gst::CoreError::Failed, ["Not started yet"]);
                return Err(gst::FlowError::Error);
            }
        };

        if *position != offset {
            file.seek(SeekFrom::Start(offset)).map_err(|err| {
                gst::element_error!(
                    element,
                    gst::LibraryError::Failed,
                    ["Failed to seek to {}: {}", offset, err.to_string()]
                );
                gst::FlowError::Error
            })?;

            *position = offset;
        }

        let size = {
            let mut map = buffer.map_writable().map_err(|_| {
                gst::element_error!(element, gst::LibraryError::Failed, ["Failed to map buffer"]);
                gst::FlowError::Error
            })?;

            file.read(map.as_mut()).map_err(|err| {
                gst::element_error!(
                    element,
                    gst::LibraryError::Failed,
                    ["Failed to read at {}: {}", offset, err.to_string()]
                );
                gst::FlowError::Error
            })?
        };

        *position += size as u64;

        buffer.set_size(size);

        Ok(gst::FlowSuccess::Ok)
    }
}

impl URIHandlerImpl for FileSrc {
    const URI_TYPE: gst::URIType = gst::URIType::Src;

    fn protocols() -> &'static [&'static str] {
        &["file"]
    }

    fn uri(&self, _element: &Self::Type) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        // Conversion to Url already checked while building the `FileLocation`
        settings.location.as_ref().map(|location| {
            Url::from_file_path(location)
                .expect("FileSrc::get_uri couldn't build `Url` from `location`")
                .into()
        })
    }

    fn set_uri(&self, element: &Self::Type, uri: &str) -> Result<(), glib::Error> {
        // Special case for "file://" as this is used by some applications to test
        // with `gst_element_make_from_uri` if there's an element that supports the URI protocol

        if uri != "file://" {
            let file_location = FileLocation::try_from_uri_str(uri)?;
            self.set_location(&element, Some(file_location))
        } else {
            Ok(())
        }
    }
}
