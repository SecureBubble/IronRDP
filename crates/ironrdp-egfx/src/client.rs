//! Client-side EGFX implementation
//!
//! This module provides client-side support for the Graphics Pipeline Extension
//! ([MS-RDPEGFX]), including H.264 AVC420 decode and surface management.
//!
//! # Protocol Compliance
//!
//! This implementation follows MS-RDPEGFX client requirements:
//!
//! - **Capability Negotiation**: Advertises V8 through V10.7 ([2.2.3])
//! - **Surface Management**: Tracks server-created surfaces ([3.3.1.6])
//! - **Frame Acknowledgment**: Sends `FrameAcknowledge` after `EndFrame` ([3.3.5.12])
//! - **Codec Dispatch**: Routes `WireToSurface1` by `codec_id` ([3.3.5.2])
//!
//! # Architecture
//!
//! ```text
//! Server                                  Client
//!    |                                       |
//!    |--- CapabilitiesConfirm -------------->|
//!    |--- ResetGraphics -------------------->|
//!    |--- CreateSurface -------------------->|
//!    |--- MapSurfaceToOutput --------------->|
//!    |                                       |
//!    |  (For each frame:)                    |
//!    |--- StartFrame ----------------------->|
//!    |--- WireToSurface1 (H.264) ----------->|  -> H264Decoder::decode()
//!    |--- EndFrame ------------------------->|  -> FrameAcknowledge
//!    |                                       |
//!    |<---------- FrameAcknowledge ----------|
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler, BitmapUpdate};
//! use ironrdp_egfx::decode::H264Decoder;
//!
//! struct MyHandler;
//!
//! impl GraphicsPipelineHandler for MyHandler {
//!     fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
//!         // Render decoded bitmap to screen
//!     }
//! }
//!
//! let client = GraphicsPipelineClient::new(Box::new(MyHandler), None);
//! ```
//!
//! [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0
//! [2.2.3]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/b5e09f90-6dde-47ca-8ec1-7dcdd5dc70b0
//! [3.3.1.6]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/83cb08ff-c97f-4d08-b834-7aa69cdea6c5
//! [3.3.5.2]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/90aba3e3-d4a8-4af1-b1bb-a94e2313bbf0
//! [3.3.5.12]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/e3c80bff-3e4e-4e65-b7c2-c2cd6b1fb4f5

use std::collections::BTreeMap;

use ironrdp_core::{Decode as _, ReadCursor, impl_as_any};
use ironrdp_dvc::{DvcClientProcessor, DvcMessage, DvcProcessor};
use ironrdp_graphics::clearcodec::ClearCodecDecoder;
use ironrdp_graphics::progressive::ProgressiveDecoder;
use ironrdp_graphics::rdp6::BitmapStreamDecoder;
use ironrdp_graphics::zgfx;
use ironrdp_pdu::geometry::{ExclusiveRectangle, InclusiveRectangle, Rectangle as _};
use ironrdp_pdu::{PduResult, decode_cursor, decode_err, pdu_other_err};
use tracing::{debug, trace, warn};

use crate::CHANNEL_NAME;
use crate::decode::H264Decoder;
use crate::pdu::{
    Avc420BitmapStream, Avc444BitmapStream, Encoding, CacheImportReplyPdu, CacheToSurfacePdu, CapabilitiesAdvertisePdu, CapabilitiesV8Flags,
    CapabilitiesV81Flags, CapabilitiesV107Flags, CapabilitySet, CapabilityVersion, Codec1Type, DeleteEncodingContextPdu,
    EvictCacheEntryPdu, FrameAcknowledgePdu, GfxPdu, MapSurfaceToScaledOutputPdu, MapSurfaceToScaledWindowPdu,
    MapSurfaceToWindowPdu, PixelFormat, ProtectSurfacePdu, QueueDepth, RawCapabilitySet, SolidFillPdu,
    SurfaceToCachePdu, SurfaceToSurfacePdu, WatermarkPdu, WireToSurface2Pdu,
};

/// Max capacity to keep for decompressed buffer when cleared.
const MAX_DECOMPRESSED_BUFFER_CAPACITY: usize = 16384; // 16 KiB

// ============================================================================
// Surface Management
// ============================================================================

/// Client-side surface state
///
/// Per [MS-RDPEGFX 3.3.1.6], the client maintains an "Offscreen Surfaces
/// ADM element" tracking surfaces created by the server.
///
/// [MS-RDPEGFX 3.3.1.6]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/83cb08ff-c97f-4d08-b834-7aa69cdea6c5
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Surface {
    /// Surface identifier (assigned by server)
    pub id: u16,
    /// Surface width in pixels
    pub width: u16,
    /// Surface height in pixels
    pub height: u16,
    /// Pixel format
    pub pixel_format: PixelFormat,
    /// Whether this surface is mapped to an output
    pub is_mapped: bool,
    /// Output X origin (if mapped)
    pub output_origin_x: u32,
    /// Output Y origin (if mapped)
    pub output_origin_y: u32,
}

// ============================================================================
// Codec Capabilities
// ============================================================================

/// Codec capabilities determined from negotiated capability set
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct CodecCapabilities {
    /// AVC420 (H.264 4:2:0) is available
    pub avc420: bool,
    /// AVC444 (H.264 4:4:4) is available
    pub avc444: bool,
    /// Small cache mode
    pub small_cache: bool,
    /// Thin client mode
    pub thin_client: bool,
}

impl CodecCapabilities {
    fn from_capability_set(cap: &CapabilitySet) -> Self {
        // Mirrors the server-side extraction logic
        match cap {
            CapabilitySet::V8 { flags } => Self {
                avc420: false,
                avc444: false,
                small_cache: flags.contains(CapabilitiesV8Flags::SMALL_CACHE),
                thin_client: flags.contains(CapabilitiesV8Flags::THIN_CLIENT),
            },
            CapabilitySet::V8_1 { flags } => Self {
                avc420: flags.contains(CapabilitiesV81Flags::AVC420_ENABLED),
                avc444: false,
                small_cache: flags.contains(CapabilitiesV81Flags::SMALL_CACHE),
                thin_client: flags.contains(CapabilitiesV81Flags::THIN_CLIENT),
            },
            CapabilitySet::V10 { flags } | CapabilitySet::V10_2 { flags } => Self {
                avc420: !flags.contains(crate::pdu::CapabilitiesV10Flags::AVC_DISABLED),
                avc444: !flags.contains(crate::pdu::CapabilitiesV10Flags::AVC_DISABLED),
                small_cache: flags.contains(crate::pdu::CapabilitiesV10Flags::SMALL_CACHE),
                thin_client: false,
            },
            CapabilitySet::V10_1 => Self {
                avc420: true,
                avc444: true,
                small_cache: false,
                thin_client: false,
            },
            CapabilitySet::V10_3 { flags } => Self {
                avc420: !flags.contains(crate::pdu::CapabilitiesV103Flags::AVC_DISABLED),
                avc444: !flags.contains(crate::pdu::CapabilitiesV103Flags::AVC_DISABLED),
                small_cache: false,
                thin_client: flags.contains(crate::pdu::CapabilitiesV103Flags::AVC_THIN_CLIENT),
            },
            CapabilitySet::V10_4 { flags }
            | CapabilitySet::V10_5 { flags }
            | CapabilitySet::V10_6 { flags }
            | CapabilitySet::V10_6Err { flags } => Self {
                avc420: !flags.contains(crate::pdu::CapabilitiesV104Flags::AVC_DISABLED),
                avc444: !flags.contains(crate::pdu::CapabilitiesV104Flags::AVC_DISABLED),
                small_cache: flags.contains(crate::pdu::CapabilitiesV104Flags::SMALL_CACHE),
                thin_client: flags.contains(crate::pdu::CapabilitiesV104Flags::AVC_THIN_CLIENT),
            },
            CapabilitySet::V10_7 { flags } | CapabilitySet::V10_8 { flags } | CapabilitySet::V10_9 { flags } => {
                Self {
                    avc420: !flags.contains(CapabilitiesV107Flags::AVC_DISABLED),
                    avc444: !flags.contains(CapabilitiesV107Flags::AVC_DISABLED),
                    small_cache: flags.contains(CapabilitiesV107Flags::SMALL_CACHE),
                    thin_client: flags.contains(CapabilitiesV107Flags::AVC_THIN_CLIENT),
                }
            }
        }
    }
}

// ============================================================================
// Bitmap Update
// ============================================================================

/// Decoded bitmap data for a surface region
///
/// Delivered to [`GraphicsPipelineHandler::on_bitmap_updated`] when
/// a `WireToSurface1` PDU is processed with decoded pixel data.
#[derive(Debug)]
#[non_exhaustive]
pub struct BitmapUpdate {
    /// Surface this update applies to
    pub surface_id: u16,
    /// Destination rectangle within the surface (exclusive `right`/`bottom`)
    pub destination_rectangle: ExclusiveRectangle,
    /// Codec that produced this update
    pub codec_id: Codec1Type,
    /// RGBA pixel data (4 bytes per pixel), row-major
    ///
    /// Dimensions match `width * height * 4` bytes.
    /// May be empty if decode was skipped (no decoder configured).
    pub data: Vec<u8>,
    /// Width of the decoded data in pixels
    pub width: u16,
    /// Height of the decoded data in pixels
    pub height: u16,
}

