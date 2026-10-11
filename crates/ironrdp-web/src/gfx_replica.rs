//! A graphics pipeline that renders a session it does not own.
//!
//! The live [`crate::session::Session`] can tap its graphics channel (the `gfx_segment_callback`
//! extension) and hand the host's eGFX stream, exactly as received, to another machine.
//! [`GfxReplica`] is the other end: the same [`GraphicsPipelineClient`] and
//! [`WasmGraphicsHandler`] the session runs (ZGFX decompressor included), fed those segments
//! instead of a socket, driving the same
//! WebGL present callbacks through [`crate::present`]. So the remote renderer decodes with exactly
//! our decoders, and the link carries ClearCodec / RFX Progressive / Planar / H.264 rather than
//! decoded RGBA.
//!
//! The replica owns no channel, so it sends nothing: it reports which frames are on screen and the
//! session (with the `remote_frame_acks` extension) acknowledges them to the host.
//! RemoteApp window positions arrive with Window List orders on the session's connection, not in
//! the graphics stream, so a replica presents full-desktop sessions only.

use futures_channel::mpsc;
use ironrdp_egfx::client::GraphicsPipelineClient;
use tracing::{debug, warn};
use wasm_bindgen::prelude::*;

use crate::graphics::{RailWindowStore, WasmGraphicsHandler, WasmGraphicsMessageProxy};
use crate::present;
use crate::session::RdpInputEvent;

#[wasm_bindgen]
pub struct GfxReplica {
    client: GraphicsPipelineClient,
    events: mpsc::UnboundedReceiver<RdpInputEvent>,
    surface_present: Option<js_sys::Function>,
    surface_layout: Option<js_sys::Function>,
    surface_copy: Option<js_sys::Function>,
    surface_cache: Option<js_sys::Function>,
    avc_decode: Option<js_sys::Function>,
    avc_watermark: Option<js_sys::Function>,
    decompressed_bytes: f64,
}

#[wasm_bindgen]
impl GfxReplica {
    /// The callbacks have the same signatures as the session extensions of the same names
    /// (`surface_present_callback`, ...), so a page wires them to the renderer exactly as it would
    /// for a local session.
    #[wasm_bindgen(constructor)]
    pub fn new(
        surface_present: Option<js_sys::Function>,
        surface_layout: Option<js_sys::Function>,
        surface_copy: Option<js_sys::Function>,
        surface_cache: Option<js_sys::Function>,
        avc_decode: Option<js_sys::Function>,
        avc_watermark: Option<js_sys::Function>,
    ) -> GfxReplica {
        let (tx, events) = mpsc::unbounded();
        // WebGL present mode, as for a session with `surface_present_callback`: regions go to JS
        // instead of being composited onto a 2D canvas here.
        let handler = WasmGraphicsHandler::new(WasmGraphicsMessageProxy::new(tx), RailWindowStore::default(), true, (0, 0));
        // AVC is decoded out of band (WebCodecs), as in the session.
        let client = GraphicsPipelineClient::new(Box::new(handler), None).advertise_avc(true);
        GfxReplica {
            client,
            events,
            surface_present,
            surface_layout,
            surface_copy,
            surface_cache,
            avc_decode,
            avc_watermark,
            decompressed_bytes: 0.0,
        }
    }

    /// Total size of the stream after ZGFX decompression, to compare with the bytes received.
    #[wasm_bindgen(getter, js_name = decompressedBytes)]
    pub fn decompressed_bytes(&self) -> f64 {
        self.decompressed_bytes
    }

    /// Decompress and decode one tapped segment (in channel order) and present what it produced.
    ///
    /// Returns the frames it completed that are now fully presented -- every frame without H.264.
    /// The page reports those to the session owner so the host is paced to this machine; H.264
    /// frames are reported by the WebCodecs decoder when they decode.
    pub fn push(&mut self, data: &[u8]) -> Vec<u32> {
        let completed = match self.client.process_segment(data) {
            Ok((frames, size)) => {
                #[expect(clippy::cast_precision_loss, reason = "statistics only")]
                {
                    self.decompressed_bytes += size as f64;
                }
                frames
            }
            Err(e) => {
                warn!(error = %e, "replica: graphics segment failed");
                Vec::new()
            }
        };
        while let Ok(event) = self.events.try_recv() {
            self.dispatch(event);
        }
        completed
    }
}

impl GfxReplica {
    fn dispatch(&self, event: RdpInputEvent) {
        match event {
            RdpInputEvent::Graphics(region) => {
                if let Some(cb) = &self.surface_present {
                    present::surface_region(cb, region.x, region.y, region.width, region.height, &region.data);
                }
            }
            RdpInputEvent::Avc(frame) => {
                if let Some(cb) = &self.avc_decode {
                    present::avc_frame(cb, &frame);
                }
            }
            RdpInputEvent::SurfaceLayout {
                mode,
                surface_w,
                surface_h,
                rects,
            } => {
                if let Some(cb) = &self.surface_layout {
                    present::surface_layout(cb, mode, surface_w, surface_h, &rects);
                }
            }
            RdpInputEvent::SurfaceCopy {
                src_x,
                src_y,
                width,
                height,
                points,
            } => {
                if let Some(cb) = &self.surface_copy {
                    present::surface_copy(cb, src_x, src_y, width, height, &points);
                }
            }
            RdpInputEvent::SurfaceCacheStore {
                cache_slot,
                src_x,
                src_y,
                width,
                height,
            } => {
                if let Some(cb) = &self.surface_cache {
                    present::surface_cache_store(cb, cache_slot, src_x, src_y, width, height);
                }
            }
            RdpInputEvent::SurfaceCacheRestore { cache_slot, points } => {
                if let Some(cb) = &self.surface_cache {
                    present::surface_cache_restore(cb, cache_slot, &points);
                }
            }
            RdpInputEvent::Watermark(wm) => {
                if let Some(cb) = &self.avc_watermark {
                    present::watermark(cb, &wm);
                }
            }
            // HiDef RAIL canvas sizing and capture-protect refusal are session concerns; the
            // replica presents a full desktop and the session enforces protection.
            _ => debug!("replica: event not presented"),
        }
    }
}
