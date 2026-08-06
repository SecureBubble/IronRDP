//! Browser-side EGFX (MS-RDPEGFX) graphics pipeline **compositor**.
//!
//! Decoding lives in [`ironrdp_egfx::client::GraphicsPipelineClient`]: ClearCodec,
//! RFX Progressive, and uncompressed all decode in the client core and arrive here
//! pre-decoded as RGBA via [`GraphicsPipelineHandler::on_bitmap_updated`]. This handler
//! only composites — surface management, solid fill, surface/cache copies, and blitting
//! decoded regions. No H.264/AVC (that needs a browser WebCodecs decoder — Tier 2).
//!
//! # Rendering integration
//!
//! [`ironrdp_egfx::client::GraphicsPipelineHandler`] is `Send`, but the render
//! canvas (`web_sys` types) is `!Send` and is owned by the session run loop.
//! So the handler cannot draw directly. Instead it keeps every surface as an
//! RGBA buffer, applies all server operations (decoded bitmap blits, solid fill,
//! surface/cache copies) to those buffers, and — on frame completion — ships the
//! dirty region of each *output-mapped* surface to the run loop through the same
//! `input_events_tx` channel the clipboard uses. The run loop blits it to the
//! canvas (see [`crate::session::RdpInputEvent::Graphics`]).

use std::collections::HashMap;

use futures_channel::mpsc;
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineHandler, Surface};
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, MapSurfaceToScaledOutputPdu, ProtectSurfacePdu, SolidFillPdu, SurfaceToCachePdu,
    SurfaceToSurfacePdu, WatermarkPdu,
};
use tracing::warn;

use crate::session::{GraphicsRegion, RdpInputEvent};

/// A surface (or cached region) as a tightly packed RGBA8888 buffer.
struct SurfaceBuf {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl SurfaceBuf {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            // Opaque white init (matches FreeRDP gdi_CreateSurface memset 0xFF).
            data: vec![0xFF; (width.saturating_mul(height).saturating_mul(4)) as usize],
        }
    }

    /// Copy a `w`x`h` RGBA block into this buffer at `(x, y)`, clipping to bounds.
    /// `src` is row-major RGBA with stride `src_stride_px * 4`.
    fn blit(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8], src_stride_px: u32) {
        let sw = self.width;
        let sh = self.height;
        for row in 0..h {
            let dy = y + row;
            if dy >= sh || x >= sw {
                if dy >= sh {
                    break;
                }
                continue;
            }
            let copy_w = w.min(sw - x);
            let src_off = ((row * src_stride_px) * 4) as usize;
            let dst_off = ((dy * sw + x) * 4) as usize;
            let bytes = (copy_w * 4) as usize;
            if src_off + bytes > src.len() || dst_off + bytes > self.data.len() {
                continue;
            }
            self.data[dst_off..dst_off + bytes].copy_from_slice(&src[src_off..src_off + bytes]);
        }
    }

    /// Extract a `w`x`h` region at `(x, y)` into a tight RGBA buffer.
    fn extract(&self, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w * h * 4) as usize];
        for row in 0..h {
            let sy = y + row;
            if sy >= self.height || x >= self.width {
                continue;
            }
            let copy_w = w.min(self.width - x);
            let src_off = ((sy * self.width + x) * 4) as usize;
            let dst_off = ((row * w) * 4) as usize;
            let bytes = (copy_w * 4) as usize;
            if src_off + bytes > self.data.len() || dst_off + bytes > out.len() {
                continue;
            }
            out[dst_off..dst_off + bytes].copy_from_slice(&self.data[src_off..src_off + bytes]);
        }
        out
    }
}