/// A raw AVC (H.264) frame handed off for out-of-band decoding.
///
/// Unlike [`BitmapUpdate`], the client does NOT decode H.264 itself when no Rust
/// [`H264Decoder`] is attached — it forwards the compressed main sub-stream to an
/// out-of-band decoder (the browser WebCodecs `VideoDecoder`), which returns RGBA
/// asynchronously and composites it via the normal region path. Delivered to
/// [`GraphicsPipelineHandler::on_avc_frame`].
///
/// MVP scope: only the main (`stream1`) YUV420 picture is forwarded — decoding it
/// alone yields a full-color 4:2:0 image. The auxiliary (4:4:4 chroma) sub-stream is
/// layered on in a later milestone (see the AVC444v2 recombination work).
#[derive(Debug)]
#[non_exhaustive]
pub struct AvcFrame<'a> {
    /// Surface this frame targets.
    pub surface_id: u16,
    /// The eGFX frame this AVC picture belongs to. The out-of-band decoder echoes it
    /// back on decode-completion so the client can send the deferred `FrameAcknowledge` for
    /// it (flow control — see [`GraphicsPipelineClient::build_frame_ack`]).
    pub frame_id: u32,
    /// Which AVC codec produced it (`Avc444` = 0x0E or `Avc444v2` = 0x0F).
    pub codec_id: Codec1Type,
    /// Destination rectangle within the surface (exclusive `right`/`bottom`).
    pub destination_rectangle: ExclusiveRectangle,
    /// The main (`stream1`) H.264 bitstream in AVC format (4-byte big-endian
    /// length-prefixed NAL units, NOT Annex B). A complete YUV420 picture.
    pub main_stream: &'a [u8],
    /// Metablock region rectangles: the dirty sub-rects updated within this frame.
    pub regions: &'a [InclusiveRectangle],
}

// ============================================================================
// Handler Trait
// ============================================================================

/// Handler trait for client-side EGFX events
///
/// Implement this trait to receive decoded bitmap data and surface
/// lifecycle notifications from the EGFX pipeline.
///
/// All methods have default no-op implementations so you only need
/// to override the ones relevant to your use case.
pub trait GraphicsPipelineHandler: Send {
    /// Returns the capability sets to advertise to the server
    ///
    /// The default advertises V10.7 (AVC420+AVC444), V8.1 (AVC420 only),
    /// and V8 (no AVC) as fallback.
    ///
    /// Note: AVC-capable versions are automatically filtered out at
    /// advertisement time if no H.264 decoder is configured on the
    /// [`GraphicsPipelineClient`]. If all returned sets require AVC
    /// and no decoder is available, a V8-only fallback is used.
    fn capabilities(&self) -> Vec<CapabilitySet> {
        vec![
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::SMALL_CACHE,
            },
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE,
            },
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::SMALL_CACHE,
            },
        ]
    }

    /// Called when the server confirms negotiated capabilities
    fn on_capabilities_confirmed(&mut self, _caps: &CapabilitySet) {}

    /// Called when the server resets the graphics output buffer
    fn on_reset_graphics(&mut self, _width: u32, _height: u32) {}

    /// Called when a surface is created by the server
    fn on_surface_created(&mut self, _surface: &Surface) {}

    /// Called when a surface is deleted by the server
    fn on_surface_deleted(&mut self, _surface_id: u16) {}

    /// Called when a surface is mapped to an output position
    fn on_surface_mapped(&mut self, _surface_id: u16, _origin_x: u32, _origin_y: u32) {}

    /// Called when decoded bitmap data is available for a surface
    ///
    /// This is the primary output path. The `update` contains the
    /// surface ID, destination rectangle, and RGBA pixel data.
    fn on_bitmap_updated(&mut self, _update: &BitmapUpdate) {}

    /// Called when a logical frame is complete
    ///
    /// All bitmap updates between the corresponding `StartFrame`
    /// and this notification belong to the same logical frame.
    fn on_frame_complete(&mut self, _frame_id: u32) {}

    /// Called when the EGFX channel is closed
    fn on_close(&mut self) {}

    // ========================================================================
    // Additional PDU handlers (server→client)
    // ========================================================================

    /// Called when the server fills a surface region with a solid color
    ///
    /// Per [MS-RDPEGFX 3.3.5.4].
    ///
    /// [MS-RDPEGFX 3.3.5.4]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/d696ab07-fd47-42f6-a601-c8b6fae26577
    fn on_solid_fill(&mut self, _pdu: &SolidFillPdu) {}

    /// Called when the server copies pixels between surfaces
    ///
    /// Per [MS-RDPEGFX 3.3.5.5].
    ///
    /// [MS-RDPEGFX 3.3.5.5]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/0b19d058-fff0-43e5-8671-8c4186d60529
    fn on_surface_to_surface(&mut self, _pdu: &SurfaceToSurfacePdu) {}

    /// Called when the server caches a surface region
    ///
    /// Per [MS-RDPEGFX 3.3.5.6].
    ///
    /// [MS-RDPEGFX 3.3.5.6]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/01108b9f-a888-4e5c-b790-42d5c5985998
    fn on_surface_to_cache(&mut self, _pdu: &SurfaceToCachePdu) {}

    /// Called when the server renders cached content to a surface
    ///
    /// Per [MS-RDPEGFX 3.3.5.7].
    ///
    /// [MS-RDPEGFX 3.3.5.7]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/78c00bcd-f5cb-4c33-8d6c-f4cd50facfab
    fn on_cache_to_surface(&mut self, _pdu: &CacheToSurfacePdu) {}

    /// Called when the server evicts a cache entry
    ///
    /// Per [MS-RDPEGFX 3.3.5.8].
    ///
    /// [MS-RDPEGFX 3.3.5.8]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/9dd32c5c-fabc-497b-81be-776fa581a4f6
    fn on_evict_cache_entry(&mut self, _pdu: &EvictCacheEntryPdu) {}

    /// Called when the server maps a surface to a RAIL window
    ///
    /// Per [MS-RDPEGFX 2.2.2.20].
    ///
    /// [MS-RDPEGFX 2.2.2.20]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/2ec1357c-ee65-4d9b-89f3-8fc49348c92a
    fn on_map_surface_to_window(&mut self, _pdu: &MapSurfaceToWindowPdu) {}

    /// Called when the server maps a surface to a scaled output
    ///
    /// Per [MS-RDPEGFX 2.2.2.22].
    ///
    /// [MS-RDPEGFX 2.2.2.22]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/3fcc3e63-e5a2-4b18-a572-26bbeb87b3aa
    fn on_map_surface_to_scaled_output(&mut self, _pdu: &MapSurfaceToScaledOutputPdu) {}

    /// Called when the server maps a surface to a scaled RAIL window
    ///
    /// Per [MS-RDPEGFX 2.2.2.23].
    ///
    /// [MS-RDPEGFX 2.2.2.23]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/22fc0ec7-38ce-4d9d-ad6d-93a0e9f3c38c
    fn on_map_surface_to_scaled_window(&mut self, _pdu: &MapSurfaceToScaledWindowPdu) {}

    /// Called for progressive codec (RFX Progressive) bitmap data
    ///
    /// Per [MS-RDPEGFX 3.3.5.3].
    ///
    /// [MS-RDPEGFX 3.3.5.3]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/e6dbb3a7-3de0-44a5-a1ee-9de90f75e7e0
    fn on_wire_to_surface2(&mut self, _pdu: &WireToSurface2Pdu) {}

    /// Called when the server deletes a progressive encoding context
    ///
    /// Per [MS-RDPEGFX 2.2.2.3].
    ///
    /// [MS-RDPEGFX 2.2.2.3]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/bd0c64d4-07b3-47e5-9f7b-ba5c14a3a2e2
    fn on_delete_encoding_context(&mut self, _pdu: &DeleteEncodingContextPdu) {}

    /// Called when the server replies to a cache import offer
    ///
    /// Per [MS-RDPEGFX 2.2.2.17].
    ///
    /// [MS-RDPEGFX 2.2.2.17]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/7c7a0a5d-50c1-44b9-a2e7-44b47ce1e49d
    fn on_cache_import_reply(&mut self, _pdu: &CacheImportReplyPdu) {}

    /// Called when the proxy pushes a session watermark overlay
    ///
    /// Non-standard SecureBubble extension (`RDPGFX_CMDID_WATERMARK`, 0x001A). The
    /// handler should keep the ARGB8888 bitmap drawn over the mapped output at the
    /// requested opacity until a new watermark arrives.
    fn on_watermark(&mut self, _pdu: &WatermarkPdu) {}

    /// Called when the proxy flags a surface as capture-protected
    ///
    /// Non-standard SecureBubble extension (`RDPGFX_CMDID_PROTECT_SURFACE`, 0x0019).
    /// A browser client cannot truly enforce capture protection, so this is
    /// best-effort/advisory.
    fn on_protect_surface(&mut self, _pdu: &ProtectSurfacePdu) {}

    /// Called for PDUs that have no specific handler
    ///
    /// This is a catch-all for any GfxPdu variant not matched above.
    fn on_unhandled_pdu(&mut self, _pdu: &GfxPdu) {}

    /// Called for an AVC (H.264) frame that must be decoded out-of-band.
    ///
    /// The client forwards the compressed main sub-stream (it does not decode H.264
    /// itself); the handler decodes it — e.g. via the browser WebCodecs
    /// `VideoDecoder` — and composites the resulting RGBA through its normal region
    /// path. Default: no-op (AVC frames dropped).
    fn on_avc_frame(&mut self, _frame: &AvcFrame<'_>) {}
}

