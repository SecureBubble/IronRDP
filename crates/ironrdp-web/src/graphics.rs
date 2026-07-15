//! Browser-side EGFX (MS-RDPEGFX) graphics pipeline handler.
//!
//! Tier 1: surface management + RFX **Progressive** and uncompressed/planar
//! bitmap paths (all decoded in pure Rust). No H.264/AVC (that needs a browser
//! WebCodecs decoder — Tier 2).
//!
//! # Rendering integration
//!
//! [`ironrdp_egfx::client::GraphicsPipelineHandler`] is `Send`, but the render
//! canvas (`web_sys` types) is `!Send` and is owned by the session run loop.
//! So the handler cannot draw directly. Instead it keeps every surface as an
//! RGBA buffer, applies all server operations (progressive decode, solid fill,
//! surface/cache copies) to those buffers, and — on frame completion — ships the
//! dirty region of each *output-mapped* surface to the run loop through the same
//! `input_events_tx` channel the clipboard uses. The run loop blits it to the
//! canvas (see [`crate::session::RdpInputEvent::Graphics`]).

use std::collections::HashMap;

use futures_channel::mpsc;
use ironrdp::graphics::progressive::ProgressiveDecoder;
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineHandler, Surface};
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface2Pdu,
};
use tracing::warn;

use crate::session::{GraphicsRegion, RdpInputEvent};

const TILE: u32 = 64;

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
            data: vec![0; (width.saturating_mul(height).saturating_mul(4)) as usize],
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
    progressive: ProgressiveDecoder,
    output_width: u32,
    output_height: u32,
}

impl WasmGraphicsHandler {
    pub(crate) fn new(proxy: WasmGraphicsMessageProxy) -> Self {
        Self {
            proxy,
            surfaces: HashMap::new(),
            cache: HashMap::new(),
            mapped: HashMap::new(),
            dirty: HashMap::new(),
            progressive: ProgressiveDecoder::new(),
            output_width: 0,
            output_height: 0,
        }
    }

    fn mark_dirty(&mut self, surface_id: u16, x: u32, y: u32, w: u32, h: u32) {
        let d = Dirty::union(self.dirty.get(&surface_id).copied(), x, y, w, h);
        self.dirty.insert(surface_id, d);
    }
}

impl GraphicsPipelineHandler for WasmGraphicsHandler {
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        self.output_width = width;
        self.output_height = height;
    }

    fn on_surface_created(&mut self, surface: &Surface) {
        self.surfaces
            .insert(surface.id, SurfaceBuf::new(u32::from(surface.width), u32::from(surface.height)));
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
        self.mapped.remove(&surface_id);
        self.dirty.remove(&surface_id);
    }

    fn on_surface_mapped(&mut self, surface_id: u16, origin_x: u32, origin_y: u32) {
        self.mapped.insert(surface_id, (origin_x, origin_y));
        // Repaint the whole surface on the next frame so it appears immediately.
        if let Some(s) = self.surfaces.get(&surface_id) {
            let (w, h) = (s.width, s.height);
            self.mark_dirty(surface_id, 0, 0, w, h);
        }
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

    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        let Some((sw, sh)) = self.surfaces.get(&pdu.surface_id).map(|s| (s.width, s.height)) else {
            return;
        };
        let tiles = match self.progressive.decode_bitmap(
            pdu.codec_context_id,
            sw.min(u32::from(u16::MAX)) as u16,
            sh.min(u32::from(u16::MAX)) as u16,
            &pdu.bitmap_data,
        ) {
            Ok(tiles) => tiles,
            Err(e) => {
                warn!(error = %e, "progressive decode failed");
                return;
            }
        };
        let Some(surface) = self.surfaces.get_mut(&pdu.surface_id) else {
            return;
        };
        let mut dirty: Option<Dirty> = None;
        for tile in &tiles {
            let tx = u32::from(tile.x_idx) * TILE;
            let ty = u32::from(tile.y_idx) * TILE;
            surface.blit(tx, ty, TILE, TILE, &tile.pixels, TILE);
            dirty = Some(Dirty::union(dirty, tx, ty, TILE, TILE));
        }
        if let Some(d) = dirty {
            self.mark_dirty(pdu.surface_id, d.min_x, d.min_y, d.max_x - d.min_x, d.max_y - d.min_y);
        }
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
        let Some(block) = self.surfaces.get(&pdu.source_surface_id).map(|s| s.extract(sx, sy, w, h)) else {
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

    fn on_frame_complete(&mut self, _frame_id: u32) {
        // Flush the dirty region of every output-mapped surface to the run loop.
        let dirty = core::mem::take(&mut self.dirty);
        for (surface_id, d) in dirty {
            let Some(&(ox, oy)) = self.mapped.get(&surface_id) else {
                continue; // off-screen composition surface: nothing to paint yet
            };
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
            let data = surface.extract(x, y, w, h);
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
    }
}
