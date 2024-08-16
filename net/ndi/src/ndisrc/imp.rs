// SPDX-License-Identifier: MPL-2.0

use atomic_refcell::AtomicRefCell;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::prelude::*;

use std::cmp;
use std::collections::VecDeque;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::ndisrcmeta::NdiSrcMeta;
use crate::ndisys;
use crate::RecvColorFormat;
use crate::TimestampMode;

use super::receiver::{Receiver, ReceiverControlHandle, ReceiverItem};
use crate::ndisrcmeta::Buffer;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ndisrc",
        gst::DebugColorFlags::empty(),
        Some("NewTek NDI Source"),
    )
});

static DEFAULT_RECEIVER_NDI_NAME: Lazy<String> = Lazy::new(|| {
    format!(
        "GStreamer NewTek NDI Source {}-{}",
        env!("CARGO_PKG_VERSION"),
        env!("COMMIT_ID")
    )
});

#[derive(Debug, Clone)]
struct Settings {
    ndi_name: Option<String>,
    url_address: Option<String>,
    connect_timeout: u32,
    timeout: u32,
    max_queue_length: u32,
    receiver_ndi_name: String,
    bandwidth: ndisys::NDIlib_recv_bandwidth_e,
    color_format: RecvColorFormat,
    timestamp_mode: TimestampMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            ndi_name: None,
            url_address: None,
            receiver_ndi_name: DEFAULT_RECEIVER_NDI_NAME.clone(),
            connect_timeout: 10000,
            timeout: 5000,
            max_queue_length: 10,
            bandwidth: ndisys::NDIlib_recv_bandwidth_highest,
            color_format: RecvColorFormat::UyvyBgra,
            timestamp_mode: TimestampMode::Auto,
        }
    }
}

#[derive(Default)]
struct State {
    receiver: Option<Receiver>,
    // Audio/video/metadata time observations
    timestamp_mode: TimestampMode,
    observations_timestamp: [Observations; 3],
    observations_timecode: [Observations; 3],
    current_latency: Option<gst::ClockTime>,
}

pub struct NdiSrc {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    receiver_controller: Mutex<Option<ReceiverControlHandle>>,
}

#[glib::object_subclass]
impl ObjectSubclass for NdiSrc {
    const NAME: &'static str = "GstNdiSrc";
    type Type = super::NdiSrc;
    type ParentType = gst_base::BaseSrc;

    fn new() -> Self {
        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
            receiver_controller: Mutex::new(None),
        }
    }
}