// ============================================================================
// Client State Machine
// ============================================================================

/// Client state machine states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    /// Waiting for server `CapabilitiesConfirm`
    WaitingForConfirm,
    /// Channel is active, processing frames
    Active,
    /// Channel has been closed
    Closed,
}

// ============================================================================
// Graphics Pipeline Client
// ============================================================================

/// Client for the Graphics Pipeline Virtual Channel (EGFX)
///
/// This client handles capability negotiation, surface tracking,
/// H.264 AVC420 decode, and frame acknowledgment per [MS-RDPEGFX].
///
/// [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/da5c75f9-cd99-450c-98c4-014a496942b0
pub struct GraphicsPipelineClient {
    handler: Box<dyn GraphicsPipelineHandler>,
    h264_decoder: Option<Box<dyn H264Decoder>>,
    /// ClearCodec decoder. Always available (pure Rust, no external decoder).
    clearcodec_decoder: ClearCodecDecoder,
    /// RFX Progressive (WireToSurface2) decoder. Pure Rust; keeps per-context
    /// tile state across frames.
    progressive_decoder: ProgressiveDecoder,
    /// RDP 6.0 Planar (`RDPGFX_CODECID_PLANAR`) decoder. Pure Rust.
    planar_decoder: BitmapStreamDecoder,
    /// When true, advertise AVC420/AVC444 capabilities (V10.x) even without a
    /// Rust `h264_decoder`. Used when H.264 is decoded out-of-band (WebCodecs in
    /// the browser). Without a Rust decoder attached, AVC frames are logged and
    /// skipped by `decode_avc420` — so this alone (Stage 0) yields blank AVC
    /// regions; it exists to confirm the server sends AVC once we advertise it.
    avc_available: bool,

    decompressor: zgfx::Decompressor,
    decompressed_buffer: Vec<u8>,

    state: ClientState,
    negotiated_caps: Option<CapabilitySet>,
    codec_caps: CodecCapabilities,

    surfaces: BTreeMap<u16, Surface>,
    current_frame_id: Option<u32>,
    frames_queued: u32,
    total_frames_decoded: u32,
    /// True if the frame currently being assembled (StartFrame..EndFrame) carried an
    /// AVC (async-decoded) picture, so its `FrameAcknowledge` must be deferred.
    current_frame_has_avc: bool,
    /// Pending FrameAcknowledges, keyed by `frame_id`. Each entry carries the
    /// `total_frames_decoded` snapshot taken at EndFrame plus whether its ack is ready to
    /// send yet (synchronous codecs: ready at EndFrame; AVC: ready only when the
    /// out-of-band decoder presents, see [`GraphicsPipelineClient::build_frame_ack`]).
    ///
    /// Emission is a **non-blocking, monotonic high-water mark**
    /// ([`GraphicsPipelineClient::flush_ready_acks`]): whenever frames become ready we emit
    /// an ack for each ready frame whose `frame_id` exceeds the last one already sent
    /// (`max_acked_frame_id`), in ascending order, and simply *retire* (drop without
    /// emitting) any ready frame whose `frame_id` is below the high-water — an out-of-order
    /// straggler already covered by the cumulative counter. Because both `frame_id` and
    /// `total_frames_decoded` only ever advance, the wire stays monotonic as MS-RDPEGFX
    /// 2.2.2.13 requires (the counter is cumulative; a decreasing value drives the server
    /// encoder into its error state).
    ///
    /// Crucially this does NOT gate on the *lowest* un-decoded frame. With two monitors,
    /// two independent browser VideoDecoders complete decode out of eGFX `frame_id` order; the
    /// old "flush only the consecutive ready run from the front" scheme head-of-line-blocked
    /// every ack behind the slowest surface's un-decoded frame, draining the server's
    /// flow-control window and stalling the whole session. High-water emission lets a fast
    /// surface's acks through while a slow surface catches up.
    pending_acks: BTreeMap<u32, PendingAck>,
    /// Lowest `frame_id` still pending (front of `pending_acks`). Cached for telemetry /
    /// the live dual-monitor repro; exposed via [`GraphicsPipelineClient::next_unacked_frame_id`].
    next_unacked_frame_id: Option<u32>,
    /// Largest `total_frames_decoded` already emitted on the wire. Emitted acks are
    /// clamped to this so the cumulative counter can never regress (MS-RDPEGFX 2.2.2.13),
    /// even across a ResetGraphics.
    max_total_acked: u32,
    /// Largest `frame_id` already emitted on the wire. A ready frame is acked only if its
    /// `frame_id` exceeds this (the high-water mark); lower-id stragglers are retired
    /// silently so the wire `frameId` never regresses. Reset on ResetGraphics because the
    /// server may restart its `frame_id` sequence for the new stream (unlike the cumulative
    /// `total_frames_decoded`, which continues).
    max_acked_frame_id: u32,
}

/// One entry in the [`GraphicsPipelineClient::pending_acks`] reorder buffer.
#[derive(Clone, Copy)]
struct PendingAck {
    /// `total_frames_decoded` snapshot at this frame's EndFrame.
    total_frames_decoded: u32,
    /// Ack is ready to send. Synchronous codecs set this at EndFrame; AVC frames set it
    /// on decode-completion in [`GraphicsPipelineClient::build_frame_ack`].
    ready: bool,
    /// Frame carried AVC (its ack was deferred until decode-completion). Purely for logging.
    avc: bool,
}

/// Memory bound on un-decoded (never-ready) entries retained in `pending_acks`. With
/// high-water emission a never-decoded AVC frame (browser decoder error — a frame whose
/// `EndFrame` registered a deferred ack but which the WebCodecs decoder never produced) no
/// longer gates flow control — the cumulative counter advances past it via later frames
/// that DO decode — but its `pending_acks` entry would otherwise linger forever. When the
/// buffer exceeds this bound we evict the oldest such entries. Set generously so it only
/// fires under genuine sustained loss. (Present-queue FIFO drops no longer leave un-acked
/// entries: the ack fires at decode, before the frame reaches the present FIFO.)
const MAX_DEFERRED_ACKS: usize = 64;

impl GraphicsPipelineClient {
    /// Create a new `GraphicsPipelineClient`
    ///
    /// If `h264_decoder` is `None`, AVC420 frames are logged and skipped.
    pub fn new(handler: Box<dyn GraphicsPipelineHandler>, h264_decoder: Option<Box<dyn H264Decoder>>) -> Self {
        Self {
            handler,
            h264_decoder,
            clearcodec_decoder: ClearCodecDecoder::new(),
            progressive_decoder: ProgressiveDecoder::new(),
            planar_decoder: BitmapStreamDecoder::default(),
            avc_available: false,
            decompressor: zgfx::Decompressor::new(),
            decompressed_buffer: Vec::new(),
            state: ClientState::WaitingForConfirm,
            negotiated_caps: None,
            codec_caps: CodecCapabilities::default(),
            surfaces: BTreeMap::new(),
            current_frame_id: None,
            frames_queued: 0,
            total_frames_decoded: 0,
            current_frame_has_avc: false,
            pending_acks: BTreeMap::new(),
            next_unacked_frame_id: None,
            max_total_acked: 0,
            max_acked_frame_id: 0,
        }
    }

    /// Advertise AVC420/AVC444 capabilities even without a Rust H.264 decoder, so
    /// H.264 can be decoded out-of-band (e.g. WebCodecs). Chainable at construction.
    #[must_use]
    pub fn advertise_avc(mut self, enable: bool) -> Self {
        self.avc_available = enable;
        self
    }

    // ========================================================================
    // State Queries
    // ========================================================================