/// A persistent, tiled watermark overlay pushed by the proxy
/// (`RDPGFX_CMDID_WATERMARK`). It is re-blended over every output region so it
/// survives frame repaints (which overwrite the canvas via `put_image_data`).
///
/// Placement is a reasoned hypothesis pending a live capture: the QR bitmap is
/// drawn at offset `(off_x, off_y)` inside a repeating `cell_w`x`cell_h` grid
/// aligned to the output origin (AVD-style tiling). `off_*` come from the PDU's
/// h/v padding, `cell_*` from its two constant words (0x017E/0x00CD). Only the
/// [`WasmGraphicsHandler::blend_watermark`] math depends on this reading, so it
/// is cheap to correct once verified.
struct Watermark {
    /// QR tile, RGBA8888 (converted from the PDU's ARGB8888).
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    /// Repeat pitch of the tiling grid.
    cell_w: u32,
    cell_h: u32,
    /// QR offset within each cell.
    off_x: u32,
    off_y: u32,
    /// 8-bit blend strength (0..=255), derived from the PDU's 0..=10000 opacity.
    opacity: u32,
}

/// Accumulated dirty rectangle in surface-local coordinates (exclusive max).
#[derive(Clone, Copy)]
struct Dirty {
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
}

impl Dirty {
    fn union(existing: Option<Dirty>, x: u32, y: u32, w: u32, h: u32) -> Dirty {
        let (nx0, ny0, nx1, ny1) = (x, y, x + w, y + h);
        match existing {
            Some(d) => Dirty {
                min_x: d.min_x.min(nx0),
                min_y: d.min_y.min(ny0),
                max_x: d.max_x.max(nx1),
                max_y: d.max_y.max(ny1),
            },
            None => Dirty {
                min_x: nx0,
                min_y: ny0,
                max_x: nx1,
                max_y: ny1,
            },
        }
    }
}

/// `Send` sink used by the handler to hand decoded regions to the run loop.
#[derive(Clone)]
pub(crate) struct WasmGraphicsMessageProxy {
    tx: mpsc::UnboundedSender<RdpInputEvent>,
}

impl WasmGraphicsMessageProxy {
    pub(crate) fn new(tx: mpsc::UnboundedSender<RdpInputEvent>) -> Self {
        Self { tx }
    }

    fn send(&self, region: GraphicsRegion) {
        if self.tx.unbounded_send(RdpInputEvent::Graphics(region)).is_err() {
            warn!("Failed to send graphics region, receiver is closed");
        }
    }
}

/// EGFX pipeline handler. One per session, lives inside the DVC processor.
pub(crate) struct WasmGraphicsHandler {
    proxy: WasmGraphicsMessageProxy,
    surfaces: HashMap<u16, SurfaceBuf>,
    cache: HashMap<u16, SurfaceBuf>,
    /// surface_id -> output origin (x, y) for surfaces mapped to the output.
    mapped: HashMap<u16, (u32, u32)>,
    /// surface_id -> accumulated dirty rect since the last frame flush.
    dirty: HashMap<u16, Dirty>,
    output_width: u32,
    output_height: u32,
    /// Active session watermark overlay (proxy extension), re-blended each flush.
    watermark: Option<Watermark>,
    /// Set once the proxy flags any surface capture-protected. Advisory only: a
    /// browser canvas cannot be truly excluded from OS/browser screen capture.
    /// Recorded as a hook for future cosmetic deterrents (e.g. blank-on-blur).
    #[allow(dead_code, reason = "advisory flag; browser cannot enforce capture protection yet")]
    capture_protected: bool,
}

impl WasmGraphicsHandler {
    pub(crate) fn new(proxy: WasmGraphicsMessageProxy) -> Self {
        Self {
            proxy,
            surfaces: HashMap::new(),
            cache: HashMap::new(),
            mapped: HashMap::new(),
            dirty: HashMap::new(),
            output_width: 0,
            output_height: 0,
            watermark: None,
            capture_protected: false,
        }
    }

    fn mark_dirty(&mut self, surface_id: u16, x: u32, y: u32, w: u32, h: u32) {
        let d = Dirty::union(self.dirty.get(&surface_id).copied(), x, y, w, h);
        self.dirty.insert(surface_id, d);
    }