impl ObjectImpl for NdiSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            let receiver = glib::ParamSpecString::builder("receiver-ndi-name")
                .nick("Receiver NDI Name")
                .blurb("NDI stream name of this receiver");
            #[cfg(feature = "doc")]
            let receiver = receiver.doc_show_default();

            vec![
                glib::ParamSpecString::builder("ndi-name")
                    .nick("NDI Name")
                    .blurb("NDI stream name of the sender")
                    .build(),
                glib::ParamSpecString::builder("url-address")
                    .nick("URL/Address")
                    .blurb("URL/address and port of the sender, e.g. 127.0.0.1:5961")
                    .build(),
                receiver.build(),
                glib::ParamSpecUInt::builder("connect-timeout")
                    .nick("Connect Timeout")
                    .blurb("Connection timeout in ms")
                    .default_value(10000)
                    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Receive timeout in ms")
                    .default_value(5000)
                    .build(),
                glib::ParamSpecUInt::builder("max-queue-length")
                    .nick("Max Queue Length")
                    .blurb("Maximum receive queue length")
                    .default_value(10)
                    .build(),
                glib::ParamSpecInt::builder("bandwidth")
                    .nick("Bandwidth")
                    .blurb("Bandwidth, -10 metadata-only, 10 audio-only, 100 highest")
                    .minimum(-10)
                    .maximum(100)
                    .default_value(100)
                    .build(),
                glib::ParamSpecEnum::builder_with_default(
                    "color-format",
                    RecvColorFormat::UyvyBgra,
                )
                .nick("Color Format")
                .blurb("Receive color format")
                .build(),
                glib::ParamSpecEnum::builder_with_default(
                    "timestamp-mode",
                    TimestampMode::ReceiveTimeTimecode,
                )
                .nick("Timestamp Mode")
                .blurb("Timestamp information to use for outgoing PTS")
                .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        // Initialize live-ness and notify the base class that
        // we'd like to operate in Time format
        let obj = self.obj();
        obj.set_live(true);
        obj.set_format(gst::Format::Time);
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "ndi-name" => {
                let mut settings = self.settings.lock().unwrap();
                let ndi_name = value.get().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Changing ndi-name from {:?} to {:?}",
                    settings.ndi_name,
                    ndi_name,
                );
                settings.ndi_name = ndi_name;
            }
            "url-address" => {
                let mut settings = self.settings.lock().unwrap();
                let url_address = value.get().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Changing url-address from {:?} to {:?}",
                    settings.url_address,
                    url_address,
                );
                settings.url_address = url_address;
            }
            "receiver-ndi-name" => {
                let mut settings = self.settings.lock().unwrap();
                let receiver_ndi_name = value.get::<Option<String>>().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Changing receiver-ndi-name from {:?} to {:?}",
                    settings.receiver_ndi_name,
                    receiver_ndi_name,
                );
                settings.receiver_ndi_name =
                    receiver_ndi_name.unwrap_or_else(|| DEFAULT_RECEIVER_NDI_NAME.clone());
            }
            "connect-timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let connect_timeout = value.get().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Changing connect-timeout from {} to {}",
                    settings.connect_timeout,
                    connect_timeout,
                );
                settings.connect_timeout = connect_timeout;
            }
            "timeout" => {
                let mut settings = self.settings.lock().unwrap();
                let timeout = value.get().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Changing timeout from {} to {}",
                    settings.timeout,
                    timeout,
                );
                settings.timeout = timeout;
            }
            "max-queue-length" => {
                let mut settings = self.settings.lock().unwrap();
                let max_queue_length = value.get().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Changing max-queue-length from {} to {}",
                    settings.max_queue_length,
                    max_queue_length,
                );
                settings.max_queue_length = max_queue_length;
            }
            "bandwidth" => {
                let mut settings = self.settings.lock().unwrap();
                let bandwidth = value.get().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Changing bandwidth from {} to {}",
                    settings.bandwidth,
                    bandwidth,
                );
                settings.bandwidth = bandwidth;
            }
            "color-format" => {
                let mut settings = self.settings.lock().unwrap();
                let color_format = value.get().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Changing color format from {:?} to {:?}",
                    settings.color_format,
                    color_format,
                );
                settings.color_format = color_format;
            }
            "timestamp-mode" => {
                let mut settings = self.settings.lock().unwrap();
                let timestamp_mode = value.get().unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Changing timestamp mode from {:?} to {:?}",
                    settings.timestamp_mode,
                    timestamp_mode
                );
                if settings.timestamp_mode != timestamp_mode {
                    let _ = self
                        .obj()
                        .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
                }
                settings.timestamp_mode = timestamp_mode;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "ndi-name" => {
                let settings = self.settings.lock().unwrap();
                settings.ndi_name.to_value()
            }
            "url-address" => {
                let settings = self.settings.lock().unwrap();
                settings.url_address.to_value()
            }
            "receiver-ndi-name" => {
                let settings = self.settings.lock().unwrap();
                settings.receiver_ndi_name.to_value()
            }
            "connect-timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.connect_timeout.to_value()
            }
            "timeout" => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            "max-queue-length" => {
                let settings = self.settings.lock().unwrap();
                settings.max_queue_length.to_value()
            }
            "bandwidth" => {
                let settings = self.settings.lock().unwrap();
                settings.bandwidth.to_value()
            }
            "color-format" => {
                let settings = self.settings.lock().unwrap();
                settings.color_format.to_value()
            }
            "timestamp-mode" => {
                let settings = self.settings.lock().unwrap();
                settings.timestamp_mode.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for NdiSrc {}

impl ElementImpl for NdiSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
            "NewTek NDI Source",
            "Source/Audio/Video/Network",
            "NewTek NDI Source",
            "Ruben Gonzalez <rubenrua@teltek.es>, Daniel Vilar <daniel.peiteado@teltek.es>, Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("application/x-ndi").build(),
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    #[allow(clippy::single_match)]
    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        match transition {
            gst::StateChange::NullToReady => {
                if let Err(err) = crate::ndi::load() {
                    gst::element_imp_error!(self, gst::LibraryError::Init, ("{}", err));
                    return Err(gst::StateChangeError);
                }
            }
            gst::StateChange::PausedToPlaying => {
                if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
                    controller.set_playing(true);
                }
            }
            gst::StateChange::PlayingToPaused => {
                if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
                    controller.set_playing(false);
                }
            }
            gst::StateChange::PausedToReady => {
                if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
                    controller.shutdown();
                }
            }
            gst::StateChange::ReadyToPaused => {
                *self.state.lock().unwrap() = Default::default();
                let settings = self.settings.lock().unwrap().clone();

                if settings.ndi_name.is_none() && settings.url_address.is_none() {
                    gst::element_imp_error!(
                        self,
                        gst::LibraryError::Settings,
                        ["No NDI name or URL/address given"]
                    );

                    return Err(gst::StateChangeError);
                }

                let receiver = Receiver::connect(
                    self.obj().upcast_ref(),
                    settings.ndi_name.as_deref(),
                    settings.url_address.as_deref(),
                    &settings.receiver_ndi_name,
                    settings.connect_timeout,
                    settings.bandwidth,
                    settings.color_format.into(),
                    settings.timeout,
                    settings.max_queue_length as usize,
                );

                match receiver {
                    None => {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::NotFound,
                            ["Could not connect to this source"]
                        );

                        return Err(gst::StateChangeError);
                    }
                    Some(receiver) => {
                        *self.receiver_controller.lock().unwrap() =
                            Some(receiver.receiver_control_handle());
                        let mut state = self.state.lock().unwrap();
                        state.receiver = Some(receiver);
                        state.timestamp_mode = settings.timestamp_mode;
                    }
                }
            }

            _ => (),
        }

        let res = self.parent_change_state(transition)?;

        match transition {
            gst::StateChange::PausedToReady => {
                *self.receiver_controller.lock().unwrap() = None;
                *self.state.lock().unwrap() = State::default();
            }
            _ => (),
        }

        Ok(res)
    }
}