    /// Check if the client has completed capability negotiation
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state == ClientState::Active
    }

    /// Get the negotiated capability set
    #[must_use]
    pub fn negotiated_capabilities(&self) -> Option<&CapabilitySet> {
        self.negotiated_caps.as_ref()
    }

    /// Get codec capabilities determined from negotiation
    #[must_use]
    pub fn codec_capabilities(&self) -> &CodecCapabilities {
        &self.codec_caps
    }

    /// Get a surface by ID
    #[must_use]
    pub fn get_surface(&self, surface_id: u16) -> Option<&Surface> {
        self.surfaces.get(&surface_id)
    }

    /// Get the total number of frames decoded
    #[must_use]
    pub fn total_frames_decoded(&self) -> u32 {
        self.total_frames_decoded
    }

    /// Lowest `frame_id` whose `FrameAcknowledge` has not yet been sent (the front of the
    /// reorder buffer), or `None` when no frame is pending. Useful for confirming the ack
    /// sequence during a live dual-monitor repro.
    #[must_use]
    pub fn next_unacked_frame_id(&self) -> Option<u32> {
        self.next_unacked_frame_id
    }

    // ========================================================================
    // PDU Handlers
    // ========================================================================

    /// Offline harness entry point: decode + composite a stream of RDPGFX PDUs that are
    /// ALREADY decompressed (no zgfx), i.e. concatenated `[cmdId(2)][flags(2)][pduLength(4)][body]`.
    /// Used by the `reconstruct` example to replay a flat capture and diff against a golden.
    #[doc(hidden)]
    pub fn process_pdu_bytes_for_test(&mut self, data: &[u8]) -> PduResult<()> {
        let mut cursor = ReadCursor::new(data);
        while !cursor.is_empty() {
            let pdu = match decode_cursor::<GfxPdu>(&mut cursor) {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "process_pdu_bytes_for_test: decode stopped");
                    break;
                }
            };
            let _ = self.handle_pdu(pdu)?;
        }
        Ok(())
    }

    fn handle_pdu(&mut self, pdu: GfxPdu) -> PduResult<Vec<DvcMessage>> {
        match pdu {
            GfxPdu::CapabilitiesConfirm(confirm) => {
                self.handle_capabilities_confirm(confirm.0);
                Ok(vec![])
            }
            GfxPdu::ResetGraphics(reset) => {
                self.handle_reset_graphics(reset.width, reset.height);
                Ok(vec![])
            }
            GfxPdu::CreateSurface(create) => {
                self.handle_create_surface(create.surface_id, create.width, create.height, create.pixel_format);
                Ok(vec![])
            }
            GfxPdu::DeleteSurface(delete) => {
                self.handle_delete_surface(delete.surface_id);
                Ok(vec![])
            }
            GfxPdu::MapSurfaceToOutput(map) => {
                self.handle_map_surface(map.surface_id, map.output_origin_x, map.output_origin_y);
                Ok(vec![])
            }
            GfxPdu::StartFrame(start) => {
                self.current_frame_id = Some(start.frame_id);
                self.current_frame_has_avc = false;
                self.frames_queued = self.frames_queued.saturating_add(1);
                trace!(frame_id = start.frame_id, "StartFrame");
                Ok(vec![])
            }
            GfxPdu::WireToSurface1(wire) => {
                self.handle_wire_to_surface1(wire)?;
                Ok(vec![])
            }
            GfxPdu::WireToSurface2(pdu) => {
                trace!("WireToSurface2 (progressive codec)");
                self.handler.on_wire_to_surface2(&pdu);
                self.handle_wire_to_surface2(pdu)?;
                Ok(vec![])
            }
            GfxPdu::EndFrame(end) => self.handle_end_frame(end.frame_id),

            // Surface operations
            GfxPdu::SolidFill(pdu) => {
                trace!(surface_id = pdu.surface_id, "SolidFill");
                self.handler.on_solid_fill(&pdu);
                Ok(vec![])
            }
            GfxPdu::SurfaceToSurface(pdu) => {
                trace!(
                    src = pdu.source_surface_id,
                    dst = pdu.destination_surface_id,
                    "SurfaceToSurface"
                );
                self.handler.on_surface_to_surface(&pdu);
                Ok(vec![])
            }

            // Cache operations
            GfxPdu::SurfaceToCache(pdu) => {
                trace!(
                    surface_id = pdu.surface_id,
                    cache_slot = pdu.cache_slot,
                    "SurfaceToCache"
                );
                self.handler.on_surface_to_cache(&pdu);
                Ok(vec![])
            }
            GfxPdu::CacheToSurface(pdu) => {
                trace!(
                    cache_slot = pdu.cache_slot,
                    surface_id = pdu.surface_id,
                    "CacheToSurface"
                );
                self.handler.on_cache_to_surface(&pdu);
                Ok(vec![])
            }
            GfxPdu::EvictCacheEntry(pdu) => {
                trace!(cache_slot = pdu.cache_slot, "EvictCacheEntry");
                self.handler.on_evict_cache_entry(&pdu);
                Ok(vec![])
            }
            GfxPdu::CacheImportReply(pdu) => {
                trace!("CacheImportReply");
                self.handler.on_cache_import_reply(&pdu);
                Ok(vec![])
            }

            // Surface mapping variants
            GfxPdu::MapSurfaceToWindow(pdu) => {
                trace!(
                    surface_id = pdu.surface_id,
                    window_id = pdu.window_id,
                    "MapSurfaceToWindow"
                );
                self.handler.on_map_surface_to_window(&pdu);
                Ok(vec![])
            }
            GfxPdu::MapSurfaceToScaledOutput(pdu) => {
                debug!(
                    surface_id = pdu.surface_id,
                    output_origin_x = pdu.output_origin_x,
                    output_origin_y = pdu.output_origin_y,
                    target_width = pdu.target_width,
                    target_height = pdu.target_height,
                    "MapSurfaceToScaledOutput"
                );
                self.handler.on_map_surface_to_scaled_output(&pdu);
                Ok(vec![])
            }
            GfxPdu::MapSurfaceToScaledWindow(pdu) => {
                trace!(surface_id = pdu.surface_id, "MapSurfaceToScaledWindow");
                self.handler.on_map_surface_to_scaled_window(&pdu);
                Ok(vec![])
            }

            // Progressive codec context management
            GfxPdu::DeleteEncodingContext(pdu) => {
                trace!(
                    surface_id = pdu.surface_id,
                    codec_context_id = pdu.codec_context_id,
                    "DeleteEncodingContext"
                );
                self.progressive_decoder
                    .delete_context(pdu.surface_id, pdu.codec_context_id);
                self.handler.on_delete_encoding_context(&pdu);
                Ok(vec![])
            }

            // Non-standard SecureBubble proxy extensions
            GfxPdu::Watermark(pdu) => {
                trace!(
                    surface_id = pdu.surface_id,
                    width = pdu.width,
                    height = pdu.height,
                    opacity = pdu.opacity,
                    "Watermark"
                );
                self.handler.on_watermark(&pdu);
                Ok(vec![])
            }
            GfxPdu::ProtectSurface(pdu) => {
                trace!(surface_id = pdu.surface_id, enable = pdu.enable, "ProtectSurface");
                self.handler.on_protect_surface(&pdu);
                Ok(vec![])
            }

            // Catch-all for any remaining PDUs
            other => {
                self.handler.on_unhandled_pdu(&other);
                Ok(vec![])
            }
        }
    }

    fn handle_capabilities_confirm(&mut self, cap: RawCapabilitySet) {
        // Server confirms a single capability set. If we cannot interpret it
        // (unknown version, or malformed body), we still transition to Active
        // to avoid hanging the session, but we keep `negotiated_caps` empty
        // and skip the typed callback so consumers don't observe a confirm
        // they can't reason about.
        let cap = match cap.parsed() {
            Ok(Some(typed)) => typed,
            Ok(None) => {
                warn!(
                    version = cap.version.0,
                    "Server confirmed an unknown EGFX capability version; proceeding with defaults"
                );
                self.state = ClientState::Active;
                return;
            }
            Err(e) => {
                warn!(error = %e, "Failed to parse server's EGFX capabilities confirmation");
                self.state = ClientState::Active;
                return;
            }
        };

        self.codec_caps = CodecCapabilities::from_capability_set(&cap);
        self.state = ClientState::Active;
        let cap = self.negotiated_caps.insert(cap);

        debug!(
            avc420 = self.codec_caps.avc420,
            avc444 = self.codec_caps.avc444,
            "EGFX capabilities confirmed"
        );

        self.handler.on_capabilities_confirmed(cap);
    }

    fn handle_reset_graphics(&mut self, width: u32, height: u32) {
        // Per spec, ResetGraphics implicitly destroys all surfaces
        self.surfaces.clear();

        // Reset frame tracking state so subsequent FrameAcknowledge PDUs
        // don't report stale queue depth from a previous stream.
        // Capability state (negotiated_caps, codec_caps) is NOT reset here:
        // per spec, capabilities are negotiated via CapabilitiesConfirm before
        // ResetGraphics, and a ResetGraphics does not re-negotiate capabilities.
        self.current_frame_id = None;
        self.frames_queued = 0;
        self.current_frame_has_avc = false;
        // Drop pending acks: their frame_ids belong to the previous stream and will
        // never be presented now, so holding them would leak / stall flow control.
        // `max_total_acked` is intentionally NOT reset (matching `total_frames_decoded`):
        // the cumulative counter must not regress across a ResetGraphics either.
        self.pending_acks.clear();
        self.next_unacked_frame_id = None;
        // Reset the frame_id high-water: the server may restart its per-channel `frame_id`
        // sequence for the new stream, and a stale high-water would suppress every ack of
        // the restarted (lower-id) sequence and stall flow control. `max_total_acked` is
        // deliberately NOT reset (its `total_frames_decoded` source keeps counting), so the
        // cumulative counter still never regresses.
        self.max_acked_frame_id = 0;

        // Reset decoder state for new stream
        if let Some(ref mut decoder) = self.h264_decoder {
            decoder.reset();
        }
        // RFX Progressive state is scoped per stream: a new codec_context_id may reuse
        // a prior id, and its tiles must not decode against stale state.
        self.progressive_decoder.reset();
        // The ClearCodec decoder is deliberately NOT reset here. MS-RDPEGFX 3.3.5.14 only
        // resizes the Graphics Output Buffer; cache lifetime is driven by the stream instead,
        // through CLEARCODEC_FLAG_CACHE_RESET (2.2.4.1), which ClearCodecDecoder::decode
        // already honors by resetting the V-bar cursors. Dropping the decoder here would also
        // drop the glyph cache, so a legitimate post-reset GLYPH_HIT would fail unless the
        // server redundantly re-sent every glyph.

        debug!(width, height, "Graphics reset");
        self.handler.on_reset_graphics(width, height);
    }

    fn handle_create_surface(&mut self, surface_id: u16, width: u16, height: u16, pixel_format: PixelFormat) {
        if width == 0 || height == 0 {
            warn!(surface_id, width, height, "Ignoring CreateSurface with zero dimensions");
            return;
        }

        let surface = Surface {
            id: surface_id,
            width,
            height,
            pixel_format,
            is_mapped: false,
            output_origin_x: 0,
            output_origin_y: 0,
        };

        debug!(surface_id, width, height, ?pixel_format, "Surface created");
        self.handler.on_surface_created(&surface);
        self.surfaces.insert(surface_id, surface);
    }

    fn handle_delete_surface(&mut self, surface_id: u16) {
        // Free any retained RFX-Progressive tile state for this surface. Progressive
        // state is keyed per surface (it persists across codec-context rotations),
        // so it is released here on surface deletion rather than on
        // DeleteEncodingContext.
        self.progressive_decoder.delete_surface(surface_id);
        if self.surfaces.remove(&surface_id).is_some() {
            debug!(surface_id, "Surface deleted");
            self.handler.on_surface_deleted(surface_id);
        } else {
            warn!(surface_id, "DeleteSurface for unknown surface");
        }
    }

    fn handle_map_surface(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        if let Some(surface) = self.surfaces.get_mut(&surface_id) {
            surface.is_mapped = true;
            surface.output_origin_x = origin_x;
            surface.output_origin_y = origin_y;
            debug!(surface_id, origin_x, origin_y, "Surface mapped to output");
            self.handler.on_surface_mapped(surface_id, origin_x, origin_y);
        } else {
            warn!(surface_id, "MapSurfaceToOutput for unknown surface");
        }
    }

    fn handle_wire_to_surface1(&mut self, pdu: crate::pdu::WireToSurface1Pdu) -> PduResult<()> {
        let surface = self
            .surfaces
            .get(&pdu.surface_id)
            .ok_or_else(|| pdu_other_err!("unknown surface in WireToSurface1"))?;

        // Validate rectangle ordering (left <= right, top <= bottom)
        let rect = &pdu.destination_rectangle;
        if rect.left > rect.right || rect.top > rect.bottom {
            warn!(
                left = rect.left,
                top = rect.top,
                right = rect.right,
                bottom = rect.bottom,
                "invalid destination rectangle ordering"
            );
            return Err(pdu_other_err!("invalid destination rectangle ordering"));
        }

        // Validate destination rectangle against surface bounds. The rectangle
        // uses exclusive `right`/`bottom`, so a full-surface update has
        // `right == surface.width` and `bottom == surface.height`, which is valid.
        if rect.right > surface.width || rect.bottom > surface.height {
            warn!(
                surface_id = pdu.surface_id,
                rect_right = rect.right,
                rect_bottom = rect.bottom,
                surface_width = surface.width,
                surface_height = surface.height,
                "WireToSurface1 destination rectangle exceeds surface bounds"
            );
        }

        match pdu.codec_id {
            Codec1Type::Avc420 => {
                self.decode_avc420(pdu.surface_id, &pdu.destination_rectangle, &pdu.bitmap_data)?;
            }
            Codec1Type::Avc444 | Codec1Type::Avc444v2 => {
                self.decode_avc444(pdu.surface_id, &pdu.destination_rectangle, &pdu.bitmap_data, pdu.codec_id);
            }
            Codec1Type::ClearCodec => {
                self.decode_clearcodec(pdu.surface_id, &pdu.destination_rectangle, &pdu.bitmap_data)?;
            }
            Codec1Type::Planar => {
                self.decode_planar(pdu.surface_id, &pdu.destination_rectangle, &pdu.bitmap_data)?;
            }
            Codec1Type::Uncompressed => {
                self.handle_uncompressed(pdu);
            }
            _ => {
                trace!(codec_id = ?pdu.codec_id, "Forwarding unsupported codec to handler");
                self.handler.on_unhandled_pdu(&GfxPdu::WireToSurface1(pdu));
            }
        }

        Ok(())
    }

    fn decode_avc420(&mut self, surface_id: u16, dest_rect: &ExclusiveRectangle, bitmap_data: &[u8]) -> PduResult<()> {
        let mut cursor = ReadCursor::new(bitmap_data);
        let stream = Avc420BitmapStream::decode(&mut cursor).map_err(|e| decode_err!(e))?;

        let Some(ref mut decoder) = self.h264_decoder else {
            trace!(
                surface_id,
                width = dest_rect.width(),
                height = dest_rect.height(),
                len = bitmap_data.len(),
                regions = stream.rectangles.len(),
                "AVC420 frame received (Stage 0 stub: decode pending out-of-band)"
            );
            return Ok(());
        };

        let frame = decoder
            .decode(stream.data)
            .map_err(|e| pdu_other_err!("H.264 decode", source: e))?;

        let dest_width = dest_rect.width();
        let dest_height = dest_rect.height();

        // Decoded frame must be at least as large as the destination rectangle.
        // Larger is expected (macroblock alignment) and handled by cropping.
        // Smaller means the server sent mismatched dimensions.
        if frame.width() < u32::from(dest_width) || frame.height() < u32::from(dest_height) {
            warn!(
                frame_width = frame.width(),
                frame_height = frame.height(),
                dest_width,
                dest_height,
                "decoded frame smaller than destination rectangle"
            );
            return Err(pdu_other_err!("decoded frame smaller than destination rectangle"));
        }

        let cropped_data = crop_decoded_frame(frame.data(), frame.width(), frame.height(), dest_width, dest_height);

        let update = BitmapUpdate {
            surface_id,
            destination_rectangle: dest_rect.clone(),
            codec_id: Codec1Type::Avc420,
            data: cropped_data,
            width: dest_width,
            height: dest_height,
        };

        self.handler.on_bitmap_updated(&update);
        Ok(())
    }

    /// Parse an AVC444 / AVC444v2 (`WireToSurface1`) bitmap stream and log its structure.
    ///
    /// MILESTONE 1 (2026-08-10): parse + structured log ONLY — no H.264 decode / no
    /// composite yet. This upgrades the Stage-0 stub so we can confirm our parser
    /// handles real AVD wire frames before the async WebCodecs bridge lands.
    ///
    /// AVC444 multiplexes TWO H.264 sub-streams — main/luma (`stream1`) and aux/chroma
    /// (`stream2`) — under the 2-bit LC field ([`Encoding`]):
    /// - `LumaAndChroma` (LC=0): both present → full 4:4:4 update.
    /// - `Luma` (LC=1): only `stream1` (luma); chroma retained from the prior frame.
    /// - `Chroma` (LC=2): only `stream1`, but it carries the *chroma* frame (the aux
    ///   sequence rides the stream-1 wire slot); luma retained.
    ///
    /// Parse errors are logged, not propagated, so one malformed frame can't abort the
    /// probe session mid-stream.
    fn decode_avc444(
        &mut self,
        surface_id: u16,
        dest_rect: &ExclusiveRectangle,
        bitmap_data: &[u8],
        codec_id: Codec1Type,
    ) {
        let mut cursor = ReadCursor::new(bitmap_data);
        let stream = match Avc444BitmapStream::decode(&mut cursor) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    surface_id,
                    codec = ?codec_id,
                    len = bitmap_data.len(),
                    error = %e,
                    "AVC444 parse failed"
                );
                return;
            }
        };

        // Which semantic sequence does the stream-1 slot feed? For LC=2 the stream-1
        // slot carries the aux/chroma frame; otherwise it is the main/luma frame.
        let stream1_role = if stream.encoding == Encoding::CHROMA {
            "chroma(aux)"
        } else {
            "luma(main)"
        };
        let (s2_regions, s2_len) = stream
            .stream2
            .as_ref()
            .map_or((0, 0), |s2| (s2.rectangles.len(), s2.data.len()));

        debug!(
            surface_id,
            codec = ?codec_id,
            lc = ?stream.encoding,
            stream1_role,
            dest_w = dest_rect.width(),
            dest_h = dest_rect.height(),
            s1_regions = stream.stream1.rectangles.len(),
            s1_len = stream.stream1.data.len(),
            s2_present = stream.stream2.is_some(),
            s2_regions,
            s2_len,
            "AVC444 parsed"
        );

        // MVP (single-decoder path): forward only the main/stream1 YUV420 picture.
        // Decoding it alone yields a full-color 4:2:0 image. For LC=CHROMA (aux-only)
        // there is no main update to forward — that case is handled once the 4:4:4
        // recombination layer lands.
        if stream.encoding != Encoding::CHROMA {
            // Mark this eGFX frame as carrying async AVC so its FrameAcknowledge is
            // deferred until the browser decoder presents it (flow control).
            self.current_frame_has_avc = true;
            self.handler.on_avc_frame(&AvcFrame {
                surface_id,
                frame_id: self.current_frame_id.unwrap_or(0),
                codec_id,
                destination_rectangle: dest_rect.clone(),
                main_stream: stream.stream1.data,
                regions: &stream.stream1.rectangles,
            });
        }
    }

    /// Decode a ClearCodec (`WireToSurface1`) bitmap and emit it through `on_bitmap_updated`.
    ///
    /// ClearCodec is the mandatory lossless EGFX codec (text/UI/icons). It decodes in pure
    /// Rust with a persistent V-bar + glyph cache, so it is always available.
    fn decode_clearcodec(
        &mut self,
        surface_id: u16,
        dest_rect: &ExclusiveRectangle,
        bitmap_data: &[u8],
    ) -> PduResult<()> {
        // `ExclusiveRectangle::width()/height()` return the exclusive extent (right - left).
        let dest_width = dest_rect.width();
        let dest_height = dest_rect.height();

        let bgra = self
            .clearcodec_decoder
            .decode(bitmap_data, dest_width, dest_height)
            .map_err(|e| {
                warn!(error = ?e, dest_width, dest_height, "ClearCodec decode failed");
                pdu_other_err!("ClearCodec decode", source: e)
            })?;

        // ClearCodec outputs BGRA; convert to RGBA for the uniform BitmapUpdate format.
        let rgba = convert_bgra_to_rgba(&bgra);

        let update = BitmapUpdate {
            surface_id,
            destination_rectangle: dest_rect.clone(),
            codec_id: Codec1Type::ClearCodec,
            data: rgba,
            width: dest_width,
            height: dest_height,
        };

        self.handler.on_bitmap_updated(&update);
        Ok(())
    }

    /// Decode an RDP 6.0 Planar bitmap ([MS-RDPEGFX] `RDPGFX_CODECID_PLANAR`, 0x000A).
    ///
    /// The payload is an `RDP6_BITMAP_STREAM` ([MS-RDPEGDI] 2.2.2.5.1), the same
    /// structure the fast-path bitmap route decodes, so this reuses `ironrdp-graphics`'
    /// decoder. Adopted from upstream; emits `on_bitmap_updated` for the web compositor
    /// (upstream feeds its in-crate compositor, which we don't use).
    fn decode_planar(&mut self, surface_id: u16, dest_rect: &ExclusiveRectangle, bitmap_data: &[u8]) -> PduResult<()> {
        // MS-RDPEGFX 2.2.2.1: destRect gives both the target point and the exact
        // width/height of the bitmap data (it is only a bounding rect for AVC).
        let dest_width = dest_rect.width();
        let dest_height = dest_rect.height();

        let mut rgb24 = Vec::new();
        self.planar_decoder
            .decode_bitmap_stream_to_rgb24(bitmap_data, &mut rgb24, usize::from(dest_width), usize::from(dest_height))
            .map_err(|e| pdu_other_err!("Planar decode", source: e))?;

        // The decoder emits RGB24 top-down row-major (surface order); no flip needed.
        // Planar carries color only — per-pixel opacity is a separate
        // RDPGFX_CODECID_ALPHA command — so the pixels are opaque.
        let rgba = convert_rgb24_to_rgba(&rgb24);

        let update = BitmapUpdate {
            surface_id,
            destination_rectangle: dest_rect.clone(),
            codec_id: Codec1Type::Planar,
            data: rgba,
            width: dest_width,
            height: dest_height,
        };

        self.handler.on_bitmap_updated(&update);
        Ok(())
    }

    /// Decode a RemoteFX Progressive (`WireToSurface2`) bitmap stream and emit each updated
    /// 64x64 tile through `on_bitmap_updated`. Edge tiles are clipped/cropped to the surface.
    fn handle_wire_to_surface2(&mut self, pdu: WireToSurface2Pdu) -> PduResult<()> {
        let surface = self
            .surfaces
            .get(&pdu.surface_id)
            .ok_or_else(|| pdu_other_err!("unknown surface in WireToSurface2"))?;
        let (surface_width, surface_height) = (surface.width, surface.height);

        let tiles = match self.progressive_decoder.decode_bitmap(
            pdu.surface_id,
            pdu.codec_context_id,
            surface_width,
            surface_height,
            &pdu.bitmap_data,
        ) {
            Ok(tiles) => tiles,
            Err(e) => {
                warn!(error = ?e, "rfx progressive decode failed");
                return Err(pdu_other_err!("rfx progressive decode failed"));
            }
        };

        for tile in tiles {
            // Commit ONLY the region-clipped spans. A full-quality single-pass
            // tile covers all 64x64, but the server marks only a small rect
            // dirty; committing the whole tile would splat uniform/stale pixels
            // over untouched wallpaper (the quality-0xFF "white splat"). The tile
            // is already fully decoded (coefficient state updated) inside
            // `decode_bitmap`; `tile.clips` are the tile ∩ region-rect overlaps in
            // surface coordinates. FreeRDP clips identically in
            // `progressive_process_tiles`.
            let tile_left = tile.x_idx.saturating_mul(64);
            let tile_top = tile.y_idx.saturating_mul(64);
            for clip in &tile.clips {
                if clip.w == 0 || clip.h == 0 {
                    continue;
                }
                // Offset of this clip within the tile's own 64x64 pixel buffer.
                let lx = clip.x.saturating_sub(tile_left);
                let ly = clip.y.saturating_sub(tile_top);
                let data = subrect_rgba(&tile.pixels, 64, lx, ly, clip.w, clip.h);
                let update = BitmapUpdate {
                    surface_id: pdu.surface_id,
                    destination_rectangle: ExclusiveRectangle {
                        left: clip.x,
                        top: clip.y,
                        right: clip.x.saturating_add(clip.w),
                        bottom: clip.y.saturating_add(clip.h),
                    },
                    codec_id: Codec1Type::Uncompressed,
                    data,
                    width: clip.w,
                    height: clip.h,
                };
                self.handler.on_bitmap_updated(&update);
            }
        }
        Ok(())
    }

    fn handle_uncompressed(&mut self, pdu: crate::pdu::WireToSurface1Pdu) {
        let dest_width = pdu.destination_rectangle.width();
        let dest_height = pdu.destination_rectangle.height();

        // Convert wire-format pixels to RGBA.
        // BitmapUpdate.data is always RGBA8888 regardless of codec -- this is
        // the convention so that handlers get a uniform pixel format.
        // Uncompressed wire format is 32-bit LE (0xAARRGGBB → bytes [B, G, R, A]).
        let rgba_data = convert_uncompressed_to_rgba(&pdu.bitmap_data);

        let update = BitmapUpdate {
            surface_id: pdu.surface_id,
            destination_rectangle: pdu.destination_rectangle,
            codec_id: Codec1Type::Uncompressed,
            data: rgba_data,
            width: dest_width,
            height: dest_height,
        };

        self.handler.on_bitmap_updated(&update);
    }

    fn handle_end_frame(&mut self, frame_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.total_frames_decoded = self.total_frames_decoded.wrapping_add(1);
        self.current_frame_id = None;
        self.frames_queued = self.frames_queued.saturating_sub(1);

        self.handler.on_frame_complete(frame_id);

        // Per [3.3.5.12] the client MUST send a FrameAcknowledge after EndFrame — but
        // WHEN, and in what ORDER, matters for flow control. Synchronous codecs
        // (ClearCodec/Progressive/Planar) are already composited here, so their ack is
        // READY now. AVC decodes asynchronously in the browser, so acking at EndFrame (before
        // the decoder has even run) would tell the server we're keeping up when we're not — it
        // would flood the async decoder and the picture would lag seconds behind. Its ack
        // becomes ready only when the decoder signals DECODE-completion (it produced a frame;
        // see `build_frame_ack`), pacing the server to our real decode throughput — the actual
        // bottleneck — exactly as the synchronous codecs pace to their Rust decode. Note the
        // ack is decoupled from PRESENTATION (rAF-gated, per-surface): presenting drives the
        // display, not flow control, so a slow present never starves delivery.
        //
        // Either way the frame is REGISTERED in the reorder buffer and actually emitted
        // by `flush_ready_acks`, which sends only the consecutive ready run from the
        // front. That keeps `frameId` / `totalFramesDecoded` monotonic on the wire even
        // when two surfaces feed two out-of-order VideoDecoders (multi-monitor), the
        // condition that otherwise trips the server's "graphics subsystem error state".
        let ready = !self.current_frame_has_avc;
        self.pending_acks.insert(
            frame_id,
            PendingAck {
                total_frames_decoded: self.total_frames_decoded,
                ready,
                avc: self.current_frame_has_avc,
            },
        );
        if self.next_unacked_frame_id.is_none() {
            self.next_unacked_frame_id = Some(frame_id);
        }
        if ready {
            trace!(frame_id, "Frame ready; flushing ready run");
        } else {
            trace!(frame_id, "Deferring FrameAcknowledge until AVC present");
        }
        Ok(self.flush_ready_acks())
    }

    /// Emit the `FrameAcknowledge` DVC messages unblocked by the current state, as a
    /// **non-blocking, monotonic high-water mark**.
    ///
    /// For every *ready* frame (ascending `frame_id`) we either emit its ack — if its
    /// `frame_id` advances the high-water `max_acked_frame_id` — or retire it silently as
    /// an out-of-order straggler already covered by the cumulative counter. Un-ready
    /// (un-decoded AVC) frames are *skipped*, not blocked on: a slow surface can no
    /// longer head-of-line-block a fast surface's acks, which was the multi-monitor stall.
    /// Both wire counters (`frame_id`, `total_frames_decoded`) only advance, so the stream
    /// stays monotonic (MS-RDPEGFX 2.2.2.13).
    ///
    /// Single-monitor / synchronous codecs are unchanged: frames become ready in ascending
    /// `frame_id` order and each strictly advances the high-water, so exactly one ack is
    /// emitted per frame, in order, with no added latency.
    ///
    /// Memory hygiene: never-decoded AVC frames (browser decoder error) leave dangling
    /// un-ready entries. They no longer gate flow control, but once the buffer exceeds
    /// `MAX_DEFERRED_ACKS` the oldest are evicted so it can't grow without bound.
    fn flush_ready_acks(&mut self) -> Vec<DvcMessage> {
        let mut out = Vec::new();

        // Collect ready frame_ids up front (ascending — BTreeMap order) so we can mutate
        // the map while iterating.
        let ready_ids: Vec<u32> = self
            .pending_acks
            .iter()
            .filter(|(_, p)| p.ready)
            .map(|(&id, _)| id)
            .collect();

        for frame_id in ready_ids {
            let pending = self.pending_acks.remove(&frame_id).expect("collected above");
            if frame_id > self.max_acked_frame_id {
                out.push(self.commit_ack(frame_id, pending));
            } else {
                // Out-of-order straggler: a lower-`frame_id` surface finished decode after the
                // cumulative high-water already passed it. Retire without emitting — an
                // earlier ack already advanced the server's window past this frame. This is
                // exactly what keeps the other monitor flowing instead of stalling here.
                trace!(
                    frame_id,
                    max_acked = self.max_acked_frame_id,
                    "AVC straggler retired (ack coalesced into high-water)"
                );
            }
        }

        // Evict never-presented entries over the safety bound (oldest first). They are
        // un-ready and no longer gate flow control; this only bounds memory under loss.
        while self.pending_acks.len() > MAX_DEFERRED_ACKS {
            let Some((&stale_id, _)) = self.pending_acks.iter().next() else {
                break;
            };
            warn!(
                stale_id,
                backlog = self.pending_acks.len(),
                "Evicting un-presented AVC ack (over MAX_DEFERRED_ACKS)"
            );
            self.pending_acks.remove(&stale_id);
        }

        self.next_unacked_frame_id = self.pending_acks.keys().next().copied();
        out
    }

    /// Advance the wire high-water marks (`frame_id`, clamped `total_frames_decoded`),
    /// log the (now monotonic) ack, and build the DVC message. Only called for a frame
    /// whose `frame_id` already exceeds `max_acked_frame_id`.
    fn commit_ack(&mut self, frame_id: u32, pending: PendingAck) -> DvcMessage {
        let total = pending.total_frames_decoded.max(self.max_total_acked);
        self.max_total_acked = total;
        self.max_acked_frame_id = frame_id;
        debug!(
            frame_id,
            total_frames_decoded = total,
            kind = if pending.avc { "deferred" } else { "sync" },
            pending_depth = self.pending_acks.len(),
            "Sending FrameAcknowledge"
        );
        self.make_frame_ack(frame_id, total)
    }

    /// Build a `FrameAcknowledge` DVC message. `queue_depth` reports the count still
    /// awaiting present, which signals backpressure to the server.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "Box<GfxPdu> to Box<dyn DvcEncode> coercion; pending_acks.len() bounded by MAX_DEFERRED_ACKS"
    )]
    fn make_frame_ack(&self, frame_id: u32, total_frames_decoded: u32) -> DvcMessage {
        let ack = GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
            queue_depth: QueueDepth::from_u32(self.pending_acks.len() as u32),
            frame_id,
            total_frames_decoded,
        });
        Box::new(ack) as DvcMessage
    }

    /// Emit the deferred `FrameAcknowledge` for an AVC frame the out-of-band decoder
    /// has now DECODED (produced a frame). Called by the session run loop when the browser
    /// signals decode-completion — NOT present. Acking at decode paces the server to the
    /// client's real decode throughput (the actual bottleneck; presentation is rAF-gated and
    /// decoupled), matching how the synchronous codecs ack right after their Rust decode.
    /// Returns the ack DVC message(s) to encode+write, or empty if `frame_id` is unknown
    /// (already acked, never deferred, or a non-AVC frame).
    #[must_use]
    pub fn build_frame_ack(&mut self, frame_id: u32) -> Vec<DvcMessage> {
        // Mark the decoded AVC frame ready, then flush whatever high-water run this unblocks.
        // The ack is emitted only if `frame_id` advances the wire high-water; a lower-id
        // straggler (e.g. the other monitor's surface decoding out of frame_id order) is
        // retired silently, already covered by the cumulative counter, so a slow surface can't
        // head-of-line-block a fast one. If `frame_id` isn't pending (already flushed, never
        // deferred, or unknown) this is a no-op.
        match self.pending_acks.get_mut(&frame_id) {
            Some(entry) => {
                entry.ready = true;
                trace!(frame_id, "AVC decoded; marking ack ready");
            }
            None => return Vec::new(),
        }
        self.flush_ready_acks()
    }
}

