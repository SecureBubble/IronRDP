//! Browser-side microphone backend for AUDIO_INPUT (MS-RDPEAI).
//!
//! Same split as the sound backend ([`crate::sound`]):
//!
//! * [`WasmMicBackend`] lives in the AUDIO_INPUT DVC processor and implements
//!   [`RdpeaiCaptureHandler`]. It is `Send` and holds no JS callbacks: when the host opens the
//!   microphone it parks the capture sink in the shared [`MicInput`] and tells the event loop.
//! * [`WasmMic`] lives in the session event loop and owns the JS callbacks. `onOpen` asks the page
//!   to start capturing (`getUserMedia`) in the format the host chose; `onClose` stops it.
//! * The page pushes captured PCM back with the `mic_data` extension, which lands in
//!   [`MicInput::push`]: it is cut into the packet size the host asked for and sent as Data PDUs.
//!
//! Only 16-bit PCM is offered, so no audio codec is needed in WASM; the page resamples to the
//! rate the host picked.
//!
//! The channel is registered as a re-createable listener ([`MicListener`]): Windows opens and
//! closes AUDIO_INPUT every time an application on the host grabs and releases the microphone.

use std::sync::{Arc, Mutex};

use futures_channel::mpsc;
use ironrdp::dvc::{DvcChannelListener, DvcClientProcessor, DynamicChannelId};
use ironrdp::rdpsnd::pdu::{AudioFormat, WaveFormat};
use ironrdp::svc::SvcMessage;
use ironrdp_rdpeai::client::{AudioPacketSink, RdpeaiCaptureHandler, RdpeaiClient};
use ironrdp_rdpeai::pdu::{MAX_DATA_PACKET_SIZE, OpenReplyPdu, pcm_format};
use tracing::{debug, error, warn};
use wasm_bindgen::JsValue;

use crate::session::RdpInputEvent;

/// Messages from the AUDIO_INPUT processor to the session event loop.
#[derive(Debug)]
pub(crate) enum MicMessage {
    /// The host opened the microphone: start capturing in this format.
    Open {
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
    },
    /// The host released the microphone.
    Close,
    /// Encoded Data PDUs (already wrapped for DRDYNVC) to send to the host. In a `Mutex` only so
    /// the event type stays `Sync` (`SvcMessage` is not).
    Uplink(Mutex<Vec<SvcMessage>>),
}

struct Capture {
    sink: AudioPacketSink,
    packet_size: usize,
    buffer: Vec<u8>,
}

/// The open capture, shared between the AUDIO_INPUT processor (which opens and closes it) and the
/// session (which feeds it PCM from the page).
#[derive(Clone, Default)]
pub(crate) struct MicInput(Arc<Mutex<Option<Capture>>>);

impl MicInput {
    /// PCM from the page, in the format of the last `onOpen`. Sent as whole packets; a remainder
    /// waits for the next push. Ignored while the host has the microphone closed.
    pub(crate) fn push(&self, pcm: &[u8]) {
        let Ok(mut guard) = self.0.lock() else {
            return;
        };
        let Some(capture) = guard.as_mut() else {
            return;
        };
        capture.buffer.extend_from_slice(pcm);
        while capture.buffer.len() >= capture.packet_size {
            let packet: Vec<u8> = capture.buffer.drain(..capture.packet_size).collect();
            (capture.sink)(packet);
        }
        // A page pushing far faster than real time would only grow latency; keep at most a few
        // packets queued.
        let limit = capture.packet_size * 8;
        if capture.buffer.len() > limit {
            let excess = capture.buffer.len() - limit;
            capture.buffer.drain(..excess);
        }
    }

    fn set(&self, capture: Option<Capture>) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = capture;
        }
    }
}

fn send(tx: &mpsc::UnboundedSender<RdpInputEvent>, message: MicMessage) {
    if tx.unbounded_send(RdpInputEvent::Mic(message)).is_err() {
        error!("Failed to queue microphone message, event loop receiver is closed");
    }
}

fn is_pcm16(format: &AudioFormat) -> bool {
    format.format == WaveFormat::PCM && format.bits_per_sample == 16
}

/// Capture handler for one incarnation of the AUDIO_INPUT channel.
struct WasmMicBackend {
    formats: Vec<AudioFormat>,
    input: MicInput,
    tx: mpsc::UnboundedSender<RdpInputEvent>,
    open_format: Option<AudioFormat>,
}