impl BaseSrcImpl for NdiSrc {
    fn negotiate(&self) -> Result<(), gst::LoggableError> {
        self.obj()
            .set_caps(&gst::Caps::builder("application/x-ndi").build())
            .map_err(|_| gst::loggable_error!(CAT, "Failed to negotiate caps",))
    }

    fn unlock(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Unlocking",);
        if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
            controller.set_flushing(true);
        }
        Ok(())
    }

    fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Stop unlocking",);
        if let Some(ref controller) = *self.receiver_controller.lock().unwrap() {
            controller.set_flushing(false);
        }
        Ok(())
    }

    fn query(&self, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryViewMut::Latency(ref mut q) => {
                let state = self.state.lock().unwrap();
                let settings = self.settings.lock().unwrap();

                if let Some(latency) = state.current_latency {
                    let min = if matches!(
                        settings.timestamp_mode,
                        TimestampMode::Auto
                            | TimestampMode::ReceiveTimeTimecode
                            | TimestampMode::ReceiveTimeTimestamp
                    ) {
                        latency
                    } else {
                        gst::ClockTime::ZERO
                    };

                    let max = settings.max_queue_length as u64 * latency;

                    gst::debug!(CAT, imp = self, "Returning latency min {} max {}", min, max);
                    q.set(true, min, max);
                    true
                } else {
                    false
                }
            }
            _ => BaseSrcImplExt::parent_query(self, query),
        }
    }

    fn create(
        &self,
        _offset: u64,
        _buffer: Option<&mut gst::BufferRef>,
        _length: u32,
    ) -> Result<CreateSuccess, gst::FlowError> {
        let recv = {
            let mut state = self.state.lock().unwrap();
            match state.receiver.take() {
                Some(recv) => recv,
                None => {
                    gst::error!(CAT, imp = self, "Have no receiver");
                    return Err(gst::FlowError::Error);
                }
            }
        };

        let res = recv.capture();

        let mut state = self.state.lock().unwrap();
        state.receiver = Some(recv);

        match res {
            ReceiverItem::Buffer(ndi_buffer) => {
                let mut latency_changed = false;

                if let Buffer::Video { ref frame, .. } = ndi_buffer {
                    let duration = gst::ClockTime::SECOND
                        .mul_div_floor(frame.frame_rate().1 as u64, frame.frame_rate().0 as u64);

                    latency_changed = state.current_latency != duration;
                    state.current_latency = duration;
                }

                let mut gst_buffer = gst::Buffer::new();
                {
                    let buffer_ref = gst_buffer.get_mut().unwrap();
                    let ((pts, duration, resync), discont) = match ndi_buffer {
                        Buffer::Audio {
                            ref frame,
                            discont,
                            receive_time_gst,
                            receive_time_real,
                        } => (
                            self.calculate_audio_timestamp(
                                &mut state,
                                receive_time_gst,
                                receive_time_real,
                                frame,
                            ),
                            discont,
                        ),
                        Buffer::Video {
                            ref frame,
                            discont,
                            receive_time_gst,
                            receive_time_real,
                        } => (
                            self.calculate_video_timestamp(
                                &mut state,
                                receive_time_gst,
                                receive_time_real,
                                frame,
                            ),
                            discont,
                        ),
                        Buffer::Metadata {
                            ref frame,
                            receive_time_gst,
                            receive_time_real,
                        } => (
                            self.calculate_metadata_timestamp(
                                &mut state,
                                receive_time_gst,
                                receive_time_real,
                                frame,
                            ),
                            false,
                        ),
                    };
                    buffer_ref.set_pts(pts);
                    buffer_ref.set_duration(duration);
                    if resync {
                        buffer_ref.set_flags(gst::BufferFlags::RESYNC);
                    }
                    if discont {
                        buffer_ref.set_flags(gst::BufferFlags::DISCONT);
                    }
                    NdiSrcMeta::add(buffer_ref, ndi_buffer);
                }

                drop(state);

                if latency_changed {
                    let _ = self
                        .obj()
                        .post_message(gst::message::Latency::builder().src(&*self.obj()).build());
                }

                Ok(CreateSuccess::NewBuffer(gst_buffer))
            }
            ReceiverItem::Timeout => Err(gst::FlowError::Eos),
            ReceiverItem::Flushing => Err(gst::FlowError::Flushing),
            ReceiverItem::Error(err) => Err(err),
        }
    }
}