impl_as_any!(GraphicsPipelineClient);

impl DvcProcessor for GraphicsPipelineClient {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        let advertise = if self.h264_decoder.is_some() || self.avc_available {
            // Replicate a modern Windows client's CAPSADVERTISE so the target enables
            // AVC (H.264). IronRDP's typed capsets stop at V10.7, but the target only
            // switches to AVC444 when the client advertises the V11.x capsets carrying
            // the AVC flags — so we build the exact raw set an up-to-date mstsc sends
            // (V8..V11.5). The V11.4 slot in a real mstsc carries an 8 KB signed client
            // license we can't reproduce; we advertise the version with zero flags.
            CapabilitiesAdvertisePdu(avc_capable_capsets())
        } else {
            // No H.264 decoder wanted: filter out AVC-implying sets so the server uses
            // the progressive/bitmap path.
            let filtered: Vec<CapabilitySet> = self
                .handler
                .capabilities()
                .into_iter()
                .filter(|cap| !CodecCapabilities::from_capability_set(cap).avc420)
                .collect();
            let caps = if filtered.is_empty() {
                debug!("No H.264 decoder and all capabilities require AVC; falling back to V8");
                vec![CapabilitySet::V8 {
                    flags: CapabilitiesV8Flags::SMALL_CACHE,
                }]
            } else {
                filtered
            };
            let mut advertise = CapabilitiesAdvertisePdu::from_typed(&caps);
            // Advertise the private V11.1 (0x000b0101) capset the SecureBubble proxy
            // keys on to enable per-session watermark / screen-capture-protect
            // injection (rdp-proxy egfx.cpp). Flags are 0: AVC is opt-in via
            // AVC420_ENABLED (0x10), which we never set, so the target stays on the
            // progressive/bitmap path this client decodes. Without this capset a
            // watermark-enabled session is refused by the proxy.
            advertise
                .0
                .push(RawCapabilitySet::new(CapabilityVersion(0x000b_0101), 0u32.to_le_bytes().to_vec()));
            advertise
        };

