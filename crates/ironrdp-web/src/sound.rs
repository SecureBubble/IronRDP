//! Browser-side audio-output backend for RDPSND (MS-RDPEA).
//!
//! Architecture mirrors the clipboard ([`crate::clipboard`]) and printer
//! ([`crate::printer`]) backends:
//!
//! * [`WasmSoundBackend`] lives on the RDPSND SVC processor side and implements
//!   [`ironrdp::rdpsnd::client::RdpsndClientHandler`]. It is `Send` (required by
//!   the trait) and holds only the advertised formats plus an mpsc proxy — no JS
//!   callbacks.
//! * [`WasmSound`] lives in the session event loop and owns the `js_sys::Function`
//!   callback. Decoded PCM frames flow from the backend to the event loop via
//!   [`SoundBackendMessage`].
//!
//! We advertise a **single PCM format** (44.1 kHz, stereo, 16-bit signed). The
//! server transcodes whatever the application produces down to that format, so no
//! audio codec is needed in WASM. Advertising exactly one format also keeps the
//! RDPSND `wFormatNo` deterministic (always index 0), sidestepping the ordering
//! ambiguity that a multi-format `HashSet` intersection would introduce.
//!
//! The raw PCM bytes are handed to JS, which feeds them to the Web Audio API
//! (`AudioContext`) for playback on the main thread.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use futures_channel::mpsc;
use ironrdp::rdpsnd::client::RdpsndClientHandler;
use ironrdp::rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use tracing::{error, trace, warn};
use wasm_bindgen::prelude::*;

use crate::session::RdpInputEvent;

/// PCM playback format we advertise to the server: 44.1 kHz, stereo, 16-bit.
const PLAYBACK_SAMPLE_RATE: u32 = 44_100;
const PLAYBACK_CHANNELS: u16 = 2;
const PLAYBACK_BITS_PER_SAMPLE: u16 = 16;

/// Upper bound on PCM bytes allowed to sit in the event queue waiting for JS.
/// Audio arrives continuously; if the main thread stalls we drop wave chunks
/// (an audible glitch) rather than let the queue grow without bound. 16 MiB of
/// 44.1 kHz/stereo/16-bit PCM is roughly 95 seconds of buffered audio.
const MAX_QUEUED_AUDIO_BYTES: usize = 16 * 1024 * 1024;

/// Messages sent from the sound backend to the session event loop.
#[derive(Debug)]
pub(crate) enum SoundBackendMessage {
    /// A chunk of raw PCM produced by the server-selected format.
    Wave {
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        data: Vec<u8>,
        _queued_bytes: QueuedAudioBytes,
    },
    /// The server closed the audio stream.
    Close,
}

/// RAII guard that releases its reservation from the shared queued-bytes budget
/// when the [`SoundBackendMessage::Wave`] carrying it is dropped by the event loop.
pub(crate) struct QueuedAudioBytes {
    len: usize,
    queued_bytes: Arc<AtomicUsize>,
}

impl Drop for QueuedAudioBytes {
    fn drop(&mut self) {
        self.queued_bytes.fetch_sub(self.len, Ordering::AcqRel);
    }
}

impl fmt::Debug for QueuedAudioBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuedAudioBytes")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// mpsc proxy used by the backend to stream PCM to the event loop.
#[derive(Debug, Clone)]
pub(crate) struct WasmSoundMessageProxy {
    tx: mpsc::UnboundedSender<RdpInputEvent>,
    queued_bytes: Arc<AtomicUsize>,
    queued_bytes_limit: usize,
}

impl WasmSoundMessageProxy {
    pub(crate) fn new(tx: mpsc::UnboundedSender<RdpInputEvent>) -> Self {
        Self::new_with_limit(tx, MAX_QUEUED_AUDIO_BYTES)
    }

    fn new_with_limit(tx: mpsc::UnboundedSender<RdpInputEvent>, queued_bytes_limit: usize) -> Self {
        Self {
            tx,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            queued_bytes_limit,
        }
    }

    fn send_wave(&self, format: &AudioFormat, data: Vec<u8>) {
        let Some(queued_bytes) = self.reserve_queue_capacity(data.len()) else {
            warn!(
                bytes = data.len(),
                limit = self.queued_bytes_limit,
                "Audio chunk exceeds queued playback budget; dropping (playback will glitch)"
            );
            return;
        };

        self.send_message(SoundBackendMessage::Wave {
            sample_rate: format.n_samples_per_sec,
            channels: format.n_channels,
            bits_per_sample: format.bits_per_sample,
            data,
            _queued_bytes: queued_bytes,
        });
    }