impl NdiSrc {
    #[allow(clippy::too_many_arguments)]
    fn calculate_timestamp(
        &self,
        state: &mut State,
        idx: usize,
        receive_time_gst: gst::ClockTime,
        receive_time_real: gst::ClockTime,
        timestamp: i64,
        timecode: i64,
        duration: Option<gst::ClockTime>,
    ) -> (gst::ClockTime, Option<gst::ClockTime>, bool) {
        let timestamp = if timestamp == ndisys::NDIlib_recv_timestamp_undefined {
            gst::ClockTime::NONE
        } else {
            Some((timestamp as u64 * 100).nseconds())
        };
        let timecode = (timecode as u64 * 100).nseconds();

        gst::log!(
            CAT,
            imp = self,
            "Received frame of type {idx} with timecode {timecode}, timestamp {}, duration {}, receive time {}, local time now {}",
            timestamp.display(),
            duration.display(),
            receive_time_gst.display(),
            receive_time_real,
        );

        let res_timestamp = state.observations_timestamp[idx].process(
            self.obj().upcast_ref(),
            timestamp,
            receive_time_gst,
            duration,
        );

        let res_timecode = state.observations_timecode[idx].process(
            self.obj().upcast_ref(),
            Some(timecode),
            receive_time_gst,
            duration,
        );

        let (pts, duration, discont) = match state.timestamp_mode {
            TimestampMode::ReceiveTimeTimecode => match res_timecode {
                Some((pts, duration, discont)) => (pts, duration, discont),
                None => {
                    gst::warning!(CAT, imp = self, "Can't calculate timestamp");
                    (receive_time_gst, duration, false)
                }
            },
            TimestampMode::ReceiveTimeTimestamp => match res_timestamp {
                Some((pts, duration, discont)) => (pts, duration, discont),
                None => {
                    if timestamp.is_some() {
                        gst::warning!(CAT, imp = self, "Can't calculate timestamp");
                    }

                    (receive_time_gst, duration, false)
                }
            },
            TimestampMode::Timecode => (timecode, duration, false),
            TimestampMode::Timestamp if timestamp.is_none() => (receive_time_gst, duration, false),
            TimestampMode::Timestamp => {
                // Timestamps are relative to the UNIX epoch
                let timestamp = timestamp.unwrap();
                if receive_time_real > timestamp {
                    let diff = receive_time_real - timestamp;
                    if diff > receive_time_gst {
                        (gst::ClockTime::ZERO, duration, false)
                    } else {
                        (receive_time_gst - diff, duration, false)
                    }
                } else {
                    let diff = timestamp - receive_time_real;
                    (receive_time_gst + diff, duration, false)
                }
            }
            TimestampMode::ReceiveTime => (receive_time_gst, duration, false),
            TimestampMode::Auto => {
                res_timecode
                    .or(res_timestamp)
                    .unwrap_or((receive_time_gst, duration, false))
            }
        };

        gst::log!(
            CAT,
            imp = self,
            "Calculated PTS {}, duration {}",
            pts.display(),
            duration.display(),
        );

        (pts, duration, discont)
    }

