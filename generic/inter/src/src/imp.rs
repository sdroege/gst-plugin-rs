// SPDX-License-Identifier: MPL-2.0

use crate::streamproducer::InterStreamProducer;
use anyhow::Error;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::Mutex;

use std::sync::LazyLock;

const DEFAULT_PRODUCER_NAME: &str = "default";

#[derive(Debug)]
struct Settings {
    producer_name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            producer_name: DEFAULT_PRODUCER_NAME.to_string(),
        }
    }
}

struct State {
    srcpad: gst::GhostPad,
    appsrc: gst_app::AppSrc,
}

/* Locking order is field order */
pub struct InterSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl InterSrc {
    fn prepare(&self) -> Result<(), Error> {
        let settings = self.settings.lock().unwrap();
        let state = self.state.lock().unwrap();

        InterStreamProducer::subscribe(&settings.producer_name, &state.appsrc);

        Ok(())
    }

    fn unprepare(&self) {
        let settings = self.settings.lock().unwrap();
        let state = self.state.lock().unwrap();

        InterStreamProducer::unsubscribe(&settings.producer_name, &state.appsrc);
    }
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new("intersrc", gst::DebugColorFlags::empty(), Some("Inter Src"))
});

#[glib::object_subclass]
impl ObjectSubclass for InterSrc {
    const NAME: &'static str = "GstInterSrc";

    type Type = super::InterSrc;
    type ParentType = gst::Bin;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::GhostPad::from_template(&templ);

        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(State {
                srcpad: srcpad.upcast(),
                appsrc: gst_app::AppSrc::builder().name("appsrc").build(),
            }),
        }
    }
}

impl ObjectImpl for InterSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![glib::ParamSpecString::builder("producer-name")
                .nick("Producer Name")
                .blurb("Producer Name to consume from")
                .doc_show_default()
                .mutable_playing()
                .build()]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "producer-name" => {
                let mut settings = self.settings.lock().unwrap();
                let old_producer_name = settings.producer_name.clone();
                settings.producer_name = value
                    .get::<String>()
                    .unwrap_or_else(|_| DEFAULT_PRODUCER_NAME.to_string());

                let state = self.state.lock().unwrap();

                if InterStreamProducer::unsubscribe(&old_producer_name, &state.appsrc) {
                    InterStreamProducer::subscribe(&settings.producer_name, &state.appsrc);
                }
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "producer-name" => {
                let settings = self.settings.lock().unwrap();
                settings.producer_name.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();

        obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
        obj.set_element_flags(gst::ElementFlags::SOURCE);

        let state = self.state.lock().unwrap();
        // The name of GstObjects can still be changed until they become child of another object.
        state
            .appsrc
            .set_property("name", format!("{}-appsrc", self.obj().name()));
        gst_utils::StreamProducer::configure_consumer(&state.appsrc);
        obj.add(&state.appsrc).unwrap();
        obj.add_pad(&state.srcpad).unwrap();
        state
            .srcpad
            .set_target(Some(&state.appsrc.static_pad("src").unwrap()))
            .unwrap();
    }
}

impl GstObjectImpl for InterSrc {}

impl ElementImpl for InterSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Inter Src",
                "Generic/Src",
                "Inter Src",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
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

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        if transition == gst::StateChange::ReadyToPaused {
            if let Err(err) = self.prepare() {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Failed,
                    ["Failed to prepare: {}", err]
                );
                return Err(gst::StateChangeError);
            }
        }

        let ret = self.parent_change_state(transition)?;

        if transition == gst::StateChange::PausedToReady {
            self.unprepare();
        }

        Ok(ret)
    }
}

impl BinImpl for InterSrc {}