        let pdu = GfxPdu::CapabilitiesAdvertise(advertise);

        #[expect(clippy::as_conversions, reason = "Box<GfxPdu> to Box<dyn DvcEncode> coercion")]
        Ok(vec![Box::new(pdu) as DvcMessage])
    }

    fn close(&mut self, _channel_id: u32) {
        self.state = ClientState::Closed;
        self.handler.on_close();
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // ZGFX decompress
        self.decompressed_buffer.clear();
        self.decompressed_buffer.shrink_to(MAX_DECOMPRESSED_BUFFER_CAPACITY);
        self.decompressor
            .decompress(payload, &mut self.decompressed_buffer)
            .map_err(|e| decode_err!(e))?;

        // Decode all PDUs first (cursor borrows decompressed_buffer).
        //
        // Be tolerant: a single undecodable PDU in a segment must NOT discard the
        // whole segment (e.g. a desktop WireToSurface2 frame batched with a PDU
        // ironrdp can't yet decode). Stop decoding at the first failure but still
        // process everything decoded before it, and log which PDU broke so it can
        // be fixed.
        let mut pdus = Vec::new();
        {
            let mut cursor = ReadCursor::new(self.decompressed_buffer.as_slice());
            while !cursor.is_empty() {
                match decode_cursor::<GfxPdu>(&mut cursor) {
                    Ok(pdu) => pdus.push(pdu),
                    Err(e) => {
                        warn!(error = %e, decoded_ok = pdus.len(), "EGFX PDU decode failed; processing prior PDUs in segment");
                        break;
                    }
                }
            }
        }

        // Process decoded PDUs
        let mut responses: Vec<DvcMessage> = Vec::new();
        for pdu in pdus {
            let pdu_responses = self.handle_pdu(pdu)?;
            responses.extend(pdu_responses);
        }

        Ok(responses)
    }
}