    fn calculate_video_timestamp(
        &self,
        state: &mut State,
        receive_time_gst: gst::ClockTime,
        receive_time_real: gst::ClockTime,
        video_frame: &crate::ndi::VideoFrame,
    ) -> (gst::ClockTime, Option<gst::ClockTime>, bool) {
        let duration = gst::ClockTime::SECOND.mul_div_floor(
            video_frame.frame_rate().1 as u64,
            video_frame.frame_rate().0 as u64,
        );

        self.calculate_timestamp(
            state,
            0,
            receive_time_gst,
            receive_time_real,
            video_frame.timestamp(),
            video_frame.timecode(),
            duration,
        )
    }

    fn calculate_audio_timestamp(
        &self,
        state: &mut State,
        receive_time_gst: gst::ClockTime,
        receive_time_real: gst::ClockTime,
        audio_frame: &crate::ndi::AudioFrame,
    ) -> (gst::ClockTime, Option<gst::ClockTime>, bool) {
        let duration = gst::ClockTime::SECOND.mul_div_floor(
            audio_frame.no_samples() as u64,
            audio_frame.sample_rate() as u64,
        );

        self.calculate_timestamp(
            state,
            1,
            receive_time_gst,
            receive_time_real,
            audio_frame.timestamp(),
            audio_frame.timecode(),
            duration,
        )
    }

    fn calculate_metadata_timestamp(
        &self,
        state: &mut State,
        receive_time_gst: gst::ClockTime,
        receive_time_real: gst::ClockTime,
        metadata_frame: &crate::ndi::MetadataFrame,
    ) -> (gst::ClockTime, Option<gst::ClockTime>, bool) {
        self.calculate_timestamp(
            state,
            2,
            receive_time_gst,
            receive_time_real,
            ndisys::NDIlib_recv_timestamp_undefined,
            metadata_frame.timecode(),
            gst::ClockTime::NONE,
        )
    }
}

const PREFILL_WINDOW_LENGTH: usize = 12;
const WINDOW_LENGTH: u64 = 512;
const WINDOW_DURATION: u64 = 2_000_000_000;

#[derive(Default)]
struct Observations(AtomicRefCell<ObservationsInner>);

struct ObservationsInner {
    base_remote_time: Option<u64>,
    base_local_time: Option<u64>,
    deltas: VecDeque<i64>,
    min_delta: i64,
    skew: i64,
    filling: bool,
    window_size: usize,

    // Remote/local times for workaround around fundamentally wrong slopes
    // This is not reset below and has a bigger window.
    times: VecDeque<(u64, u64)>,
    slope_correction: (u64, u64),
}

