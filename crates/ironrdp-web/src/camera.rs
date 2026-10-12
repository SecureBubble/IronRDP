//! Browser-side camera for video-capture redirection (MS-RDPECAM).
//!
//! The page offers one camera. The host enumerates it, and when an application on the host opens
//! it the host starts a stream in one of the formats we list -- always H.264, which the page
//! encodes with the browser's own encoder (WebCodecs), because uncompressed video is far too large
//! for a user's uplink.
//!
//! Windows opens SEVERAL channels for one camera at once (seen: three, the stream started on one
//! and pictures requested on another), so the camera's state lives once, in [`CameraFeed`], and
//! every channel ([`CameraChannel`]) answers from it on its own channel id -- as FreeRDP keeps one
//! device behind all of a camera's channels. (The protocol crate's `DeviceClient` keeps the stream
//! state per channel, which answers the picture requests with errors.)
//!
//! * When the host starts or stops the stream, the event loop is told ([`CameraMessage`]) and the
//!   page starts or stops `getUserMedia` + the encoder.
//! * The page pushes each encoded picture with the `camera_frame` extension.
//! * The host pulls pictures one Sample Request at a time while the browser produces them on its
//!   own clock: a request that arrives before a picture is parked and answered by the event loop
//!   when the next picture arrives ([`CameraFeed::take_ready`]), as FreeRDP does.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_channel::mpsc;
use ironrdp::dvc::{DvcChannelListener, DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp_core::{decode, impl_as_any};
use ironrdp_pdu::PduResult;
use ironrdp_rdpecam::pdu::DevicePdu;
use ironrdp_rdpecam::{ErrorCode, MediaFormat, MediaType, RedirectedDevice, StreamDescription};
use tracing::{debug, error, warn};
use wasm_bindgen::JsValue;

use crate::session::RdpInputEvent;

/// Pictures waiting for the host. More than this means the host is not keeping up: start again
/// from the next key frame rather than stream ever later.
const MAX_QUEUED_FRAMES: usize = 30;

/// The sizes we offer, all 30 fps H.264; the host picks one.
const SIZES: [(u32, u32); 3] = [(1280, 720), (640, 480), (320, 240)];

/// Messages from the camera channels to the session event loop.
#[derive(Debug)]
pub(crate) enum CameraMessage {
    /// The host started streaming: capture and encode at this size and rate.
    Start { width: u32, height: u32, fps: u32 },
    /// The host stopped streaming.
    Stop,
    /// Pictures were dropped: the next one must be a key frame.
    Keyframe,
}

#[derive(Default)]
struct CameraState {
    /// The stream the host started, if any.
    streaming: Option<MediaType>,
    /// The format reported as current (the last one started).
    current: Option<MediaType>,
    frames: VecDeque<Vec<u8>>,
    /// Drop pictures until a key frame: set at stream start and after an overflow.
    need_keyframe: bool,
    /// Sample Requests waiting for a picture: (channel id, stream index).
    pending: VecDeque<(u32, u8)>,
    /// Open channels of this camera.
    channels: usize,
}

/// The one camera's state, shared by all its channels and the session.
#[derive(Clone)]
pub(crate) struct CameraFeed {
    state: Arc<Mutex<CameraState>>,
    tx: mpsc::UnboundedSender<RdpInputEvent>,
}

impl CameraFeed {
    pub(crate) fn new(tx: mpsc::UnboundedSender<RdpInputEvent>) -> Self {
        Self {
            state: Arc::default(),
            tx,
        }
    }

    fn send(&self, message: CameraMessage) {
        if self.tx.unbounded_send(RdpInputEvent::Camera(message)).is_err() {
            error!("Failed to queue camera message, event loop receiver is closed");
        }
    }

    /// One encoded picture (an H.264 access unit, Annex B). Returns `true` if a parked request can
    /// now be answered (call [`Self::take_ready`]).
    pub(crate) fn push(&self, frame: Vec<u8>, key: bool) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.streaming.is_none() || frame.is_empty() || (state.need_keyframe && !key) {
            return false;
        }
        state.need_keyframe = false;
        if state.frames.len() >= MAX_QUEUED_FRAMES {
            warn!("Host is not taking camera pictures fast enough; restarting from a key frame");
            state.frames.clear();
            state.need_keyframe = true;
            drop(state);
            self.send(CameraMessage::Keyframe);
            return false;
        }
        state.frames.push_back(frame);
        !state.pending.is_empty()
    }

    /// Parked requests that can be answered now, paired with their pictures.
    pub(crate) fn take_ready(&self) -> Vec<(u32, DevicePdu)> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        let mut ready = Vec::new();
        while !state.pending.is_empty() && !state.frames.is_empty() {
            let (channel_id, stream_index) = state.pending.pop_front().expect("checked");
            let sample = state.frames.pop_front().expect("checked");
            ready.push((channel_id, DevicePdu::SampleResponse { stream_index, sample }));
        }
        ready
    }

    fn stop(state: &mut CameraState) -> bool {
        let was = state.streaming.take().is_some();
        state.frames.clear();
        state.pending.clear();
        was
    }

    /// The answer to one request on `channel_id`; `None` = parked (a Sample Request waiting for
    /// a picture).
    fn respond(&self, channel_id: u32, request: DevicePdu) -> Option<DevicePdu> {
        let mut state = self.state.lock().ok()?;
        let response = match request {
            DevicePdu::ActivateDeviceRequest => {
                debug!(channel_id, "Camera activated by the host");
                DevicePdu::SuccessResponse
            }
            DevicePdu::DeactivateDeviceRequest => {
                debug!(channel_id, "Camera deactivated by the host");
                if Self::stop(&mut state) {
                    drop(state);
                    self.send(CameraMessage::Stop);
                }
                DevicePdu::SuccessResponse
            }
            DevicePdu::StreamListRequest => DevicePdu::StreamListResponse(vec![StreamDescription {
                frame_source_types: StreamDescription::COLOR,
                selected: true,
                can_be_shared: true,
            }]),
            DevicePdu::MediaTypeListRequest(0) => DevicePdu::MediaTypeListResponse(media_types()),
            DevicePdu::CurrentMediaTypeRequest(0) => {
                DevicePdu::CurrentMediaTypeResponse(state.current.unwrap_or_else(|| media_types()[1]))
            }
            DevicePdu::MediaTypeListRequest(_) | DevicePdu::CurrentMediaTypeRequest(_) => {
                DevicePdu::ErrorResponse(ErrorCode::InvalidStreamNumber)
            }
            DevicePdu::StartStreamsRequest(streams) => match streams.as_slice() {
                [stream] if stream.stream_index == 0 && media_types().contains(&stream.media_type) => {
                    let media_type = stream.media_type;
                    debug!(channel_id, ?media_type, "Host started the camera stream");
                    state.streaming = Some(media_type);
                    state.current = Some(media_type);
                    state.frames.clear();
                    state.pending.clear();
                    state.need_keyframe = true;
                    drop(state);
                    let fps = media_type.frame_rate_numerator / media_type.frame_rate_denominator.max(1);
                    self.send(CameraMessage::Start {
                        width: media_type.width,
                        height: media_type.height,
                        fps: fps.max(1),
                    });
                    DevicePdu::SuccessResponse
                }
                _ => DevicePdu::ErrorResponse(ErrorCode::InvalidMediaType),
            },
            DevicePdu::StopStreamsRequest => {
                debug!(channel_id, "Host stopped the camera stream");
                if Self::stop(&mut state) {
                    drop(state);
                    self.send(CameraMessage::Stop);
                }
                DevicePdu::SuccessResponse
            }
            DevicePdu::SampleRequest(stream_index) => {
                if state.streaming.is_none() || stream_index != 0 {
                    DevicePdu::SampleErrorResponse {
                        stream_index,
                        error: ErrorCode::InvalidRequest,
                    }
                } else if state.pending.is_empty() && !state.frames.is_empty() {
                    let sample = state.frames.pop_front().expect("checked");
                    DevicePdu::SampleResponse { stream_index, sample }
                } else {
                    state.pending.push_back((channel_id, stream_index));
                    return None;
                }
            }
            other => {
                warn!(?other, "Unexpected camera PDU from the host");
                DevicePdu::ErrorResponse(ErrorCode::InvalidMessage)
            }
        };
        Some(response)
    }

    fn opened(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.channels += 1;
        }
    }

    fn closed(&self, channel_id: u32) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.pending.retain(|&(id, _)| id != channel_id);
        state.channels = state.channels.saturating_sub(1);
        // The last channel gone with a stream still running: the host is done with the camera.
        if state.channels == 0 && Self::stop(&mut state) {
            drop(state);
            self.send(CameraMessage::Stop);
        }
    }
}