impl DvcClientProcessor for GraphicsPipelineClient {}

// ============================================================================
// Frame Cropping
// ============================================================================

/// The CAPSADVERTISE capset list a current Windows client (mstsc) sends, built as raw
/// capsets so we can advertise the V11.x versions IronRDP has no typed variant for.
///
/// The target only enables AVC (H.264) when the client advertises these V11.x capsets
/// with their AVC flags (`0x400`/`0xc00`/`0x2c00`); advertising up to V10.7 alone yields
/// a progressive fallback. `(version, flags)` pairs are captured verbatim from a real
/// mstsc CAPSADVERTISE. The V11.4 slot in a real client carries an ~8 KB signed
/// per-machine license we can't reproduce, so we advertise that version with zero flags.
fn avc_capable_capsets() -> Vec<RawCapabilitySet> {
    const CAPS: &[(u32, u32)] = &[
        (0x0008_0004, 0),           // V8
        (0x0008_0105, 0),           // V8.1
        (0x000a_0002, 0),           // V10
        (0x000a_0200, 0),           // V10.2
        (0x000a_0301, 0),           // V10.3
        (0x000a_0400, 0),           // V10.4
        (0x000a_0502, 0),           // V10.5
        (0x000a_0600, 0),           // V10.6
        (0x000a_0701, 0),           // V10.7
        (0x000b_0101, 0),           // V11.1
        (0x000b_0200, 0x0000_0400), // V11.2
        (0x000b_0300, 0x0000_0c00), // V11.3
        (0x000b_0400, 0),           // V11.4 (real mstsc carries an 8 KB client license here)
        (0x000b_0500, 0x0000_2c00), // V11.5
    ];
    CAPS.iter()
        .map(|&(ver, flags)| RawCapabilitySet::new(CapabilityVersion(ver), flags.to_le_bytes().to_vec()))
        .collect()
}