    /// Alpha-blend the tiled watermark onto a freshly extracted output region.
    /// `data` is tight RGBA for the `w`x`h` block whose top-left sits at output
    /// coordinate `(out_x, out_y)`. No-op when no watermark is set.
    fn blend_watermark(&self, data: &mut [u8], out_x: u32, out_y: u32, w: u32, h: u32) {
        let Some(wm) = &self.watermark else { return };
        if wm.cell_w == 0 || wm.cell_h == 0 || wm.opacity == 0 {
            return;
        }
        for row in 0..h {
            let oy = out_y + row;
            let cy = oy % wm.cell_h;
            if cy < wm.off_y || cy >= wm.off_y + wm.height {
                continue; // this output row falls between watermark tiles
            }
            let wy = cy - wm.off_y;
            for col in 0..w {
                let ox = out_x + col;
                let cx = ox % wm.cell_w;
                if cx < wm.off_x || cx >= wm.off_x + wm.width {
                    continue;
                }
                let wx = cx - wm.off_x;
                let wsrc = ((wy * wm.width + wx) * 4) as usize;
                let Some(wpix) = wm.rgba.get(wsrc..wsrc + 4) else {
                    continue;
                };
                // Effective alpha = tile alpha scaled by the requested opacity.
                let a = (u32::from(wpix[3]) * wm.opacity) / 255;
                if a == 0 {
                    continue;
                }
                let didx = ((row * w + col) * 4) as usize;
                let Some(dpix) = data.get_mut(didx..didx + 4) else {
                    continue;
                };
                let inv = 255 - a;
                for c in 0..3 {
                    dpix[c] = ((u32::from(dpix[c]) * inv + u32::from(wpix[c]) * a) / 255) as u8;
                }
                // Leave alpha channel; the canvas forces opaque on present.
            }
        }
    }
}