fn media_types() -> Vec<MediaType> {
    SIZES
        .iter()
        .map(|&(width, height)| MediaType {
            format: MediaFormat::H264,
            width,
            height,
            frame_rate_numerator: 30,
            frame_rate_denominator: 1,
            pixel_aspect_ratio_numerator: 1,
            pixel_aspect_ratio_denominator: 1,
            flags: MediaType::DECODING_REQUIRED,
        })
        .collect()
}

/// One of the camera's channels.
pub(crate) struct CameraChannel {
    name: String,
    feed: CameraFeed,
    channel_id: Option<u32>,
}

impl_as_any!(CameraChannel);

impl DvcProcessor for CameraChannel {
    fn channel_name(&self) -> &str {
        &self.name
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        debug!(channel_id, "Camera channel opened");
        self.channel_id = Some(channel_id);
        self.feed.opened();
        Ok(Vec::new())
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let request = match decode::<DevicePdu>(payload) {
            Ok(request) => request,
            Err(error) => {
                warn!(%error, "Malformed camera PDU from the host");
                return Ok(vec![Box::new(DevicePdu::ErrorResponse(ErrorCode::InvalidMessage))]);
            }
        };
        if !matches!(request, DevicePdu::SampleRequest(_)) {
            debug!(channel_id, ?request, "Camera request");
        }
        Ok(self
            .feed
            .respond(channel_id, request)
            .map(|response| {
                let message: DvcMessage = Box::new(response);
                vec![message]
            })
            .unwrap_or_default())
    }

