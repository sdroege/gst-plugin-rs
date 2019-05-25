// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//               2018 François Laignel <fengalin@free.fr>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

use url::Url;

use file_location::FileLocation;

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

static PROPERTIES: [subclass::Property; 1] = [subclass::Property("location", |name| {
    glib::ParamSpec::string(
        name,
        "File Location",
        "Location of the file to read from",
        None,
        glib::ParamFlags::READWRITE,
    )
})];

enum State {
    Stopped,
    Started { file: File, position: u64 },
}

impl Default for State {
    fn default() -> State {
        State::Stopped
    }
}

pub struct FileSrc {
    cat: gst::DebugCategory,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl FileSrc {
    fn set_location(
        &self,
        element: &gst_base::BaseSrc,
        location: Option<FileLocation>,
    ) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            return Err(gst::Error::new(
                gst::URIError::BadState,
                "Changing the `location` property on a started `filesrc` is not supported",
            ));
        }

        let mut settings = self.settings.lock().unwrap();
        settings.location = match location {
            Some(location) => {
                if !location.exists() {
                    return Err(gst::Error::new(
                        gst::URIError::BadReference,
                        format!("{} doesn't exist", location).as_str(),
                    ));
                }

                if !location.is_file() {
                    return Err(gst::Error::new(
                        gst::URIError::BadReference,
                        format!("{} is not a file", location).as_str(),
                    ));
                }

                match settings.location {
                    Some(ref location_cur) => {
                        gst_info!(
                            self.cat,
                            obj: element,
                            "Changing `location` from {:?} to {}",
                            location_cur,
                            location,
                        );
                    }
                    None => {
                        gst_info!(self.cat, obj: element, "Setting `location to {}", location,);
                    }
                }
                Some(location)
            }
            None => {
                gst_info!(self.cat, obj: element, "Resetting `location` to None",);
                None
            }
        };

        Ok(())
    }
}

impl ObjectSubclass for FileSrc {
    const NAME: &'static str = "RsFileSrc";
    type ParentType = gst_base::BaseSrc;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            cat: gst::DebugCategory::new(
                "rsfilesrc",
                gst::DebugColorFlags::empty(),
                Some("File Source"),
            ),
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
        }
    }

    fn type_init(type_: &mut subclass::InitializingType<Self>) {
        type_.add_interface::<gst::URIHandler>();
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "File Source",
            "Source/File",
            "Read stream from a file",
            "François Laignel <fengalin@free.fr>, Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for FileSrc {
    glib_object_impl!();

    fn set_property(&self, obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let element = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();

                let res = match value.get::<String>() {
                    Some(location) => FileLocation::try_from_path_str(location)
                        .and_then(|file_location| self.set_location(&element, Some(file_location))),
                    None => self.set_location(&element, None),
                };

                if let Err(err) = res {
                    gst_error!(
                        self.cat,
                        obj: element,
                        "Failed to set property `location`: {}",
                        err
                    );
                }
            }
            _ => unimplemented!(),
        };
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let settings = self.settings.lock().unwrap();
                let location = settings
                    .location
                    .as_ref()
                    .map(|location| location.to_string());

                Ok(location.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst_base::BaseSrc>().unwrap();
        element.set_format(gst::Format::Bytes);
    }
}

impl ElementImpl for FileSrc {}

impl BaseSrcImpl for FileSrc {
    fn is_seekable(&self, _src: &gst_base::BaseSrc) -> bool {
        true
    }

    fn get_size(&self, _src: &gst_base::BaseSrc) -> Option<u64> {
        let state = self.state.lock().unwrap();
        if let State::Started { ref file, .. } = *state {
            file.metadata().ok().map(|m| m.len())
        } else {
            None
        }
    }

    fn start(&self, element: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            unreachable!("FileSrc already started");
        }

        let settings = self.settings.lock().unwrap();
        let location = settings.location.as_ref().ok_or_else(|| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["File location is not defined"]
            )
        })?;

        let file = File::open(location).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenRead,
                [
                    "Could not open file {} for reading: {}",
                    location,
                    err.to_string(),
                ]
            )
        })?;

        gst_debug!(self.cat, obj: element, "Opened file {:?}", file);

        *state = State::Started { file, position: 0 };

        gst_info!(self.cat, obj: element, "Started");

        Ok(())
    }

    fn stop(&self, element: &gst_base::BaseSrc) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if let State::Stopped = *state {
            return Err(gst_error_msg!(
                gst::ResourceError::Settings,
                ["FileSrc not started"]
            ));
        }

        *state = State::Stopped;

        gst_info!(self.cat, obj: element, "Stopped");

        Ok(())
    }

    fn fill(
        &self,
        element: &gst_base::BaseSrc,
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
                gst_element_error!(element, gst::CoreError::Failed, ["Not started yet"]);
                return Err(gst::FlowError::Error);
            }
        };

        if *position != offset {
            file.seek(SeekFrom::Start(offset)).map_err(|err| {
                gst_element_error!(
                    element,
                    gst::LibraryError::Failed,
                    ["Failed to seek to {}: {}", offset, err.to_string()]
                );
                gst::FlowError::Error
            })?;

            *position = offset;
        }

        let size = {
            let mut map = buffer.map_writable().ok_or_else(|| {
                gst_element_error!(element, gst::LibraryError::Failed, ["Failed to map buffer"]);
                gst::FlowError::Error
            })?;

            file.read(map.as_mut()).map_err(|err| {
                gst_element_error!(
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
    fn get_uri(&self, _element: &gst::URIHandler) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        // Conversion to Url already checked while building the `FileLocation`
        settings.location.as_ref().map(|location| {
            Url::from_file_path(location)
                .expect("FileSrc::get_uri couldn't build `Url` from `location`")
                .into_string()
        })
    }

    fn set_uri(&self, element: &gst::URIHandler, uri: &str) -> Result<(), glib::Error> {
        let element = element.dynamic_cast_ref::<gst_base::BaseSrc>().unwrap();

        // Special case for "file://" as this is used by some applications to test
        // with `gst_element_make_from_uri` if there's an element that supports the URI protocol

        if uri != "file://" {
            let file_location = FileLocation::try_from_uri_str(uri)?;
            self.set_location(&element, Some(file_location))
        } else {
            Ok(())
        }
    }

    fn get_uri_type() -> gst::URIType {
        gst::URIType::Src
    }

    fn get_protocols() -> Vec<String> {
        vec!["file".to_string()]
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(Some(plugin), "rsfilesrc", 256 + 100, FileSrc::get_type())
}