/// Convert BGRA pixel data to RGBA8888.
///
/// ClearCodec produces BGRA output per [MS-RDPEGFX 2.2.4.1]. Reorder to
/// `[R, G, B, A]` for the uniform `BitmapUpdate` pixel format.
fn convert_bgra_to_rgba(src: &[u8]) -> Vec<u8> {
    debug_assert!(src.len() % 4 == 0, "BGRA input length not aligned to 4 bytes");
    let mut dst = Vec::with_capacity(src.len());
    for pixel in src.chunks_exact(4) {
        dst.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    dst
}

/// Widen RGB24 (from the Planar decoder) to opaque RGBA8888.
fn convert_rgb24_to_rgba(src: &[u8]) -> Vec<u8> {
    debug_assert!(src.len() % 3 == 0, "RGB24 input length not aligned to 3 bytes");
    let mut dst = Vec::with_capacity(src.len() / 3 * 4);
    for pixel in src.chunks_exact(3) {
        dst.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xFF]);
    }
    dst
}

/// Convert uncompressed 32bpp little-endian pixels to RGBA8888
///
/// The wire format for uncompressed graphics is 0xAARRGGBB in a 32-bit
/// little-endian word, which corresponds to bytes [B, G, R, A]. This
/// reorders to [R, G, B, 0xFF], treating all pixels as fully opaque.
fn convert_uncompressed_to_rgba(src: &[u8]) -> Vec<u8> {
    let mut dst = Vec::with_capacity(src.len());
    for pixel in src.chunks_exact(4) {
        let b = pixel[0];
        let g = pixel[1];
        let r = pixel[2];
        dst.extend_from_slice(&[r, g, b, 0xFF]);
    }
    dst
}

/// Crop a decoded RGBA frame to target dimensions
///
/// H.264 frames are macroblock-aligned (16x16), so decoded frames
/// may be larger than the destination rectangle. This function
/// extracts the top-left region matching the target size.
/// Extract a `w`x`h` RGBA sub-rectangle at (`x`, `y`) from a `src_w`-pixel-wide
/// RGBA buffer (4 bytes/pixel). Used to commit only the region-clipped span of a
/// decoded 64x64 progressive tile. Rows past the end of `src` are skipped.
fn subrect_rgba(src: &[u8], src_w: u16, x: u16, y: u16, w: u16, h: u16) -> Vec<u8> {
    let src_w = usize::from(src_w);
    let (x, y, w, h) = (usize::from(x), usize::from(y), usize::from(w), usize::from(h));
    let mut out = Vec::with_capacity(w.saturating_mul(h).saturating_mul(4));
    for row in 0..h {
        let start = ((y + row) * src_w + x) * 4;
        let end = start + w * 4;
        if end <= src.len() {
            out.extend_from_slice(&src[start..end]);
        }
    }
    out
}

fn crop_decoded_frame(
    data: &[u8],
    decoded_width: u32,
    decoded_height: u32,
    target_width: u16,
    target_height: u16,
) -> Vec<u8> {
    let tw = u32::from(target_width);
    let th = u32::from(target_height);

    if decoded_width == 0 || decoded_height == 0 || tw == 0 || th == 0 {
        return Vec::new();
    }

    // If dimensions match, return as-is
    if decoded_width == tw && decoded_height == th {
        return data.to_vec();
    }

    let src_stride = decoded_width.saturating_mul(4);
    let dst_stride = tw.saturating_mul(4);
    let rows = th.min(decoded_height);

    #[expect(clippy::as_conversions, reason = "product of u32 values bounded by frame dimensions")]
    let mut cropped = Vec::with_capacity((dst_stride as usize).saturating_mul(rows as usize));

    for row in 0..rows {
        #[expect(clippy::as_conversions, reason = "row * src_stride bounded by frame size")]
        let src_start = (row.saturating_mul(src_stride)) as usize;
        #[expect(clippy::as_conversions, reason = "bounded by frame dimensions")]
        let copy_len = dst_stride.min(src_stride) as usize;
        let src_end = src_start.saturating_add(copy_len);
        if src_end <= data.len() {
            cropped.extend_from_slice(&data[src_start..src_end]);
        }
    }

    #[expect(clippy::as_conversions, reason = "dst_stride * rows bounded by frame dimensions")]
    let expected_len = (dst_stride as usize).saturating_mul(rows as usize);
    if cropped.len() < expected_len {
        tracing::warn!(
            expected = expected_len,
            actual = cropped.len(),
            "Decoded frame data truncated during crop"
        );
    }

    cropped
}

/// Unit tests that require access to private fields (state, surfaces, frame tracking).
/// Integration tests exercising the public DVC API are in ironrdp-testsuite-core/tests/egfx/client.rs.
#[cfg(test)]
mod tests {
    use super::*;

    struct TestHandler;
    impl GraphicsPipelineHandler for TestHandler {
        fn on_capabilities_confirmed(&mut self, _caps: &CapabilitySet) {}
        fn on_reset_graphics(&mut self, _width: u32, _height: u32) {}
        fn on_surface_created(&mut self, _surface: &Surface) {}
        fn on_surface_deleted(&mut self, _surface_id: u16) {}
        fn on_surface_mapped(&mut self, _surface_id: u16, _x: u32, _y: u32) {}
        fn on_bitmap_updated(&mut self, _update: &BitmapUpdate) {}
        fn on_frame_complete(&mut self, _frame_id: u32) {}
        fn on_close(&mut self) {}
        fn on_unhandled_pdu(&mut self, _pdu: &GfxPdu) {}
    }

    #[test]
    fn state_transitions() {
        let mut client = GraphicsPipelineClient::new(Box::new(TestHandler), None);

        assert_eq!(client.state, ClientState::WaitingForConfirm);
        assert!(!client.is_active());

        let _ = client.handle_pdu(GfxPdu::CapabilitiesConfirm(
            crate::pdu::CapabilitiesConfirmPdu::from_typed(&CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::empty(),
            }),
        ));
        assert_eq!(client.state, ClientState::Active);
        assert!(client.is_active());

        client.close(0);
        assert_eq!(client.state, ClientState::Closed);
        assert!(!client.is_active());
    }

    #[test]
    fn reset_graphics_clears_surfaces_and_frame_tracking() {
        let mut client = GraphicsPipelineClient::new(Box::new(TestHandler), None);

        let _ = client.handle_pdu(GfxPdu::CreateSurface(crate::pdu::CreateSurfacePdu {
            surface_id: 1,
            width: 100,
            height: 100,
            pixel_format: PixelFormat::XRgb,
        }));
        assert_eq!(client.surfaces.len(), 1);

        // Simulate mid-stream state
        let _ = client.handle_pdu(GfxPdu::StartFrame(crate::pdu::StartFramePdu {
            timestamp: crate::pdu::Timestamp {
                milliseconds: 0,
                seconds: 0,
                minutes: 0,
                hours: 0,
            },
            frame_id: 42,
        }));
        assert!(client.current_frame_id.is_some());
        assert_eq!(client.frames_queued, 1);

        let _ = client.handle_pdu(GfxPdu::ResetGraphics(crate::pdu::ResetGraphicsPdu {
            width: 1920,
            height: 1080,
            monitors: vec![],
        }));

        assert!(client.surfaces.is_empty(), "surfaces should be cleared");
        assert!(client.current_frame_id.is_none(), "frame_id should be reset");
        assert_eq!(client.frames_queued, 0, "frame queue should be reset");
    }

    #[test]
    fn crop_decoded_frame_identity() {
        let data = vec![0xFFu8; 4 * 4 * 4];
        let cropped = crop_decoded_frame(&data, 4, 4, 4, 4);
        assert_eq!(cropped.len(), data.len());
    }

    #[test]
    fn crop_decoded_frame_macroblock_alignment() {
        // H.264 encodes 1920x1080 as 1920x1088 (rounded to 16-pixel macroblock boundary)
        let data = vec![0xAAu8; 1920 * 1088 * 4];
        let cropped = crop_decoded_frame(&data, 1920, 1088, 1920, 1080);
        assert_eq!(cropped.len(), 1920 * 1080 * 4);
    }

    #[test]
    fn convert_uncompressed_bgrx_to_rgba() {
        // Wire format: [B, G, R, A] per pixel (0xAARRGGBB little-endian)
        let wire_pixels = vec![
            0x00, 0x80, 0xFF, 0xCC, // B=0, G=128, R=255, A=204
            0x10, 0x20, 0x30, 0x40, // B=16, G=32, R=48, A=64
        ];
        let rgba = convert_uncompressed_to_rgba(&wire_pixels);
        // Expected: [R, G, B, 0xFF] per pixel (alpha forced to opaque)
        assert_eq!(rgba, vec![0xFF, 0x80, 0x00, 0xFF, 0x30, 0x20, 0x10, 0xFF]);
    }
}