impl GraphicsPipelineHandler for WasmGraphicsHandler {
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        self.output_width = width;
        self.output_height = height;
        // Match FreeRDP gdi_ResetGraphics: blank each existing surface to white and
        // clear its pending invalid region; keep the surfaces, output mappings and
        // the persistent bitmap cache. The RFX Progressive codec state is reset in the
        // client core's ResetGraphics handling, not here.
        for s in self.surfaces.values_mut() {
            s.data.iter_mut().for_each(|b| *b = 0xFF);
        }
        self.dirty.clear();
    }

    fn on_surface_created(&mut self, surface: &Surface) {
        self.surfaces.insert(
            surface.id,
            SurfaceBuf::new(u32::from(surface.width), u32::from(surface.height)),
        );
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
        self.mapped.remove(&surface_id);
        self.dirty.remove(&surface_id);
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        self.mapped.insert(surface_id, (origin_x, origin_y));
        // Do NOT mark the whole surface dirty here. FreeRDP only ever blits the
        // regions explicitly invalidated by decode/cache/fill ops; repainting the
        // entire surface buffer on map pushes never-rendered (white/incomplete)
        // tiles to the output — which, with the old black surface init, showed as
        // large black blocks after a RESET_GRAPHICS + surface re-create. Content
        // rendered before the map is retained via its own accumulated dirty region
        // (see on_frame_complete).
    }

    fn on_map_surface_to_scaled_output(&mut self, pdu: &MapSurfaceToScaledOutputPdu) {
        // Higher eGFX versions (V10.x) map surfaces to output via the *scaled*
        // variant even at 1:1 (DPI 100%). Without handling it, the surface is never
        // added to `mapped`, so `on_frame_complete` flushes nothing and the whole
        // screen stays black. Treat it as a normal output map at the given origin.
        // Non-1:1 scaling (target size != surface size) would need resampling at
        // flush time and is not yet supported — we map 1:1, which covers DPI 100%.
        if let Some(surface) = self.surfaces.get(&pdu.surface_id) {
            if pdu.target_width != surface.width || pdu.target_height != surface.height {
                warn!(
                    surface_id = pdu.surface_id,
                    target_w = pdu.target_width,
                    target_h = pdu.target_height,
                    surface_w = surface.width,
                    surface_h = surface.height,
                    "MapSurfaceToScaledOutput with non-1:1 scaling; mapping 1:1 (scaling unsupported)"
                );
            }
        }
        self.on_surface_mapped(pdu.surface_id, pdu.output_origin_x, pdu.output_origin_y);
    }

    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        if update.data.is_empty() {
            return; // decode skipped (e.g. AVC with no decoder)
        }
        let x = u32::from(update.destination_rectangle.left);
        let y = u32::from(update.destination_rectangle.top);
        let w = u32::from(update.width);
        let h = u32::from(update.height);
        if let Some(surface) = self.surfaces.get_mut(&update.surface_id) {
            surface.blit(x, y, w, h, &update.data, w);
        }
        self.mark_dirty(update.surface_id, x, y, w, h);
    }

    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        let c = &pdu.fill_pixel;
        let rgba = [c.r, c.g, c.b, 0xff];
        let rects: Vec<_> = pdu.rectangles.clone();
        if let Some(surface) = self.surfaces.get_mut(&pdu.surface_id) {
            for rect in &rects {
                let x = u32::from(rect.left);
                let y = u32::from(rect.top);
                let w = u32::from(rect.right.saturating_sub(rect.left));
                let h = u32::from(rect.bottom.saturating_sub(rect.top));
                let sw = surface.width;
                let sh = surface.height;
                for row in 0..h {
                    let dy = y + row;
                    if dy >= sh {
                        break;
                    }
                    let cw = w.min(sw.saturating_sub(x));
                    for col in 0..cw {
                        let off = (((dy * sw) + x + col) * 4) as usize;
                        if off + 4 <= surface.data.len() {
                            surface.data[off..off + 4].copy_from_slice(&rgba);
                        }
                    }
                }
            }
        }
        for rect in &rects {
            let x = u32::from(rect.left);
            let y = u32::from(rect.top);
            let w = u32::from(rect.right.saturating_sub(rect.left));
            let h = u32::from(rect.bottom.saturating_sub(rect.top));
            self.mark_dirty(pdu.surface_id, x, y, w, h);
        }
    }

    fn on_surface_to_surface(&mut self, pdu: &SurfaceToSurfacePdu) {
        let r = &pdu.source_rectangle;
        let (sx, sy) = (u32::from(r.left), u32::from(r.top));
        let w = u32::from(r.right.saturating_sub(r.left));
        let h = u32::from(r.bottom.saturating_sub(r.top));
        // Extract source into an owned buffer first (releases the source borrow).
        let Some(block) = self
            .surfaces
            .get(&pdu.source_surface_id)
            .map(|s| s.extract(sx, sy, w, h))
        else {
            return;
        };
        let points: Vec<_> = pdu.destination_points.clone();
        if let Some(dst) = self.surfaces.get_mut(&pdu.destination_surface_id) {
            for p in &points {
                dst.blit(u32::from(p.x), u32::from(p.y), w, h, &block, w);
            }
        }
        for p in &points {
            self.mark_dirty(pdu.destination_surface_id, u32::from(p.x), u32::from(p.y), w, h);
        }
    }

    fn on_surface_to_cache(&mut self, pdu: &SurfaceToCachePdu) {
        let r = &pdu.source_rectangle;
        let (sx, sy) = (u32::from(r.left), u32::from(r.top));
        let w = u32::from(r.right.saturating_sub(r.left));
        let h = u32::from(r.bottom.saturating_sub(r.top));
        if let Some(src) = self.surfaces.get(&pdu.surface_id) {
            let mut buf = SurfaceBuf::new(w, h);
            buf.data = src.extract(sx, sy, w, h);
            self.cache.insert(pdu.cache_slot, buf);
        }
    }

    fn on_cache_to_surface(&mut self, pdu: &CacheToSurfacePdu) {
        let Some((w, h, block)) = self
            .cache
            .get(&pdu.cache_slot)
            .map(|c| (c.width, c.height, c.data.clone()))
        else {
            // Cache miss: the server referenced a slot this (fresh) client never
            // filled. Leave the destination region as-is rather than painting garbage.
            return;
        };
        let points: Vec<_> = pdu.destination_points.clone();
        if let Some(dst) = self.surfaces.get_mut(&pdu.surface_id) {
            for p in &points {
                dst.blit(u32::from(p.x), u32::from(p.y), w, h, &block, w);
            }
        }
        for p in &points {
            self.mark_dirty(pdu.surface_id, u32::from(p.x), u32::from(p.y), w, h);
        }
    }

    fn on_watermark(&mut self, pdu: &WatermarkPdu) {
        let width = u32::from(pdu.width);
        let height = u32::from(pdu.height);
        let expected = (width as usize).saturating_mul(height as usize).saturating_mul(4);
        if width == 0 || height == 0 || pdu.image.len() < expected {
            warn!(
                width,
                height,
                image_len = pdu.image.len(),
                "ignoring malformed watermark PDU"
            );
            return;
        }
        // Convert the tile from ARGB8888 (wire byte order B,G,R,A) to RGBA8888.
        let mut rgba = vec![0u8; expected];
        for (dst, src) in rgba.chunks_exact_mut(4).zip(pdu.image.chunks_exact(4)) {
            dst[0] = src[2]; // R
            dst[1] = src[1]; // G
            dst[2] = src[0]; // B
            dst[3] = src[3]; // A
        }
        // Placement hypothesis (see `Watermark`): cell pitch from the two constant
        // words, QR offset from h/v padding. Fall back to a single tile if a cell
        // dimension is degenerate.
        let cell_w = if u32::from(pdu.reserved_a) > width {
            u32::from(pdu.reserved_a)
        } else {
            self.output_width.max(width)
        };
        let cell_h = if u32::from(pdu.reserved_b) > height {
            u32::from(pdu.reserved_b)
        } else {
            self.output_height.max(height)
        };
        self.watermark = Some(Watermark {
            rgba,
            width,
            height,
            cell_w,
            cell_h,
            off_x: u32::from(pdu.h_padding),
            off_y: u32::from(pdu.v_padding),
            // The proxy/AVD opacity is on a 0..=10000 "parts" scale (≈100 faint ..
            // ≈10000 opaque), NOT a 0..=255 alpha. Map it to an 8-bit blend factor
            // so a given value matches mstsc (e.g. 1000 -> ~10%, not fully opaque).
            opacity: u32::from(pdu.opacity).min(10_000) * 255 / 10_000,
        });
        // Repaint mapped surfaces fully so the watermark shows without waiting for
        // the server to touch every region.
        let mapped_ids: Vec<u16> = self.mapped.keys().copied().collect();
        for id in mapped_ids {
            if let Some(s) = self.surfaces.get(&id) {
                let (w, h) = (s.width, s.height);
                self.mark_dirty(id, 0, 0, w, h);
            }
        }
    }

    fn on_protect_surface(&mut self, pdu: &ProtectSurfacePdu) {
        // Advisory only: a browser cannot exclude its canvas from OS/browser screen
        // capture (unlike a native client's SetWindowDisplayAffinity). We record the
        // request but cannot enforce it.
        if pdu.enable != 0 {
            self.capture_protected = true;
        }
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        // Flush the dirty region of every output-MAPPED surface to the run loop.
        // Unmapped surfaces keep their accumulated dirty region so it is painted
        // once they become mapped (FreeRDP's invalidRegion persists until blitted).
        let mapped_ids: Vec<u16> = self.mapped.keys().copied().collect();
        for surface_id in mapped_ids {
            let Some(d) = self.dirty.remove(&surface_id) else {
                continue;
            };
            let (ox, oy) = self.mapped[&surface_id];
            let Some(surface) = self.surfaces.get(&surface_id) else {
                continue;
            };
            let x = d.min_x.min(surface.width);
            let y = d.min_y.min(surface.height);
            let w = d.max_x.min(surface.width).saturating_sub(x);
            let h = d.max_y.min(surface.height).saturating_sub(y);
            if w == 0 || h == 0 {
                continue;
            }
            let mut data = surface.extract(x, y, w, h);
            // Re-blend the persistent watermark on top so it survives this region's
            // overwrite of the canvas.
            self.blend_watermark(&mut data, ox + x, oy + y, w, h);
            self.proxy.send(GraphicsRegion {
                x: ox + x,
                y: oy + y,
                width: w,
                height: h,
                data,
            });
        }
    }

    fn on_close(&mut self) {
        self.surfaces.clear();
        self.cache.clear();
        self.mapped.clear();
        self.dirty.clear();
        self.watermark = None;
    }
}
