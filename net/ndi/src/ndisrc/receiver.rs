// SPDX-License-Identifier: MPL-2.0

use glib::prelude::*;
use gst::prelude::*;
use gst_video::prelude::*;

use byte_slice_cast::*;

use std::cmp;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time;

use atomic_refcell::AtomicRefCell;

use gst::glib::once_cell::sync::Lazy;

use crate::ndi::*;
use crate::ndisys;
use crate::ndisys::*;
use crate::TimestampMode;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ndireceiver",
        gst::DebugColorFlags::empty(),
        Some("NewTek NDI receiver"),
    )
});

pub struct Receiver(Arc<ReceiverInner>);

#[derive(Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum AudioInfo {
    Audio(gst_audio::AudioInfo),
    #[cfg(feature = "advanced-sdk")]
    #[allow(dead_code)]
    Opus {
        sample_rate: i32,
        no_channels: i32,
    },
    #[cfg(feature = "advanced-sdk")]
    Aac {
        sample_rate: i32,
        no_channels: i32,
        codec_data: [u8; 2],
    },
}

impl AudioInfo {
    pub fn to_caps(&self) -> Result<gst::Caps, glib::BoolError> {
        match self {
            AudioInfo::Audio(ref info) => info.to_caps(),
            #[cfg(feature = "advanced-sdk")]
            AudioInfo::Opus {
                sample_rate,
                no_channels,
            } => Ok(gst::Caps::builder("audio/x-opus")
                .field("channels", *no_channels)
                .field("rate", *sample_rate)
                .field("channel-mapping-family", 0i32)
                .build()),
            #[cfg(feature = "advanced-sdk")]
            AudioInfo::Aac {
                sample_rate,
                no_channels,
                codec_data,
            } => Ok(gst::Caps::builder("audio/mpeg")
                .field("channels", *no_channels)
                .field("rate", *sample_rate)
                .field("mpegversion", 4i32)
                .field("stream-format", "raw")
                .field("codec_data", gst::Buffer::from_mut_slice(*codec_data))
                .build()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum VideoInfo {
    Video(gst_video::VideoInfo),
    #[cfg(feature = "advanced-sdk")]
    SpeedHQInfo {
        variant: String,
        xres: i32,
        yres: i32,
        fps_n: i32,
        fps_d: i32,
        par_n: i32,
        par_d: i32,
        interlace_mode: gst_video::VideoInterlaceMode,
    },
    #[cfg(feature = "advanced-sdk")]
    H264 {
        xres: i32,
        yres: i32,
        fps_n: i32,
        fps_d: i32,
        par_n: i32,
        par_d: i32,
        interlace_mode: gst_video::VideoInterlaceMode,
    },
    #[cfg(feature = "advanced-sdk")]
    H265 {
        xres: i32,
        yres: i32,
        fps_n: i32,
        fps_d: i32,
        par_n: i32,
        par_d: i32,
        interlace_mode: gst_video::VideoInterlaceMode,
    },
}

impl VideoInfo {
    pub fn to_caps(&self) -> Result<gst::Caps, glib::BoolError> {
        match self {
            VideoInfo::Video(ref info) => info.to_caps(),
            #[cfg(feature = "advanced-sdk")]
            VideoInfo::SpeedHQInfo {
                ref variant,
                xres,
                yres,
                fps_n,
                fps_d,
                par_n,
                par_d,
                interlace_mode,
            } => Ok(gst::Caps::builder("video/x-speedhq")
                .field("width", *xres)
                .field("height", *yres)
                .field("framerate", gst::Fraction::new(*fps_n, *fps_d))
                .field("pixel-aspect-ratio", gst::Fraction::new(*par_n, *par_d))
                .field("interlace-mode", interlace_mode.to_str())
                .field("variant", variant)
                .build()),
            #[cfg(feature = "advanced-sdk")]
            VideoInfo::H264 {
                xres,
                yres,
                fps_n,
                fps_d,
                par_n,
                par_d,
                interlace_mode,
                ..
            } => Ok(gst::Caps::builder("video/x-h264")
                .field("width", *xres)
                .field("height", *yres)
                .field("framerate", gst::Fraction::new(*fps_n, *fps_d))
                .field("pixel-aspect-ratio", gst::Fraction::new(*par_n, *par_d))
                .field("interlace-mode", interlace_mode.to_str())
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build()),
            #[cfg(feature = "advanced-sdk")]
            VideoInfo::H265 {
                xres,
                yres,
                fps_n,
                fps_d,
                par_n,
                par_d,
                interlace_mode,
                ..
            } => Ok(gst::Caps::builder("video/x-h265")
                .field("width", *xres)
                .field("height", *yres)
                .field("framerate", gst::Fraction::new(*fps_n, *fps_d))
                .field("pixel-aspect-ratio", gst::Fraction::new(*par_n, *par_d))
                .field("interlace-mode", interlace_mode.to_str())
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build()),
        }
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Buffer {
    Audio(gst::Buffer, AudioInfo),
    Video(gst::Buffer, VideoInfo),
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ReceiverItem {
    Buffer(Buffer),
    Flushing,
    Timeout,
    Error(gst::FlowError),
}

pub struct ReceiverInner {
    queue: ReceiverQueue,
    max_queue_length: usize,

    // Audio/video time observations
    observations_timestamp: [Observations; 2],
    observations_timecode: [Observations; 2],

    element: glib::WeakRef<gst::Element>,
    timestamp_mode: TimestampMode,

    timeout: u32,
    connect_timeout: u32,

    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[derive(Clone)]
struct ReceiverQueue(Arc<(Mutex<ReceiverQueueInner>, Condvar)>);

struct ReceiverQueueInner {
    // Set to true when the capture thread should be stopped
    shutdown: bool,

    // If we're flushing right now and all buffers should simply be discarded
    // and capture() directly returns Flushing
    flushing: bool,

    // If we're playing right now or not: if not we simply discard everything captured
    playing: bool,
    // Queue containing our buffers. This holds at most 5 buffers at a time.
    //
    // On timeout/error will contain a single item and then never be filled again
    buffer_queue: VecDeque<Buffer>,

    error: Option<gst::FlowError>,
    timeout: bool,
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
            obj: element,
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
                        obj: element,
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
                        obj: element,
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
                        obj: element,
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
            obj: element,
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
                obj: element,
                "Delta {} too far from skew {}, resetting",
                delta,
                inner.skew
            );

            let discont = !inner.deltas.is_empty();

            gst::debug!(
                CAT,
                obj: element,
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
            obj: element,
            "Skew {}, min delta {}",
            inner.skew,
            inner.min_delta
        );
        gst::trace!(CAT, obj: element, "Outputting {}", out_time.nseconds());

        Some((out_time.nseconds(), duration, false))
    }
}

#[derive(Clone)]
pub struct ReceiverControlHandle {
    queue: ReceiverQueue,
}

impl ReceiverControlHandle {
    pub fn set_flushing(&self, flushing: bool) {
        let mut queue = (self.queue.0).0.lock().unwrap();
        queue.flushing = flushing;
        (self.queue.0).1.notify_all();
    }

    pub fn set_playing(&self, playing: bool) {
        let mut queue = (self.queue.0).0.lock().unwrap();
        queue.playing = playing;
    }

    pub fn shutdown(&self) {
        let mut queue = (self.queue.0).0.lock().unwrap();
        queue.shutdown = true;
        (self.queue.0).1.notify_all();
    }
}

impl Drop for ReceiverInner {
    fn drop(&mut self) {
        // Will shut down the receiver thread on the next iteration
        let mut queue = (self.queue.0).0.lock().unwrap();
        queue.shutdown = true;
        drop(queue);

        let element = self.element.upgrade();

        if let Some(ref element) = element {
            gst::debug!(CAT, obj: element, "Closed NDI connection");
        }
    }
}

impl Receiver {
    fn new(
        recv: RecvInstance,
        timestamp_mode: TimestampMode,
        timeout: u32,
        connect_timeout: u32,
        max_queue_length: usize,
        element: &gst::Element,
    ) -> Self {
        let receiver = Receiver(Arc::new(ReceiverInner {
            queue: ReceiverQueue(Arc::new((
                Mutex::new(ReceiverQueueInner {
                    shutdown: false,
                    playing: false,
                    flushing: false,
                    buffer_queue: VecDeque::with_capacity(max_queue_length),
                    error: None,
                    timeout: false,
                }),
                Condvar::new(),
            ))),
            max_queue_length,
            observations_timestamp: Default::default(),
            observations_timecode: Default::default(),
            element: element.downgrade(),
            timestamp_mode,
            timeout,
            connect_timeout,
            thread: Mutex::new(None),
        }));

        let weak = Arc::downgrade(&receiver.0);
        let thread = thread::spawn(move || {
            use std::panic;

            let weak_clone = weak.clone();
            match panic::catch_unwind(panic::AssertUnwindSafe(move || {
                Self::receive_thread(&weak_clone, recv)
            })) {
                Ok(_) => (),
                Err(_) => {
                    if let Some(receiver) = weak.upgrade().map(Receiver) {
                        if let Some(element) = receiver.0.element.upgrade() {
                            gst::element_error!(
                                element,
                                gst::LibraryError::Failed,
                                ["Panic while connecting to NDI source"]
                            );
                        }

                        let mut queue = (receiver.0.queue.0).0.lock().unwrap();
                        queue.error = Some(gst::FlowError::Error);
                        (receiver.0.queue.0).1.notify_one();
                    }
                }
            }
        });

        *receiver.0.thread.lock().unwrap() = Some(thread);

        receiver
    }

    pub fn receiver_control_handle(&self) -> ReceiverControlHandle {
        ReceiverControlHandle {
            queue: self.0.queue.clone(),
        }
    }

    #[allow(dead_code)]
    pub fn set_flushing(&self, flushing: bool) {
        let mut queue = (self.0.queue.0).0.lock().unwrap();
        queue.flushing = flushing;
        (self.0.queue.0).1.notify_all();
    }

    #[allow(dead_code)]
    pub fn set_playing(&self, playing: bool) {
        let mut queue = (self.0.queue.0).0.lock().unwrap();
        queue.playing = playing;
    }

    #[allow(dead_code)]
    pub fn shutdown(&self) {
        let mut queue = (self.0.queue.0).0.lock().unwrap();
        queue.shutdown = true;
        (self.0.queue.0).1.notify_all();
    }

    pub fn capture(&self) -> ReceiverItem {
        let mut queue = (self.0.queue.0).0.lock().unwrap();
        loop {
            if let Some(err) = queue.error {
                return ReceiverItem::Error(err);
            } else if queue.buffer_queue.is_empty() && queue.timeout {
                return ReceiverItem::Timeout;
            } else if queue.flushing || queue.shutdown {
                return ReceiverItem::Flushing;
            } else if let Some(buffer) = queue.buffer_queue.pop_front() {
                return ReceiverItem::Buffer(buffer);
            }

            queue = (self.0.queue.0).1.wait(queue).unwrap();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        element: &gst::Element,
        ndi_name: Option<&str>,
        url_address: Option<&str>,
        receiver_ndi_name: &str,
        connect_timeout: u32,
        bandwidth: NDIlib_recv_bandwidth_e,
        color_format: NDIlib_recv_color_format_e,
        timestamp_mode: TimestampMode,
        timeout: u32,
        max_queue_length: usize,
    ) -> Option<Self> {
        gst::debug!(CAT, obj: element, "Starting NDI connection...");

        assert!(ndi_name.is_some() || url_address.is_some());

        gst::debug!(
            CAT,
            obj: element,
            "Connecting to NDI source with NDI name '{:?}' and URL/Address {:?}",
            ndi_name,
            url_address,
        );

        // FIXME: Ideally we would use NDIlib_recv_color_format_fastest here but that seems to be
        // broken with interlaced content currently
        let recv = RecvInstance::builder(ndi_name, url_address, receiver_ndi_name)
            .bandwidth(bandwidth)
            .color_format(color_format)
            .allow_video_fields(true)
            .build();
        let recv = match recv {
            None => {
                gst::element_error!(
                    element,
                    gst::CoreError::Negotiation,
                    ["Failed to connect to source"]
                );
                return None;
            }
            Some(recv) => recv,
        };

        recv.set_tally(&Tally::default());

        let enable_hw_accel = MetadataFrame::new(0, Some("<ndi_hwaccel enabled=\"true\"/>"));
        recv.send_metadata(&enable_hw_accel);

        // This will set info.audio/video accordingly
        let receiver = Receiver::new(
            recv,
            timestamp_mode,
            timeout,
            connect_timeout,
            max_queue_length,
            element,
        );

        Some(receiver)
    }

    fn receive_thread(receiver: &Weak<ReceiverInner>, recv: RecvInstance) {
        let mut first_video_frame = true;
        let mut first_audio_frame = true;
        let mut first_frame = true;
        let mut timer = time::Instant::now();

        // Capture until error or shutdown
        loop {
            let receiver = match receiver.upgrade().map(Receiver) {
                None => break,
                Some(receiver) => receiver,
            };

            let element = match receiver.0.element.upgrade() {
                None => return,
                Some(element) => element,
            };

            let flushing = {
                let queue = (receiver.0.queue.0).0.lock().unwrap();
                if queue.shutdown {
                    gst::debug!(CAT, obj: element, "Shutting down");
                    break;
                }

                // If an error happened in the meantime, just go out of here
                if queue.error.is_some() {
                    gst::error!(CAT, obj: element, "Error while waiting for connection");
                    return;
                }

                queue.flushing
            };

            let timeout = if first_frame {
                receiver.0.connect_timeout
            } else {
                receiver.0.timeout
            };

            let res = match recv.capture(50) {
                _ if flushing => {
                    gst::debug!(CAT, obj: element, "Flushing");
                    Err(gst::FlowError::Flushing)
                }
                Err(_) => {
                    gst::element_error!(
                        element,
                        gst::ResourceError::Read,
                        ["Error receiving frame"]
                    );
                    Err(gst::FlowError::Error)
                }
                Ok(None) if timeout > 0 && timer.elapsed().as_millis() >= timeout as u128 => {
                    gst::debug!(CAT, obj: element, "Timed out -- assuming EOS",);
                    Err(gst::FlowError::Eos)
                }
                Ok(None) => {
                    gst::debug!(CAT, obj: element, "No frame received yet, retry");
                    continue;
                }
                Ok(Some(Frame::Video(frame))) => {
                    first_frame = false;
                    let mut buffer = receiver.create_video_buffer_and_info(&element, frame);
                    if first_video_frame {
                        if let Ok(Buffer::Video(ref mut buffer, _)) = buffer {
                            buffer
                                .get_mut()
                                .unwrap()
                                .set_flags(gst::BufferFlags::DISCONT);
                            first_video_frame = false;
                        }
                    }
                    buffer
                }
                Ok(Some(Frame::Audio(frame))) => {
                    first_frame = false;
                    let mut buffer = receiver.create_audio_buffer_and_info(&element, frame);
                    if first_audio_frame {
                        if let Ok(Buffer::Audio(ref mut buffer, _)) = buffer {
                            buffer
                                .get_mut()
                                .unwrap()
                                .set_flags(gst::BufferFlags::DISCONT);
                            first_audio_frame = false;
                        }
                    }
                    buffer
                }
                Ok(Some(Frame::Metadata(frame))) => {
                    if let Some(metadata) = frame.metadata() {
                        gst::debug!(
                            CAT,
                            obj: element,
                            "Received metadata at timecode {}: {}",
                            (frame.timecode() as u64 * 100).nseconds(),
                            metadata,
                        );
                    }

                    continue;
                }
            };

            match res {
                Ok(item) => {
                    let mut queue = (receiver.0.queue.0).0.lock().unwrap();
                    while queue.buffer_queue.len() > receiver.0.max_queue_length {
                        gst::warning!(
                            CAT,
                            obj: element,
                            "Dropping old buffer -- queue has {} items",
                            queue.buffer_queue.len()
                        );
                        queue.buffer_queue.pop_front();
                    }
                    queue.buffer_queue.push_back(item);
                    (receiver.0.queue.0).1.notify_one();
                    timer = time::Instant::now();
                }
                Err(gst::FlowError::Eos) => {
                    gst::debug!(CAT, obj: element, "Signalling EOS");
                    let mut queue = (receiver.0.queue.0).0.lock().unwrap();
                    queue.timeout = true;
                    (receiver.0.queue.0).1.notify_one();
                    break;
                }
                Err(gst::FlowError::Flushing) => {
                    // Flushing, nothing to be done here except for emptying our queue
                    let mut queue = (receiver.0.queue.0).0.lock().unwrap();
                    queue.buffer_queue.clear();
                    (receiver.0.queue.0).1.notify_one();
                    timer = time::Instant::now();
                }
                Err(err) => {
                    gst::error!(CAT, obj: element, "Signalling error");
                    let mut queue = (receiver.0.queue.0).0.lock().unwrap();
                    if queue.error.is_none() {
                        queue.error = Some(err);
                    }
                    (receiver.0.queue.0).1.notify_one();
                    break;
                }
            }
        }
    }

    fn calculate_timestamp(
        &self,
        element: &gst::Element,
        is_audio: bool,
        timestamp: i64,
        timecode: i64,
        duration: Option<gst::ClockTime>,
    ) -> Option<(gst::ClockTime, Option<gst::ClockTime>, bool)> {
        let receive_time = element.current_running_time()?;

        let real_time_now = (glib::real_time() as u64 * 1000).nseconds();
        let timestamp = if timestamp == ndisys::NDIlib_recv_timestamp_undefined {
            gst::ClockTime::NONE
        } else {
            Some((timestamp as u64 * 100).nseconds())
        };
        let timecode = (timecode as u64 * 100).nseconds();

        gst::log!(
            CAT,
            obj: element,
            "Received frame with timecode {}, timestamp {}, duration {}, receive time {}, local time now {}",
            timecode,
            timestamp.display(),
            duration.display(),
            receive_time.display(),
            real_time_now,
        );

        let res_timestamp = self.0.observations_timestamp[usize::from(!is_audio)].process(
            element,
            timestamp,
            receive_time,
            duration,
        );

        let res_timecode = self.0.observations_timecode[usize::from(!is_audio)].process(
            element,
            Some(timecode),
            receive_time,
            duration,
        );

        let (pts, duration, discont) = match self.0.timestamp_mode {
            TimestampMode::ReceiveTimeTimecode => match res_timecode {
                Some((pts, duration, discont)) => (pts, duration, discont),
                None => {
                    gst::warning!(CAT, obj: element, "Can't calculate timestamp");
                    (receive_time, duration, false)
                }
            },
            TimestampMode::ReceiveTimeTimestamp => match res_timestamp {
                Some((pts, duration, discont)) => (pts, duration, discont),
                None => {
                    if timestamp.is_some() {
                        gst::warning!(CAT, obj: element, "Can't calculate timestamp");
                    }

                    (receive_time, duration, false)
                }
            },
            TimestampMode::Timecode => (timecode, duration, false),
            TimestampMode::Timestamp if timestamp.is_none() => (receive_time, duration, false),
            TimestampMode::Timestamp => {
                // Timestamps are relative to the UNIX epoch
                let timestamp = timestamp?;
                if real_time_now > timestamp {
                    let diff = real_time_now - timestamp;
                    if diff > receive_time {
                        (gst::ClockTime::ZERO, duration, false)
                    } else {
                        (receive_time - diff, duration, false)
                    }
                } else {
                    let diff = timestamp - real_time_now;
                    (receive_time + diff, duration, false)
                }
            }
            TimestampMode::ReceiveTime => (receive_time, duration, false),
            TimestampMode::Auto => {
                res_timecode
                    .or(res_timestamp)
                    .unwrap_or((receive_time, duration, false))
            }
        };

        gst::log!(
            CAT,
            obj: element,
            "Calculated PTS {}, duration {}",
            pts.display(),
            duration.display(),
        );

        Some((pts, duration, discont))
    }

    fn create_video_buffer_and_info(
        &self,
        element: &gst::Element,
        video_frame: VideoFrame,
    ) -> Result<Buffer, gst::FlowError> {
        gst::debug!(CAT, obj: element, "Received video frame {:?}", video_frame);

        let (pts, duration, discont) = self
            .calculate_video_timestamp(element, &video_frame)
            .ok_or_else(|| {
                gst::debug!(CAT, obj: element, "Flushing, dropping buffer");
                gst::FlowError::Flushing
            })?;

        let info = self.create_video_info(element, &video_frame)?;

        let mut buffer = self.create_video_buffer(element, pts, duration, &info, &video_frame)?;
        if discont {
            buffer
                .get_mut()
                .unwrap()
                .set_flags(gst::BufferFlags::RESYNC);
        }

        gst::log!(CAT, obj: element, "Produced video buffer {:?}", buffer);

        Ok(Buffer::Video(buffer, info))
    }

    fn calculate_video_timestamp(
        &self,
        element: &gst::Element,
        video_frame: &VideoFrame,
    ) -> Option<(gst::ClockTime, Option<gst::ClockTime>, bool)> {
        let duration = gst::ClockTime::SECOND.mul_div_floor(
            video_frame.frame_rate().1 as u64,
            video_frame.frame_rate().0 as u64,
        );

        self.calculate_timestamp(
            element,
            false,
            video_frame.timestamp(),
            video_frame.timecode(),
            duration,
        )
    }

    fn create_video_info(
        &self,
        element: &gst::Element,
        video_frame: &VideoFrame,
    ) -> Result<VideoInfo, gst::FlowError> {
        let fourcc = video_frame.fourcc();

        let par = gst::Fraction::approximate_f32(video_frame.picture_aspect_ratio())
            .unwrap_or_else(|| gst::Fraction::new(1, 1))
            * gst::Fraction::new(video_frame.yres(), video_frame.xres());
        let interlace_mode = match video_frame.frame_format_type() {
            ndisys::NDIlib_frame_format_type_e::NDIlib_frame_format_type_progressive => {
                gst_video::VideoInterlaceMode::Progressive
            }
            ndisys::NDIlib_frame_format_type_e::NDIlib_frame_format_type_interleaved => {
                gst_video::VideoInterlaceMode::Interleaved
            }
            #[cfg(feature = "interlaced-fields")]
            _ => gst_video::VideoInterlaceMode::Alternate,
            #[cfg(not(feature = "interlaced-fields"))]
            _ => {
                gst::element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Separate field interlacing not supported"]
                );
                return Err(gst::FlowError::NotNegotiated);
            }
        };

        if [
            ndisys::NDIlib_FourCC_video_type_UYVY,
            ndisys::NDIlib_FourCC_video_type_UYVA,
            ndisys::NDIlib_FourCC_video_type_YV12,
            ndisys::NDIlib_FourCC_video_type_NV12,
            ndisys::NDIlib_FourCC_video_type_I420,
            ndisys::NDIlib_FourCC_video_type_BGRA,
            ndisys::NDIlib_FourCC_video_type_BGRX,
            ndisys::NDIlib_FourCC_video_type_RGBA,
            ndisys::NDIlib_FourCC_video_type_BGRX,
        ]
        .contains(&fourcc)
        {
            // YV12 and I420 are swapped in the NDI SDK compared to GStreamer
            let format = match video_frame.fourcc() {
                ndisys::NDIlib_FourCC_video_type_UYVY => gst_video::VideoFormat::Uyvy,
                // FIXME: This drops the alpha plane!
                ndisys::NDIlib_FourCC_video_type_UYVA => gst_video::VideoFormat::Uyvy,
                ndisys::NDIlib_FourCC_video_type_YV12 => gst_video::VideoFormat::I420,
                ndisys::NDIlib_FourCC_video_type_NV12 => gst_video::VideoFormat::Nv12,
                ndisys::NDIlib_FourCC_video_type_I420 => gst_video::VideoFormat::Yv12,
                ndisys::NDIlib_FourCC_video_type_BGRA => gst_video::VideoFormat::Bgra,
                ndisys::NDIlib_FourCC_video_type_BGRX => gst_video::VideoFormat::Bgrx,
                ndisys::NDIlib_FourCC_video_type_RGBA => gst_video::VideoFormat::Rgba,
                ndisys::NDIlib_FourCC_video_type_RGBX => gst_video::VideoFormat::Rgbx,
                _ => {
                    gst::element_error!(
                        element,
                        gst::StreamError::Format,
                        ["Unsupported video fourcc {:08x}", video_frame.fourcc()]
                    );

                    return Err(gst::FlowError::NotNegotiated);
                } // TODO: NDIlib_FourCC_video_type_P216 and NDIlib_FourCC_video_type_PA16 not
                  // supported by GStreamer
            };

            #[cfg(feature = "interlaced-fields")]
            {
                let mut builder = gst_video::VideoInfo::builder(
                    format,
                    video_frame.xres() as u32,
                    video_frame.yres() as u32,
                )
                .fps(gst::Fraction::from(video_frame.frame_rate()))
                .par(par)
                .interlace_mode(interlace_mode);

                if video_frame.frame_format_type()
                    == ndisys::NDIlib_frame_format_type_e::NDIlib_frame_format_type_interleaved
                {
                    builder = builder.field_order(gst_video::VideoFieldOrder::TopFieldFirst);
                }

                return Ok(VideoInfo::Video(builder.build().map_err(|_| {
                    gst::element_error!(
                        element,
                        gst::StreamError::Format,
                        ["Invalid video format configuration"]
                    );

                    gst::FlowError::NotNegotiated
                })?));
            }

            #[cfg(not(feature = "interlaced-fields"))]
            {
                let mut builder = gst_video::VideoInfo::builder(
                    format,
                    video_frame.xres() as u32,
                    video_frame.yres() as u32,
                )
                .fps(gst::Fraction::from(video_frame.frame_rate()))
                .par(par)
                .interlace_mode(interlace_mode);

                if video_frame.frame_format_type()
                    == ndisys::NDIlib_frame_format_type_e::NDIlib_frame_format_type_interleaved
                {
                    builder = builder.field_order(gst_video::VideoFieldOrder::TopFieldFirst);
                }

                return Ok(VideoInfo::Video(builder.build().map_err(|_| {
                    gst::element_error!(
                        element,
                        gst::StreamError::Format,
                        ["Invalid video format configuration"]
                    );

                    gst::FlowError::NotNegotiated
                })?));
            }
        }

        #[cfg(feature = "advanced-sdk")]
        if [
            ndisys::NDIlib_FourCC_video_type_ex_SHQ0_highest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_SHQ2_highest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_SHQ7_highest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_SHQ0_lowest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_SHQ2_lowest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_SHQ7_lowest_bandwidth,
        ]
        .contains(&fourcc)
        {
            let variant = match fourcc {
                ndisys::NDIlib_FourCC_video_type_ex_SHQ0_highest_bandwidth
                | ndisys::NDIlib_FourCC_video_type_ex_SHQ0_lowest_bandwidth => String::from("SHQ0"),
                ndisys::NDIlib_FourCC_video_type_ex_SHQ2_highest_bandwidth
                | ndisys::NDIlib_FourCC_video_type_ex_SHQ2_lowest_bandwidth => String::from("SHQ2"),
                ndisys::NDIlib_FourCC_video_type_ex_SHQ7_highest_bandwidth
                | ndisys::NDIlib_FourCC_video_type_ex_SHQ7_lowest_bandwidth => String::from("SHQ7"),
                _ => {
                    gst::element_error!(
                        element,
                        gst::StreamError::Format,
                        [
                            "Unsupported SpeedHQ video fourcc {:08x}",
                            video_frame.fourcc()
                        ]
                    );

                    return Err(gst::FlowError::NotNegotiated);
                }
            };

            return Ok(VideoInfo::SpeedHQInfo {
                variant,
                xres: video_frame.xres(),
                yres: video_frame.yres(),
                fps_n: video_frame.frame_rate().0,
                fps_d: video_frame.frame_rate().1,
                par_n: par.numer(),
                par_d: par.denom(),
                interlace_mode,
            });
        }

        #[cfg(feature = "advanced-sdk")]
        if [
            ndisys::NDIlib_FourCC_video_type_ex_H264_highest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_H264_lowest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_H264_alpha_highest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_H264_alpha_lowest_bandwidth,
        ]
        .contains(&fourcc)
        {
            let compressed_packet = video_frame.compressed_packet().ok_or_else(|| {
                gst::error!(
                    CAT,
                    obj: element,
                    "Video packet doesn't have compressed packet start"
                );
                gst::element_error!(element, gst::StreamError::Format, ["Invalid video packet"]);

                gst::FlowError::Error
            })?;

            if compressed_packet.fourcc != NDIlib_compressed_FourCC_type_H264 {
                gst::error!(CAT, obj: element, "Non-H264 video packet");
                gst::element_error!(element, gst::StreamError::Format, ["Invalid video packet"]);

                return Err(gst::FlowError::Error);
            }

            return Ok(VideoInfo::H264 {
                xres: video_frame.xres(),
                yres: video_frame.yres(),
                fps_n: video_frame.frame_rate().0,
                fps_d: video_frame.frame_rate().1,
                par_n: par.numer(),
                par_d: par.denom(),
                interlace_mode,
            });
        }

        #[cfg(feature = "advanced-sdk")]
        if [
            ndisys::NDIlib_FourCC_video_type_ex_HEVC_highest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_HEVC_lowest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_HEVC_alpha_highest_bandwidth,
            ndisys::NDIlib_FourCC_video_type_ex_HEVC_alpha_lowest_bandwidth,
        ]
        .contains(&fourcc)
        {
            let compressed_packet = video_frame.compressed_packet().ok_or_else(|| {
                gst::error!(
                    CAT,
                    obj: element,
                    "Video packet doesn't have compressed packet start"
                );
                gst::element_error!(element, gst::StreamError::Format, ["Invalid video packet"]);

                gst::FlowError::Error
            })?;

            if compressed_packet.fourcc != NDIlib_compressed_FourCC_type_HEVC {
                gst::error!(CAT, obj: element, "Non-H265 video packet");
                gst::element_error!(element, gst::StreamError::Format, ["Invalid video packet"]);

                return Err(gst::FlowError::Error);
            }

            return Ok(VideoInfo::H265 {
                xres: video_frame.xres(),
                yres: video_frame.yres(),
                fps_n: video_frame.frame_rate().0,
                fps_d: video_frame.frame_rate().1,
                par_n: par.numer(),
                par_d: par.denom(),
                interlace_mode,
            });
        }

        gst::element_error!(
            element,
            gst::StreamError::Format,
            ["Unsupported video fourcc {:08x}", video_frame.fourcc()]
        );
        Err(gst::FlowError::NotNegotiated)
    }

    fn create_video_buffer(
        &self,
        element: &gst::Element,
        pts: gst::ClockTime,
        duration: Option<gst::ClockTime>,
        info: &VideoInfo,
        video_frame: &VideoFrame,
    ) -> Result<gst::Buffer, gst::FlowError> {
        let mut buffer = self.copy_video_frame(element, info, video_frame)?;
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(pts);
            buffer.set_duration(duration);

            gst::ReferenceTimestampMeta::add(
                buffer,
                &crate::TIMECODE_CAPS,
                (video_frame.timecode() as u64 * 100).nseconds(),
                gst::ClockTime::NONE,
            );
            if video_frame.timestamp() != ndisys::NDIlib_recv_timestamp_undefined {
                gst::ReferenceTimestampMeta::add(
                    buffer,
                    &crate::TIMESTAMP_CAPS,
                    (video_frame.timestamp() as u64 * 100).nseconds(),
                    gst::ClockTime::NONE,
                );
            }

            #[cfg(feature = "interlaced-fields")]
            {
                match video_frame.frame_format_type() {
                    ndisys::NDIlib_frame_format_type_e::NDIlib_frame_format_type_interleaved => {
                        buffer.set_video_flags(
                            gst_video::VideoBufferFlags::INTERLACED
                                | gst_video::VideoBufferFlags::TFF,
                        );
                    }
                    ndisys::NDIlib_frame_format_type_e::NDIlib_frame_format_type_field_0 => {
                        buffer.set_video_flags(
                            gst_video::VideoBufferFlags::INTERLACED
                                | gst_video::VideoBufferFlags::TOP_FIELD,
                        );
                    }
                    ndisys::NDIlib_frame_format_type_e::NDIlib_frame_format_type_field_1 => {
                        buffer.set_video_flags(
                            gst_video::VideoBufferFlags::INTERLACED
                                | gst_video::VideoBufferFlags::BOTTOM_FIELD,
                        );
                    }
                    _ => (),
                };
            }

            #[cfg(not(feature = "interlaced-fields"))]
            {
                if video_frame.frame_format_type()
                    == ndisys::NDIlib_frame_format_type_e::NDIlib_frame_format_type_interleaved
                {
                    buffer.set_video_flags(
                        gst_video::VideoBufferFlags::INTERLACED | gst_video::VideoBufferFlags::TFF,
                    );
                }
            }
        }

        Ok(buffer)
    }

    fn copy_video_frame(
        &self,
        #[allow(unused_variables)] element: &gst::Element,
        info: &VideoInfo,
        video_frame: &VideoFrame,
    ) -> Result<gst::Buffer, gst::FlowError> {
        match info {
            VideoInfo::Video(ref info) => {
                let src = video_frame.data().ok_or(gst::FlowError::Error)?;

                let buffer = gst::Buffer::with_size(info.size()).unwrap();
                let mut vframe = gst_video::VideoFrame::from_buffer_writable(buffer, info).unwrap();

                match info.format() {
                    gst_video::VideoFormat::Uyvy
                    | gst_video::VideoFormat::Bgra
                    | gst_video::VideoFormat::Bgrx
                    | gst_video::VideoFormat::Rgba
                    | gst_video::VideoFormat::Rgbx => {
                        let line_bytes = if info.format() == gst_video::VideoFormat::Uyvy {
                            2 * vframe.width() as usize
                        } else {
                            4 * vframe.width() as usize
                        };

                        let dest_stride = vframe.plane_stride()[0] as usize;
                        let dest = vframe.plane_data_mut(0).unwrap();
                        let src_stride = video_frame.line_stride_or_data_size_in_bytes() as usize;
                        let plane_size = video_frame.yres() as usize * src_stride;

                        if src.len() < plane_size || src_stride < line_bytes {
                            gst::error!(CAT, obj: element, "Video packet has wrong stride or size");
                            gst::element_error!(
                                element,
                                gst::StreamError::Format,
                                ["Video packet has wrong stride or size"]
                            );
                            return Err(gst::FlowError::Error);
                        }

                        for (dest, src) in dest
                            .chunks_exact_mut(dest_stride)
                            .zip(src.chunks_exact(src_stride))
                        {
                            dest[..line_bytes].copy_from_slice(&src[..line_bytes]);
                        }
                    }
                    gst_video::VideoFormat::Nv12 => {
                        let line_bytes = vframe.width() as usize;
                        let src_stride = video_frame.line_stride_or_data_size_in_bytes() as usize;
                        let plane_size = video_frame.yres() as usize * src_stride;

                        if src.len() < 2 * plane_size || src_stride < line_bytes {
                            gst::error!(CAT, obj: element, "Video packet has wrong stride or size");
                            gst::element_error!(
                                element,
                                gst::StreamError::Format,
                                ["Video packet has wrong stride or size"]
                            );
                            return Err(gst::FlowError::Error);
                        }

                        // First plane
                        {
                            let dest_stride = vframe.plane_stride()[0] as usize;
                            let dest = vframe.plane_data_mut(0).unwrap();
                            let src = &src[..plane_size];

                            for (dest, src) in dest
                                .chunks_exact_mut(dest_stride)
                                .zip(src.chunks_exact(src_stride))
                            {
                                dest[..line_bytes].copy_from_slice(&src[..line_bytes]);
                            }
                        }

                        // Second plane
                        {
                            let dest_stride = vframe.plane_stride()[1] as usize;
                            let dest = vframe.plane_data_mut(1).unwrap();
                            let src = &src[plane_size..];

                            for (dest, src) in dest
                                .chunks_exact_mut(dest_stride)
                                .zip(src.chunks_exact(src_stride))
                            {
                                dest[..line_bytes].copy_from_slice(&src[..line_bytes]);
                            }
                        }
                    }
                    gst_video::VideoFormat::Yv12 | gst_video::VideoFormat::I420 => {
                        let line_bytes = vframe.width() as usize;
                        let line_bytes1 = (line_bytes + 1) / 2;

                        let src_stride = video_frame.line_stride_or_data_size_in_bytes() as usize;
                        let src_stride1 = (src_stride + 1) / 2;

                        let plane_size = video_frame.yres() as usize * src_stride;
                        let plane_size1 = ((video_frame.yres() as usize + 1) / 2) * src_stride1;

                        if src.len() < plane_size + 2 * plane_size1 || src_stride < line_bytes {
                            gst::error!(CAT, obj: element, "Video packet has wrong stride or size");
                            gst::element_error!(
                                element,
                                gst::StreamError::Format,
                                ["Video packet has wrong stride or size"]
                            );
                            return Err(gst::FlowError::Error);
                        }

                        // First plane
                        {
                            let dest_stride = vframe.plane_stride()[0] as usize;
                            let dest = vframe.plane_data_mut(0).unwrap();
                            let src = &src[..plane_size];

                            for (dest, src) in dest
                                .chunks_exact_mut(dest_stride)
                                .zip(src.chunks_exact(src_stride))
                            {
                                dest[..line_bytes].copy_from_slice(&src[..line_bytes]);
                            }
                        }

                        // Second plane
                        {
                            let dest_stride = vframe.plane_stride()[1] as usize;
                            let dest = vframe.plane_data_mut(1).unwrap();
                            let src = &src[plane_size..][..plane_size1];

                            for (dest, src) in dest
                                .chunks_exact_mut(dest_stride)
                                .zip(src.chunks_exact(src_stride1))
                            {
                                dest[..line_bytes1].copy_from_slice(&src[..line_bytes1]);
                            }
                        }

                        // Third plane
                        {
                            let dest_stride = vframe.plane_stride()[2] as usize;
                            let dest = vframe.plane_data_mut(2).unwrap();
                            let src = &src[plane_size + plane_size1..][..plane_size1];

                            for (dest, src) in dest
                                .chunks_exact_mut(dest_stride)
                                .zip(src.chunks_exact(src_stride1))
                            {
                                dest[..line_bytes1].copy_from_slice(&src[..line_bytes1]);
                            }
                        }
                    }
                    _ => unreachable!(),
                }

                Ok(vframe.into_buffer())
            }
            #[cfg(feature = "advanced-sdk")]
            VideoInfo::SpeedHQInfo { .. } => {
                let data = video_frame.data().ok_or_else(|| {
                    gst::error!(CAT, obj: element, "Video packet has no data");
                    gst::element_error!(
                        element,
                        gst::StreamError::Format,
                        ["Invalid video packet"]
                    );

                    gst::FlowError::Error
                })?;

                Ok(gst::Buffer::from_mut_slice(Vec::from(data)))
            }
            #[cfg(feature = "advanced-sdk")]
            VideoInfo::H264 { .. } | VideoInfo::H265 { .. } => {
                let compressed_packet = video_frame.compressed_packet().ok_or_else(|| {
                    gst::error!(
                        CAT,
                        obj: element,
                        "Video packet doesn't have compressed packet start"
                    );
                    gst::element_error!(
                        element,
                        gst::StreamError::Format,
                        ["Invalid video packet"]
                    );

                    gst::FlowError::Error
                })?;

                let mut buffer = Vec::new();
                if let Some(extra_data) = compressed_packet.extra_data {
                    buffer.extend_from_slice(extra_data);
                }
                buffer.extend_from_slice(compressed_packet.data);
                let mut buffer = gst::Buffer::from_mut_slice(buffer);
                if !compressed_packet.key_frame {
                    let buffer = buffer.get_mut().unwrap();
                    buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
                }

                Ok(buffer)
            }
        }
    }

    fn create_audio_buffer_and_info(
        &self,
        element: &gst::Element,
        audio_frame: AudioFrame,
    ) -> Result<Buffer, gst::FlowError> {
        gst::debug!(CAT, obj: element, "Received audio frame {:?}", audio_frame);

        let (pts, duration, discont) = self
            .calculate_audio_timestamp(element, &audio_frame)
            .ok_or_else(|| {
                gst::debug!(CAT, obj: element, "Flushing, dropping buffer");
                gst::FlowError::Flushing
            })?;

        let info = self.create_audio_info(element, &audio_frame)?;

        let mut buffer = self.create_audio_buffer(element, pts, duration, &info, &audio_frame)?;
        if discont {
            buffer
                .get_mut()
                .unwrap()
                .set_flags(gst::BufferFlags::RESYNC);
        }

        gst::log!(CAT, obj: element, "Produced audio buffer {:?}", buffer);

        Ok(Buffer::Audio(buffer, info))
    }

    fn calculate_audio_timestamp(
        &self,
        element: &gst::Element,
        audio_frame: &AudioFrame,
    ) -> Option<(gst::ClockTime, Option<gst::ClockTime>, bool)> {
        let duration = gst::ClockTime::SECOND.mul_div_floor(
            audio_frame.no_samples() as u64,
            audio_frame.sample_rate() as u64,
        );

        self.calculate_timestamp(
            element,
            true,
            audio_frame.timestamp(),
            audio_frame.timecode(),
            duration,
        )
    }

    fn create_audio_info(
        &self,
        element: &gst::Element,
        audio_frame: &AudioFrame,
    ) -> Result<AudioInfo, gst::FlowError> {
        let fourcc = audio_frame.fourcc();

        if [NDIlib_FourCC_audio_type_FLTp].contains(&fourcc) {
            let channels = audio_frame.no_channels() as u32;
            let mut positions = [gst_audio::AudioChannelPosition::None; 64];
            let _ = gst_audio::AudioChannelPosition::positions_from_mask(
                gst_audio::AudioChannelPosition::fallback_mask(channels),
                &mut positions[..channels as usize],
            );

            let builder = gst_audio::AudioInfo::builder(
                gst_audio::AUDIO_FORMAT_F32,
                audio_frame.sample_rate() as u32,
                channels,
            )
            .positions(&positions[..channels as usize]);

            let info = builder.build().map_err(|_| {
                gst::element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Invalid audio format configuration"]
                );

                gst::FlowError::NotNegotiated
            })?;

            return Ok(AudioInfo::Audio(info));
        }

        #[cfg(feature = "advanced-sdk")]
        if [NDIlib_FourCC_audio_type_AAC].contains(&fourcc) {
            let compressed_packet = audio_frame.compressed_packet().ok_or_else(|| {
                gst::error!(
                    CAT,
                    obj: element,
                    "Audio packet doesn't have compressed packet start"
                );
                gst::element_error!(element, gst::StreamError::Format, ["Invalid audio packet"]);

                gst::FlowError::Error
            })?;

            if compressed_packet.fourcc != NDIlib_compressed_FourCC_type_AAC {
                gst::error!(CAT, obj: element, "Non-AAC audio packet");
                gst::element_error!(element, gst::StreamError::Format, ["Invalid audio packet"]);

                return Err(gst::FlowError::Error);
            }

            return Ok(AudioInfo::Aac {
                sample_rate: audio_frame.sample_rate(),
                no_channels: audio_frame.no_channels(),
                codec_data: compressed_packet
                    .extra_data
                    .ok_or(gst::FlowError::NotNegotiated)?
                    .try_into()
                    .map_err(|_| gst::FlowError::NotNegotiated)?,
            });
        }

        #[cfg(feature = "advanced-sdk")]
        if [NDIlib_FourCC_audio_type_Opus].contains(&fourcc) {}

        gst::element_error!(
            element,
            gst::StreamError::Format,
            ["Unsupported audio fourcc {:08x}", audio_frame.fourcc()]
        );
        Err(gst::FlowError::NotNegotiated)
    }

    fn create_audio_buffer(
        &self,
        #[allow(unused_variables)] element: &gst::Element,
        pts: gst::ClockTime,
        duration: Option<gst::ClockTime>,
        info: &AudioInfo,
        audio_frame: &AudioFrame,
    ) -> Result<gst::Buffer, gst::FlowError> {
        match info {
            AudioInfo::Audio(ref info) => {
                let src = audio_frame.data().ok_or(gst::FlowError::Error)?;
                let buff_size = (audio_frame.no_samples() as u32 * info.bpf()) as usize;

                let mut buffer = gst::Buffer::with_size(buff_size).unwrap();
                {
                    let buffer = buffer.get_mut().unwrap();

                    buffer.set_pts(pts);
                    buffer.set_duration(duration);

                    gst::ReferenceTimestampMeta::add(
                        buffer,
                        &crate::TIMECODE_CAPS,
                        (audio_frame.timecode() as u64 * 100).nseconds(),
                        gst::ClockTime::NONE,
                    );
                    if audio_frame.timestamp() != ndisys::NDIlib_recv_timestamp_undefined {
                        gst::ReferenceTimestampMeta::add(
                            buffer,
                            &crate::TIMESTAMP_CAPS,
                            (audio_frame.timestamp() as u64 * 100).nseconds(),
                            gst::ClockTime::NONE,
                        );
                    }

                    let mut dest = buffer.map_writable().unwrap();
                    let dest = dest
                        .as_mut_slice_of::<f32>()
                        .map_err(|_| gst::FlowError::NotNegotiated)?;
                    assert!(
                        dest.len()
                            == audio_frame.no_samples() as usize
                                * audio_frame.no_channels() as usize
                    );

                    for (channel, samples) in src
                        .chunks_exact(audio_frame.channel_stride_or_data_size_in_bytes() as usize)
                        .enumerate()
                    {
                        let samples = samples
                            .as_slice_of::<f32>()
                            .map_err(|_| gst::FlowError::NotNegotiated)?;

                        for (i, sample) in samples[..audio_frame.no_samples() as usize]
                            .iter()
                            .enumerate()
                        {
                            dest[i * (audio_frame.no_channels() as usize) + channel] = *sample;
                        }
                    }
                }

                Ok(buffer)
            }
            #[cfg(feature = "advanced-sdk")]
            AudioInfo::Opus { .. } => {
                let data = audio_frame.data().ok_or_else(|| {
                    gst::error!(CAT, obj: element, "Audio packet has no data");
                    gst::element_error!(
                        element,
                        gst::StreamError::Format,
                        ["Invalid audio packet"]
                    );

                    gst::FlowError::Error
                })?;

                Ok(gst::Buffer::from_mut_slice(Vec::from(data)))
            }
            #[cfg(feature = "advanced-sdk")]
            AudioInfo::Aac { .. } => {
                let compressed_packet = audio_frame.compressed_packet().ok_or_else(|| {
                    gst::error!(
                        CAT,
                        obj: element,
                        "Audio packet doesn't have compressed packet start"
                    );
                    gst::element_error!(
                        element,
                        gst::StreamError::Format,
                        ["Invalid audio packet"]
                    );

                    gst::FlowError::Error
                })?;

                Ok(gst::Buffer::from_mut_slice(Vec::from(
                    compressed_packet.data,
                )))
            }
        }
    }
}