    fn close(&mut self, channel_id: u32) {
        debug!(channel_id, "Camera channel closed");
        if self.channel_id.take().is_some() {
            self.feed.closed(channel_id);
        }
    }
}

impl DvcClientProcessor for CameraChannel {}

/// Hands out a channel processor on every open of the camera's channel name, any number of times.
pub(crate) struct CameraListener {
    device: RedirectedDevice,
    feed: CameraFeed,
}

impl CameraListener {
    pub(crate) fn new(device: RedirectedDevice, feed: CameraFeed) -> Self {
        Self { device, feed }
    }
}

impl DvcChannelListener for CameraListener {
    fn channel_name(&self) -> &str {
        &self.device.descriptor().channel_name
    }

    fn create(&mut self, _channel_id: u32) -> Option<Box<dyn DvcClientProcessor>> {
        Some(Box::new(CameraChannel {
            name: self.device.descriptor().channel_name.clone(),
            feed: self.feed.clone(),
            channel_id: None,
        }))
    }
}

/// Event-loop side: the page's callbacks.
#[derive(Debug, Clone)]
pub(crate) struct JsCameraCallbacks {
    /// `function(width: number, height: number, fps: number): void`
    on_start: js_sys::Function,
    /// `function(): void`
    on_stop: Option<js_sys::Function>,
    /// `function(): void` -- send a key frame next.
    on_keyframe: Option<js_sys::Function>,
    /// The camera's name on the host.
    pub(crate) name: String,
}

impl JsCameraCallbacks {
    pub(crate) fn handle(&self, message: CameraMessage) {
        let result = match message {
            CameraMessage::Start { width, height, fps } => self.on_start.apply(
                &JsValue::NULL,
                &js_sys::Array::of3(
                    &JsValue::from_f64(f64::from(width)),
                    &JsValue::from_f64(f64::from(height)),
                    &JsValue::from_f64(f64::from(fps)),
                ),
            ),
            CameraMessage::Stop => self
                .on_stop
                .as_ref()
                .map_or(Ok(JsValue::NULL), |f| f.call0(&JsValue::NULL)),
            CameraMessage::Keyframe => self
                .on_keyframe
                .as_ref()
                .map_or(Ok(JsValue::NULL), |f| f.call0(&JsValue::NULL)),
        };
        if let Err(error) = result {
            error!(?error, "Camera callback failed");
        }
    }
}

pub(crate) fn parse_camera_callbacks(value: JsValue) -> Result<JsCameraCallbacks, &'static str> {
    use wasm_bindgen::JsCast as _;
    let get = |key: &str| js_sys::Reflect::get(&value, &JsValue::from_str(key)).ok();
    let func = |key: &str| get(key).and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    Ok(JsCameraCallbacks {
        on_start: func("onStart").ok_or("camera_callbacks requires an onStart function")?,
        on_stop: func("onStop"),
        on_keyframe: func("onKeyframe"),
        name: get("name")
            .and_then(|v| v.as_string())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Remote camera".to_owned()),
    })
}