impl RdpeaiCaptureHandler for WasmMicBackend {
    fn supported_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn open(
        &mut self,
        capture_format: &AudioFormat,
        encode_format: &AudioFormat,
        packet_size: usize,
        sink: AudioPacketSink,
    ) -> i32 {
        // PCM in, PCM out: the Data PDUs carry exactly what the page captures.
        let same = capture_format.n_channels == encode_format.n_channels
            && capture_format.n_samples_per_sec == encode_format.n_samples_per_sec;
        if !is_pcm16(encode_format) || !same {
            warn!(
                ?capture_format,
                ?encode_format,
                "Refusing microphone open: not one 16-bit PCM format"
            );
            return OpenReplyPdu::E_FAIL;
        }
        if packet_size == 0 || packet_size > MAX_DATA_PACKET_SIZE {
            warn!(packet_size, "Refusing microphone open with invalid packet size");
            return OpenReplyPdu::E_FAIL;
        }
        debug!(?encode_format, packet_size, "Microphone opened by the host");
        self.input.set(Some(Capture {
            sink,
            packet_size,
            buffer: Vec::with_capacity(packet_size * 2),
        }));
        self.open_format = Some(encode_format.clone());
        send(
            &self.tx,
            MicMessage::Open {
                sample_rate: encode_format.n_samples_per_sec,
                channels: encode_format.n_channels,
                bits_per_sample: encode_format.bits_per_sample,
            },
        );
        OpenReplyPdu::S_OK
    }

    fn set_format(&mut self, encode_format: &AudioFormat, _packet_size: usize) -> bool {
        // The page captures one format per open; accept a "change" only to that same format.
        self.open_format.as_ref().is_some_and(|open| {
            open.n_channels == encode_format.n_channels
                && open.n_samples_per_sec == encode_format.n_samples_per_sec
                && is_pcm16(encode_format)
        })
    }

    fn close(&mut self) {
        if self.open_format.take().is_some() {
            debug!("Microphone closed by the host");
            self.input.set(None);
            send(&self.tx, MicMessage::Close);
        }
    }
}

impl Drop for WasmMicBackend {
    fn drop(&mut self) {
        self.close();
    }
}

/// Hands out a fresh AUDIO_INPUT processor on every channel create.
pub(crate) struct MicListener {
    input: MicInput,
    tx: mpsc::UnboundedSender<RdpInputEvent>,
}

impl MicListener {
    pub(crate) fn new(input: MicInput, tx: mpsc::UnboundedSender<RdpInputEvent>) -> Self {
        Self { input, tx }
    }
}

impl DvcChannelListener for MicListener {
    fn channel_name(&self) -> &str {
        ironrdp_rdpeai::CHANNEL_NAME
    }

    fn create(&mut self, _channel_id: DynamicChannelId) -> Option<Box<dyn DvcClientProcessor>> {
        let backend = WasmMicBackend {
            // Common voice and Windows rates, mono first; the page converts to whichever the
            // host picks.
            formats: vec![
                pcm_format(1, 48000, 16),
                pcm_format(1, 44100, 16),
                pcm_format(1, 22050, 16),
                pcm_format(1, 16000, 16),
                pcm_format(2, 48000, 16),
                pcm_format(2, 44100, 16),
            ],
            input: self.input.clone(),
            tx: self.tx.clone(),
            open_format: None,
        };
        let tx = self.tx.clone();
        Some(Box::new(RdpeaiClient::new(
            Box::new(backend),
            Box::new(move |_channel_id, messages| {
                send(&tx, MicMessage::Uplink(Mutex::new(messages)));
                Ok(())
            }),
        )))
    }
}

/// Event-loop side: owns the page's callbacks.
#[derive(Debug, Clone)]
pub(crate) struct JsMicCallbacks {
    /// `function(sampleRate: number, channels: number, bitsPerSample: number): void`
    pub(crate) on_open: js_sys::Function,
    /// `function(): void`
    pub(crate) on_close: Option<js_sys::Function>,
}

impl JsMicCallbacks {
    pub(crate) fn open(&self, sample_rate: u32, channels: u16, bits_per_sample: u16) {
        let args = js_sys::Array::of3(
            &JsValue::from_f64(f64::from(sample_rate)),
            &JsValue::from_f64(f64::from(channels)),
            &JsValue::from_f64(f64::from(bits_per_sample)),
        );
        if let Err(error) = self.on_open.apply(&JsValue::NULL, &args) {
            error!(?error, "Microphone onOpen callback failed");
        }
    }

    pub(crate) fn close(&self) {
        if let Some(on_close) = &self.on_close {
            if let Err(error) = on_close.call0(&JsValue::NULL) {
                error!(?error, "Microphone onClose callback failed");
            }
        }
    }
}

pub(crate) fn parse_mic_callbacks(value: JsValue) -> Result<JsMicCallbacks, &'static str> {
    use wasm_bindgen::JsCast as _;
    let get = |key: &str| js_sys::Reflect::get(&value, &JsValue::from_str(key)).ok();
    let on_open = get("onOpen")
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or("mic_callbacks requires an onOpen function")?;
    let on_close = get("onClose").and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    Ok(JsMicCallbacks { on_open, on_close })
}