    fn send_close(&self) {
        self.send_message(SoundBackendMessage::Close);
    }

    fn send_message(&self, message: SoundBackendMessage) {
        if self.tx.unbounded_send(RdpInputEvent::Sound(message)).is_err() {
            error!("Failed to queue sound backend message, event loop receiver is closed");
        }
    }

    fn reserve_queue_capacity(&self, len: usize) -> Option<QueuedAudioBytes> {
        let mut queued = self.queued_bytes.load(Ordering::Acquire);
        loop {
            let next = queued.checked_add(len)?;
            if next > self.queued_bytes_limit {
                return None;
            }

            match self
                .queued_bytes
                .compare_exchange_weak(queued, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(QueuedAudioBytes {
                        len,
                        queued_bytes: Arc::clone(&self.queued_bytes),
                    });
                }
                Err(actual) => queued = actual,
            }
        }
    }
}

/// RDPSND client handler that streams server audio to the session event loop.
///
/// Advertises a single PCM format and forwards each received wave chunk verbatim;
/// all decoding/resampling is delegated to the server (which transcodes to PCM)
/// and the browser's Web Audio API (which resamples to the output device rate).
///
/// `Clone` is derived so one logical audio sink can back both the static
/// "rdpsnd" SVC and the "AUDIO_PLAYBACK_DVC" DVC simultaneously: the clone shares
/// the same `WasmSoundMessageProxy` (hence the same mpsc sender and the same
/// `Arc<AtomicUsize>` queued-bytes budget), so whichever transport the server
/// actually uses funnels PCM into the one event-loop `WasmSound`.
#[derive(Debug, Clone)]
pub(crate) struct WasmSoundBackend {
    /// Formats advertised to the server. A single PCM entry, so `wFormatNo` from
    /// the server's Wave2 PDU is always `0`.
    formats: Vec<AudioFormat>,
    proxy: WasmSoundMessageProxy,
}

impl WasmSoundBackend {
    pub(crate) fn new(proxy: WasmSoundMessageProxy) -> Self {
        let n_block_align = PLAYBACK_CHANNELS * (PLAYBACK_BITS_PER_SAMPLE / 8);
        let formats = vec![AudioFormat {
            format: WaveFormat::PCM,
            n_channels: PLAYBACK_CHANNELS,
            n_samples_per_sec: PLAYBACK_SAMPLE_RATE,
            n_avg_bytes_per_sec: PLAYBACK_SAMPLE_RATE * u32::from(n_block_align),
            n_block_align,
            bits_per_sample: PLAYBACK_BITS_PER_SAMPLE,
            data: None,
        }];

        Self { formats, proxy }
    }
}

impl RdpsndClientHandler for WasmSoundBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, format_no: usize, ts: u32, data: Cow<'_, [u8]>) {
        let Some(format) = self.formats.get(format_no) else {
            warn!(format_no, "RDPSND wave for unknown format index; dropping chunk");
            return;
        };

        trace!(format_no, ts, bytes = data.len(), "RDPSND wave chunk");
        self.proxy.send_wave(format, data.into_owned());
    }

    fn set_volume(&mut self, volume: VolumePdu) {
        // Volume/pitch are applied server-side (we do not advertise the VOLUME or
        // PITCH format flags), so there is nothing to do client-side here.
        trace!(?volume, "RDPSND set_volume (ignored; no client-side mixing)");
    }

    fn set_pitch(&mut self, pitch: PitchPdu) {
        trace!(?pitch, "RDPSND set_pitch (ignored; no client-side mixing)");
    }

    fn close(&mut self) {
        trace!("RDPSND close");
        self.proxy.send_close();
    }
}

/// Event-loop-side companion to [`WasmSoundBackend`]. Owns the `js_sys::Function`
/// callbacks (`!Send`, so they live here, not in the backend). The session event
/// loop forwards every [`SoundBackendMessage`] into [`WasmSound::process_message`].
#[derive(Debug)]
pub(crate) struct WasmSound {
    callbacks: JsSoundCallbacks,
}

#[derive(Debug, Clone)]
pub(crate) struct JsSoundCallbacks {
    /// Required `function(sampleRate: number, channels: number, bitsPerSample: number, pcm: Uint8Array): void`.
    pub(crate) on_wave: js_sys::Function,
    /// Optional `function(): void`, called when the server closes the stream.
    pub(crate) on_close: Option<js_sys::Function>,
}