impl Default for ObservationsInner {
    fn default() -> ObservationsInner {
        ObservationsInner {
            base_local_time: None,
            base_remote_time: None,
            deltas: VecDeque::new(),
            min_delta: 0,
            skew: 0,
            filling: true,
            window_size: 0,
            times: VecDeque::new(),
            slope_correction: (1, 1),
        }
    }
}

impl ObservationsInner {
    fn reset(&mut self) {
        self.base_local_time = None;
        self.base_remote_time = None;
        self.deltas = VecDeque::new();
        self.min_delta = 0;
        self.skew = 0;
        self.filling = true;
        self.window_size = 0;
    }
}

impl Observations {
    // Based on the algorithm used in GStreamer's rtpjitterbuffer, which comes from
    // Fober, Orlarey and Letz, 2005, "Real Time Clock Skew Estimation over Network Delays":
    // http://citeseerx.ist.psu.edu/viewdoc/summary?doi=10.1.1.102.1546
    fn process(
        &self,
        element: &gst::Element,
        remote_time: Option<gst::ClockTime>,
        local_time: gst::ClockTime,
        duration: Option<gst::ClockTime>,
    ) -> Option<(gst::ClockTime, Option<gst::ClockTime>, bool)> {
        let remote_time = remote_time?.nseconds();
        let local_time = local_time.nseconds();

        let mut inner = self.0.borrow_mut();

        gst::trace!(
            CAT,
            obj = element,
            "Local time {}, remote time {}, slope correct {}/{}",
            local_time.nseconds(),
            remote_time.nseconds(),
            inner.slope_correction.0,
            inner.slope_correction.1,
        );

        inner.times.push_back((remote_time, local_time));
        while inner
            .times
            .back()
            .unwrap()
            .1
            .saturating_sub(inner.times.front().unwrap().1)
            > WINDOW_DURATION
        {
            let _ = inner.times.pop_front();
        }

        // Static remote times
        if inner.slope_correction.1 == 0 {
            return None;
        }

        let remote_time =
            remote_time.mul_div_round(inner.slope_correction.0, inner.slope_correction.1)?;

        let (base_remote_time, base_local_time) =
            match (inner.base_remote_time, inner.base_local_time) {
                (Some(remote), Some(local)) => (remote, local),
                _ => {
                    gst::debug!(
                        CAT,
                        obj = element,
                        "Initializing base time: local {}, remote {}",
                        local_time.nseconds(),
                        remote_time.nseconds(),
                    );
                    inner.base_remote_time = Some(remote_time);
                    inner.base_local_time = Some(local_time);

                    return Some((local_time.nseconds(), duration, true));
                }
            };

        if inner.times.len() < PREFILL_WINDOW_LENGTH {
            return Some((local_time.nseconds(), duration, false));
        }

        // Check if the slope is simply wrong and try correcting
        {
            let local_diff = inner
                .times
                .back()
                .unwrap()
                .1
                .saturating_sub(inner.times.front().unwrap().1);
            let remote_diff = inner
                .times
                .back()
                .unwrap()
                .0
                .saturating_sub(inner.times.front().unwrap().0);

            if remote_diff == 0 {
                inner.reset();
                inner.base_remote_time = Some(remote_time);
                inner.base_local_time = Some(local_time);

                // Static remote times
                inner.slope_correction = (0, 0);
                return None;
            } else {
                let slope = local_diff as f64 / remote_diff as f64;
                let scaled_slope =
                    slope * (inner.slope_correction.1 as f64) / (inner.slope_correction.0 as f64);

                // Check for some obviously wrong slopes and try to correct for that
                if !(0.5..1.5).contains(&scaled_slope) {
                    gst::warning!(
                        CAT,
                        obj = element,
                        "Too small/big slope {}, resetting",
                        scaled_slope
                    );

                    let discont = !inner.deltas.is_empty();
                    inner.reset();

                    if (0.0005..0.0015).contains(&slope) {
                        // Remote unit was actually 0.1ns
                        inner.slope_correction = (1, 1000);
                    } else if (0.005..0.015).contains(&slope) {
                        // Remote unit was actually 1ns
                        inner.slope_correction = (1, 100);
                    } else if (0.05..0.15).contains(&slope) {
                        // Remote unit was actually 10ns
                        inner.slope_correction = (1, 10);
                    } else if (5.0..15.0).contains(&slope) {
                        // Remote unit was actually 1us
                        inner.slope_correction = (10, 1);
                    } else if (50.0..150.0).contains(&slope) {
                        // Remote unit was actually 10us
                        inner.slope_correction = (100, 1);
                    } else if (50.0..150.0).contains(&slope) {
                        // Remote unit was actually 100us
                        inner.slope_correction = (1000, 1);
                    } else if (50.0..150.0).contains(&slope) {
                        // Remote unit was actually 1ms
                        inner.slope_correction = (10000, 1);
                    } else {
                        inner.slope_correction = (1, 1);
                    }

                    let remote_time = inner
                        .times
                        .back()
                        .unwrap()
                        .0
                        .mul_div_round(inner.slope_correction.0, inner.slope_correction.1)?;
                    gst::debug!(
                        CAT,
                        obj = element,
                        "Initializing base time: local {}, remote {}, slope correction {}/{}",
                        local_time.nseconds(),
                        remote_time.nseconds(),
                        inner.slope_correction.0,
                        inner.slope_correction.1,
                    );
                    inner.base_remote_time = Some(remote_time);
                    inner.base_local_time = Some(local_time);

                    return Some((local_time.nseconds(), duration, discont));
                }
            }
        }

        let remote_diff = remote_time.saturating_sub(base_remote_time);
        let local_diff = local_time.saturating_sub(base_local_time);
        let delta = (local_diff as i64) - (remote_diff as i64);

        gst::trace!(
            CAT,
            obj = element,
            "Local diff {}, remote diff {}, delta {}",
            local_diff.nseconds(),
            remote_diff.nseconds(),
            delta,
        );

        if (delta > inner.skew && delta - inner.skew > 1_000_000_000)
            || (delta < inner.skew && inner.skew - delta > 1_000_000_000)
        {
            gst::warning!(
                CAT,
                obj = element,
                "Delta {} too far from skew {}, resetting",
                delta,
                inner.skew
            );

            let discont = !inner.deltas.is_empty();

            gst::debug!(
                CAT,
                obj = element,
                "Initializing base time: local {}, remote {}",
                local_time.nseconds(),
                remote_time.nseconds(),
            );

            inner.reset();
            inner.base_remote_time = Some(remote_time);
            inner.base_local_time = Some(local_time);

            return Some((local_time.nseconds(), duration, discont));
        }

        if inner.filling {
            if inner.deltas.is_empty() || delta < inner.min_delta {
                inner.min_delta = delta;
            }
            inner.deltas.push_back(delta);

            if remote_diff > WINDOW_DURATION || inner.deltas.len() as u64 == WINDOW_LENGTH {
                inner.window_size = inner.deltas.len();
                inner.skew = inner.min_delta;
                inner.filling = false;
            } else {
                let perc_time = remote_diff.mul_div_floor(100, WINDOW_DURATION).unwrap() as i64;
                let perc_window = (inner.deltas.len() as u64)
                    .mul_div_floor(100, WINDOW_LENGTH)
                    .unwrap() as i64;
                let perc = cmp::max(perc_time, perc_window);

                inner.skew = (perc * inner.min_delta + ((10_000 - perc) * inner.skew)) / 10_000;
            }
        } else {
            let old = inner.deltas.pop_front().unwrap();
            inner.deltas.push_back(delta);

            if delta <= inner.min_delta {
                inner.min_delta = delta;
            } else if old == inner.min_delta {
                inner.min_delta = inner.deltas.iter().copied().min().unwrap();
            }

            inner.skew = (inner.min_delta + (124 * inner.skew)) / 125;
        }

        let out_time = base_local_time + remote_diff;
        let out_time = if inner.skew < 0 {
            out_time.saturating_sub((-inner.skew) as u64)
        } else {
            out_time + (inner.skew as u64)
        };

        gst::trace!(
            CAT,
            obj = element,
            "Skew {}, min delta {}",
            inner.skew,
            inner.min_delta
        );
        gst::trace!(CAT, obj = element, "Outputting {}", out_time.nseconds());

        Some((out_time.nseconds(), duration, false))
    }
}