impl WasmSound {
    pub(crate) fn new(callbacks: JsSoundCallbacks) -> Self {
        Self { callbacks }
    }

    pub(crate) fn process_message(&self, message: SoundBackendMessage) {
        let this = JsValue::NULL;
        match message {
            SoundBackendMessage::Wave {
                sample_rate,
                channels,
                bits_per_sample,
                data,
                _queued_bytes: _,
            } => {
                let pcm = js_sys::Uint8Array::from(data.as_slice());
                let args = js_sys::Array::of4(
                    &JsValue::from_f64(f64::from(sample_rate)),
                    &JsValue::from_f64(f64::from(channels)),
                    &JsValue::from_f64(f64::from(bits_per_sample)),
                    &pcm,
                );
                if let Err(err) = self.callbacks.on_wave.apply(&this, &args) {
                    error!(?err, "on_wave JS callback threw");
                }
            }
            SoundBackendMessage::Close => {
                if let Some(on_close) = &self.callbacks.on_close {
                    if let Err(err) = on_close.call0(&this) {
                        error!(?err, "on_close JS callback threw");
                    }
                }
            }
        }
    }
}

/// Factory used by [`crate::session::SessionBuilder::connect`] to build a matched
/// (backend, event-loop) pair from a single mpsc channel.
pub(crate) fn wasm_sound_pair(
    input_events_tx: mpsc::UnboundedSender<RdpInputEvent>,
    callbacks: JsSoundCallbacks,
) -> (WasmSoundBackend, WasmSound) {
    let proxy = WasmSoundMessageProxy::new(input_events_tx);
    let backend = WasmSoundBackend::new(proxy);
    let sound = WasmSound::new(callbacks);
    (backend, sound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound_backend() -> (WasmSoundBackend, mpsc::UnboundedReceiver<RdpInputEvent>) {
        let (tx, rx) = mpsc::unbounded();
        (WasmSoundBackend::new(WasmSoundMessageProxy::new(tx)), rx)
    }

    #[test]
    fn advertises_single_pcm_format() {
        let (backend, _rx) = sound_backend();
        let formats = backend.get_formats();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].format, WaveFormat::PCM);
        assert_eq!(formats[0].n_samples_per_sec, PLAYBACK_SAMPLE_RATE);
        assert_eq!(formats[0].n_channels, PLAYBACK_CHANNELS);
        assert_eq!(formats[0].n_block_align, 4);
        assert_eq!(formats[0].n_avg_bytes_per_sec, 176_400);
    }

    #[test]
    fn wave_forwards_pcm_with_format_params() {
        let (mut backend, mut rx) = sound_backend();
        backend.wave(0, 123, Cow::Borrowed(&[1, 2, 3, 4]));

        match rx.try_recv().unwrap() {
            Some(RdpInputEvent::Sound(SoundBackendMessage::Wave {
                sample_rate,
                channels,
                bits_per_sample,
                data,
                ..
            })) => {
                assert_eq!(sample_rate, PLAYBACK_SAMPLE_RATE);
                assert_eq!(channels, PLAYBACK_CHANNELS);
                assert_eq!(bits_per_sample, PLAYBACK_BITS_PER_SAMPLE);
                assert_eq!(data, vec![1, 2, 3, 4]);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn unknown_format_index_is_dropped() {
        let (mut backend, mut rx) = sound_backend();
        backend.wave(9, 0, Cow::Borrowed(&[1, 2]));
        assert!(rx.try_recv().unwrap().is_none());
    }

    #[test]
    fn close_forwards_close_message() {
        let (mut backend, mut rx) = sound_backend();
        backend.close();
        assert!(matches!(
            rx.try_recv().unwrap(),
            Some(RdpInputEvent::Sound(SoundBackendMessage::Close))
        ));
    }

    #[test]
    fn over_budget_wave_is_dropped() {
        let (tx, mut rx) = mpsc::unbounded();
        let mut backend = WasmSoundBackend::new(WasmSoundMessageProxy::new_with_limit(tx, 4));
        backend.wave(0, 0, Cow::Borrowed(&[1, 2, 3, 4])); // exactly at budget
        assert!(matches!(
            rx.try_recv().unwrap(),
            Some(RdpInputEvent::Sound(SoundBackendMessage::Wave { .. }))
        ));
        // The first chunk is still queued (not yet dropped by the event loop), so
        // a second chunk exceeds the 4-byte budget and is dropped.
        backend.wave(0, 0, Cow::Borrowed(&[5, 6, 7, 8]));
        assert!(rx.try_recv().unwrap().is_none());
    }
}
