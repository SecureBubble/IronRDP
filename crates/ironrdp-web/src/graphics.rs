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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures_channel::mpsc;
use ironrdp::graphics::alpha::decode_alpha_stream;
use ironrdp_egfx::client::{AvcFrame, BitmapUpdate, GraphicsPipelineHandler, Surface};
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, MapSurfaceToScaledOutputPdu, MapSurfaceToScaledWindowPdu, MapSurfaceToWindowPdu,
    ProtectSurfacePdu, SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu, WatermarkPdu,
};
use ironrdp_pdu::geometry::ExclusiveRectangle;
use tracing::{debug, trace, warn};

use crate::session::{AvcFrameEvent, GraphicsRegion, RdpInputEvent};

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
            // Zero/transparent init (proxy gen invariant): a fresh surface has NO valid pixels yet.
            // The host clears it to BLACK via SolidFill before painting, so a transparent init keeps
            // the tiny create→fill gap consistent (no white flash) and, where a region is presented
            // before any paint, it reads transparent/black rather than an opaque white block. Only
            // explicitly-invalidated regions are ever flushed (on_surface_mapped does NOT mark the
            // whole surface dirty), so unpainted init pixels don't leak — the historical black-block
            // bug was that whole-surface-dirty-on-map, not the init colour.
            data: vec![0x00; (width.saturating_mul(height).saturating_mul(4)) as usize],
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
#[derive(Clone)]
pub(crate) struct Watermark {
    /// QR tile, RGBA8888 (converted from the PDU's ARGB8888).
    pub(crate) rgba: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Repeat pitch of the tiling grid.
    pub(crate) cell_w: u32,
    pub(crate) cell_h: u32,
    /// QR offset within each cell.
    pub(crate) off_x: u32,
    pub(crate) off_y: u32,
    /// 8-bit blend strength (0..=255), derived from the PDU's 0..=10000 opacity.
    pub(crate) opacity: u32,
}

impl core::fmt::Debug for Watermark {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Omit the (large) rgba buffer from Debug output.
        f.debug_struct("Watermark")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("cell_w", &self.cell_w)
            .field("cell_h", &self.cell_h)
            .field("opacity", &self.opacity)
            .finish_non_exhaustive()
    }
}

/// Blend the tiled watermark onto a freshly extracted output region using a neutral,
/// background-opposing contrast so it stays legible on light *and* dark content.
/// `data` is tight RGBA for the `w`x`h` block whose top-left sits at output
/// coordinate `(out_x, out_y)`. Shared by the handler's flush and the run loop's
/// out-of-band AVC path (which composites decoded frames outside the surface buffer).
pub(crate) fn blend_watermark_into(wm: &Watermark, data: &mut [u8], out_x: u32, out_y: u32, w: u32, h: u32) {
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
            // NEUTRAL luminance delta whose sign opposes the background so the QR
            // stays legible on light and dark alike (see the handler note below).
            let lum = (u32::from(dpix[0]) * 77 + u32::from(dpix[1]) * 150 + u32::from(dpix[2]) * 29) >> 8;
            let d = a as i32;
            for c in 0..3 {
                let v = i32::from(dpix[c]);
                dpix[c] = if lum >= 128 { (v - d).max(0) } else { (v + d).min(255) } as u8;
            }
            // Leave alpha channel; the canvas forces opaque on present.
        }
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

    /// Ask the run loop to resize the render canvas to a HiDef RAIL window surface
    /// (piece 3). Sent before the window region flush for the frame so the canvas is
    /// sized before its pixels arrive.
    fn send_resize(&self, width: u32, height: u32) {
        if self
            .tx
            .unbounded_send(RdpInputEvent::GraphicsResize { width, height })
            .is_err()
        {
            warn!("Failed to send graphics resize, receiver is closed");
        }
    }

    /// Hand a compressed AVC main sub-stream to the run loop for out-of-band decode.
    fn send_avc(&self, frame: AvcFrameEvent) {
        if self.tx.unbounded_send(RdpInputEvent::Avc(frame)).is_err() {
            warn!("Failed to send AVC frame, receiver is closed");
        }
    }

    /// Hand the WebGL present layout (mode + app-window rects) to the run loop, which forwards it
    /// to the JS renderer. Only the `?ironwebgl=1` path emits these; JS clips the presented surface
    /// to `rects` (or blanks / presents full-screen per `mode`).
    fn send_layout(&self, mode: u8, surface_w: u32, surface_h: u32, rects: Vec<(u32, i32, i32, u32, u32)>) {
        if self
            .tx
            .unbounded_send(RdpInputEvent::SurfaceLayout {
                mode,
                surface_w,
                surface_h,
                rects,
            })
            .is_err()
        {
            warn!("Failed to send surface layout, receiver is closed");
        }
    }

    /// Hand a SurfaceToSurface copy to the run loop for GPU execution (WebGL path). The pixels
    /// being moved live only in the GPU texture, so the copy cannot be done here.
    fn send_cache_store(&self, cache_slot: u16, src_x: u32, src_y: u32, width: u32, height: u32) {
        if self
            .tx
            .unbounded_send(RdpInputEvent::SurfaceCacheStore { cache_slot, src_x, src_y, width, height })
            .is_err()
        {
            warn!("Failed to send surface cache store, receiver is closed");
        }
    }

    fn send_cache_restore(&self, cache_slot: u16, points: Vec<(u32, u32)>) {
        if self
            .tx
            .unbounded_send(RdpInputEvent::SurfaceCacheRestore { cache_slot, points })
            .is_err()
        {
            warn!("Failed to send surface cache restore, receiver is closed");
        }
    }

    fn send_copy(&self, src_x: u32, src_y: u32, width: u32, height: u32, points: Vec<(u32, u32)>) {
        if self
            .tx
            .unbounded_send(RdpInputEvent::SurfaceCopy {
                src_x,
                src_y,
                width,
                height,
                points,
            })
            .is_err()
        {
            warn!("Failed to send surface copy, receiver is closed");
        }
    }

    /// Forward the current watermark to the run loop so it can re-blend it onto
    /// out-of-band AVC regions (which bypass this handler's flush-time re-blend).
    fn send_watermark(&self, wm: Watermark) {
        if self.tx.unbounded_send(RdpInputEvent::Watermark(wm)).is_err() {
            warn!("Failed to send watermark, receiver is closed");
        }
    }

    /// Ask the run loop to fail-close a capture-protected session.
    fn refuse_protected_session(&self) {
        if self.tx.unbounded_send(RdpInputEvent::ProtectedSessionRefused).is_err() {
            warn!("Failed to send protected-session refusal, receiver is closed");
        }
    }
}

/// Records which RAIL window an eGFX surface is mapped to (HiDef RAIL, piece 2).
///
/// Populated from `RDPGFX_CMDID_MAPSURFACETOWINDOW` (0x0015) and
/// `RDPGFX_CMDID_MAPSURFACETOSCALEDWINDOW` (0x0018). Mirrors the FreeRDP
/// `gdiGfxSurface` window fields (`windowId`, `mappedWidth`/`mappedHeight`, and
/// `outputTargetWidth`/`outputTargetHeight` for the scaled variant). Window
/// mapping is mutually exclusive with output mapping (see [`WasmGraphicsHandler::mapped`]),
/// exactly as FreeRDP rejects a window map on an already output-mapped surface.
///
/// This is pure bookkeeping: nothing is composited from it yet. Piece 3
/// (per-window compositing) consumes [`WasmGraphicsHandler::window_mapped`] to
/// present each RAIL window from its owning surface.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowMapping {
    /// The RAIL window this surface backs (MS-RDPEGFX WindowId, 64-bit).
    pub(crate) window_id: u64,
    /// Region of the surface presented to the window.
    pub(crate) mapped_width: u32,
    pub(crate) mapped_height: u32,
    /// Scaled target size `(target_width, target_height)`; `Some` only for the
    /// scaled variant (`MapSurfaceToScaledWindow`), `None` for the 1:1 map.
    pub(crate) target: Option<(u32, u32)>,
}

/// On-screen position of a RAIL window (MS-RDPERP Window List), joined by window id to
/// the eGFX surface that backs it for HiDef RAIL per-window compositing. Coordinates are
/// virtual-desktop pixels and may be negative in RAIL, hence `i32`. `z` orders windows
/// back-to-front (higher = nearer the front / drawn last).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RailWindowPos {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) z: u32,
    /// Window size (desktop px). Needed by Path A (non-HiDef RAIL) to clip the single desktop
    /// surface to each app window's rect. HiDef ignores it (it sizes from the per-window surface
    /// mapping). 0 until a Window-List order supplies a size.
    pub(crate) w: u32,
    pub(crate) h: u32,
}

/// Shared HiDef-RAIL layout state written by the run loop (from decoded RAIL Window List
/// orders) and read by the graphics handler (in [`WasmGraphicsHandler::on_frame_complete`]).
#[derive(Default)]
struct RailSharedInner {
    /// RAIL `window_id` (u32 value widened to u64) -> its desktop position + z-order.
    positions: HashMap<u64, RailWindowPos>,
    /// The HOST's actual z-order, TOP-MOST FIRST, from the Window List order's window id list.
    /// Empty until the host sends one.
    server_zorder: Vec<u32>,
    /// Desktop-space top-left of the last composited bounding box (0,0 in desktop mode).
    bbox_origin: (i32, i32),
    /// Active HiDef-RAIL local move/size drag (MS-RDPERP ServerLocalMoveSize). While `Some`,
    /// the input path drives the window's position client-side instead of round-tripping every
    /// mouse move to the host.
    local_drag: Option<LocalDrag>,
    /// Path A: the input desktop is a Non-Monitored Desktop (Ctrl+Alt+Del / lock / UAC secure
    /// desktop). Set from the RAIL Desktop order's `DESKTOP_NONE` flag; while true the compositor
    /// presents the primary surface full-screen instead of clipping to the (irrelevant) RAIL
    /// window rects. Cleared when an Actively Monitored Desktop order returns.
    secure_desktop: bool,
}

/// An in-progress client-side window drag (Server Move/Size, MOVE). The host handed us the
/// move loop; we follow the cursor locally and send one WindowMove on release.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LocalDrag {
    /// RAIL window id (u32 widened to u64) being dragged.
    pub(crate) window_id: u64,
    /// Cursor position relative to the window top-left at grab (`posX/posY` from START):
    /// `new_top_left = mouse_desktop - (anchor_x, anchor_y)`.
    pub(crate) anchor_x: i32,
    pub(crate) anchor_y: i32,
    /// Full window size (for the final WindowMove rect: right/bottom = top_left + size).
    pub(crate) width: i32,
    pub(crate) height: i32,
}

/// `Send` + `Clone` handle to the shared HiDef-RAIL layout state.
///
/// The eGFX [`GraphicsPipelineHandler`] is `Send` and is moved into the DVC processor by
/// `connect()` before the [`crate::session::Session`] exists, so the run loop can never hold a
/// `&mut` to the handler. This handle is the bridge: the run loop clones it onto the session
/// and writes window positions via [`Self::set_rail_window`]/[`Self::remove_rail_window`];
/// the handler holds another clone and reads it while compositing. wasm is single-threaded,
/// so the `Mutex` never actually contends (it exists only to satisfy the `Send` bound).
#[derive(Clone, Default)]
pub(crate) struct RailWindowStore(Arc<Mutex<RailSharedInner>>);

impl RailWindowStore {
    /// Record (or update) a RAIL window's desktop rect (position + size) and z-order. Called from
    /// the run loop's Window List order loop on CreateWindow / UpdateWindow.
    pub(crate) fn set_rail_window(&self, window_id: u64, x: i32, y: i32, w: u32, h: u32, z: u32) {
        if let Ok(mut inner) = self.0.lock() {
            inner.positions.insert(window_id, RailWindowPos { x, y, z, w, h });
        }
    }

    /// The presentable app-window rects `(x, y, w, h, z)` for Path A clipping, sorted **bottom→top**
    /// by z-order. Windows with a zero dimension are skipped (they carry no pixels). The caller
    /// additionally drops the full-desktop shell window (a rect that spans the whole surface).
    pub(crate) fn app_windows(&self) -> Vec<(u32, i32, i32, u32, u32, u32)> {
        let Ok(inner) = self.0.lock() else {
            return Vec::new();
        };
        let zorder = &inner.server_zorder;
        // Rank by the HOST's z-order, not by `p.z`.
        //
        // `p.z` is a counter bumped on every window order, so it means "most recently updated",
        // not "on top". Two user-visible bugs came from that: a newly launched app dropped behind
        // as soon as any other window repainted (a blinking caret is enough), and the resize-edge
        // hit-test resolved against the wrong window where two overlap, because it walks this same
        // list. The host tells us the truth -- `active=Some(id) count=11` on every Window List
        // order -- and we were using it only to sort taskbar buttons.
        //
        // `server_zorder` is TOP-MOST FIRST and we need BOTTOM-FIRST for painting, hence the
        // inversion. Windows the host has not ranked get 0, i.e. the bottom, with `p.z` breaking
        // ties among them so their relative order stays stable rather than jittering.
        let rank = |id: u64| -> usize {
            u32::try_from(id)
                .ok()
                .and_then(|id| zorder.iter().position(|&z| z == id))
                .map_or(0, |pos| zorder.len() - pos)
        };
        let mut wins: Vec<(usize, u32, i32, i32, u32, u32, u32)> = inner
            .positions
            .iter()
            .filter(|(_, p)| p.w > 0 && p.h > 0)
            .map(|(&id, p)| (rank(id), u32::try_from(id).unwrap_or(0), p.x, p.y, p.w, p.h, p.z))
            .collect();
        wins.sort_by_key(|&(rank, .., z)| (rank, z));
        wins.into_iter().map(|(_, id, x, y, w, h, z)| (id, x, y, w, h, z)).collect()
    }

    /// Record the host's real z-order (top-most first), as carried by the Window List order.
    pub(crate) fn set_server_zorder(&self, ids: &[u32]) {
        if let Ok(mut inner) = self.0.lock() {
            inner.server_zorder = ids.to_vec();
        }
    }

    /// Move a RAIL window to a new desktop top-left, keeping its z-order. Used during a local
    /// drag (no z change). No-op if the window has no tracked position.
    pub(crate) fn move_rail_window(&self, window_id: u64, x: i32, y: i32) {
        if let Ok(mut inner) = self.0.lock() {
            if let Some(pos) = inner.positions.get_mut(&window_id) {
                pos.x = x;
                pos.y = y;
            }
        }
    }

    /// Read a window's current desktop top-left (for the final WindowMove rect).
    pub(crate) fn window_pos(&self, window_id: u64) -> Option<(i32, i32)> {
        self.0
            .lock()
            .ok()
            .and_then(|inner| inner.positions.get(&window_id).map(|p| (p.x, p.y)))
    }

    /// Begin a client-side local move drag. Replaces any prior drag.
    pub(crate) fn begin_local_drag(&self, drag: LocalDrag) {
        if let Ok(mut inner) = self.0.lock() {
            inner.local_drag = Some(drag);
        }
    }

    /// End the local drag; returns the drag that was active (if any).
    pub(crate) fn end_local_drag(&self) -> Option<LocalDrag> {
        self.0.lock().ok().and_then(|mut inner| inner.local_drag.take())
    }

    /// Peek the active local drag, if any.
    pub(crate) fn local_drag(&self) -> Option<LocalDrag> {
        self.0.lock().ok().and_then(|inner| inner.local_drag)
    }

    /// Set/clear the Non-Monitored (secure) desktop state from a RAIL Desktop order.
    pub(crate) fn set_secure_desktop(&self, on: bool) {
        if let Ok(mut inner) = self.0.lock() {
            inner.secure_desktop = on;
        }
    }

    /// Whether the input desktop is currently the Non-Monitored (secure) desktop.
    pub(crate) fn secure_desktop(&self) -> bool {
        self.0.lock().ok().is_some_and(|inner| inner.secure_desktop)
    }

    /// Drop a RAIL window's tracked position. Called on DeleteWindow.
    pub(crate) fn remove_rail_window(&self, window_id: u64) {
        if let Ok(mut inner) = self.0.lock() {
            inner.positions.remove(&window_id);
        }
    }

    /// Drop every tracked window position (session close).
    pub(crate) fn clear(&self) {
        if let Ok(mut inner) = self.0.lock() {
            inner.positions.clear();
            inner.local_drag = None;
            inner.secure_desktop = false;
        }
    }

    /// Snapshot the current positions for a compositing pass (releases the lock at once).
    fn positions(&self) -> HashMap<u64, RailWindowPos> {
        self.0.lock().map(|inner| inner.positions.clone()).unwrap_or_default()
    }

    /// Record the composited bbox origin so the input path can later offset by it.
    fn set_bbox_origin(&self, x: i32, y: i32) {
        if let Ok(mut inner) = self.0.lock() {
            inner.bbox_origin = (x, y);
        }
    }

    /// Read the last composited bbox origin (desktop coords of the canvas top-left). The
    /// input path adds this to outgoing mouse coordinates: the canvas is the bbox sub-region,
    /// so browser mouse coords are bbox-relative, but the host expects DESKTOP-absolute coords
    /// (see `Session::apply_inputs`).
    pub(crate) fn bbox_origin(&self) -> (i32, i32) {
        self.0.lock().map(|inner| inner.bbox_origin).unwrap_or_default()
    }
}

/// Whether two `(x, y, w, h)` rectangles overlap (share any pixel). Used to decide which windows
/// fall inside a composite's damaged region and must be redrawn.
fn rects_overlap(a: (u32, u32, u32, u32), b: (u32, u32, u32, u32)) -> bool {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    ax < bx.saturating_add(bw) && bx < ax.saturating_add(aw) && ay < by.saturating_add(bh) && by < ay.saturating_add(ah)
}

/// Bilinear scale of a tight RGBA8888 buffer from `sw`x`sh` to `dw`x`dh`.
///
/// Resamples a HiDef-RAIL window surface's mapped region to its (slightly different) scaled
/// window client size before compositing. The host maps each RemoteApp window at a size a few
/// percent off its target (e.g. 1125x664 -> 1152x704, ~1.02x1.06); nearest-neighbor on that
/// tiny scale visibly smears ClearType text, so we interpolate bilinearly to match mstsc /
/// FreeRDP's window blit. Callers skip this entirely when source == destination (the 1:1 fast
/// path), so unscaled windows stay pixel-exact. Center-aligned sampling with edge clamping.
fn scale_bilinear_rgba(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dw as usize).saturating_mul(dh as usize).saturating_mul(4)];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    let (sw_i, sh_i) = (sw as i64, sh as i64);
    let scale_x = sw as f32 / dw as f32;
    let scale_y = sh as f32 / dh as f32;
    for dy in 0..dh {
        // Map this destination pixel's CENTER back into source space, then take the two
        // straddling source rows and their vertical weight.
        let fy = (dy as f32 + 0.5) * scale_y - 0.5;
        let y0f = fy.floor();
        let wy = fy - y0f;
        let y0 = (y0f as i64).clamp(0, sh_i - 1);
        let y1 = (y0 + 1).min(sh_i - 1);
        for dx in 0..dw {
            let fx = (dx as f32 + 0.5) * scale_x - 0.5;
            let x0f = fx.floor();
            let wx = fx - x0f;
            let x0 = (x0f as i64).clamp(0, sw_i - 1);
            let x1 = (x0 + 1).min(sw_i - 1);

            let p00 = ((y0 * sw_i + x0) * 4) as usize;
            let p01 = ((y0 * sw_i + x1) * 4) as usize;
            let p10 = ((y1 * sw_i + x0) * 4) as usize;
            let p11 = ((y1 * sw_i + x1) * 4) as usize;
            let d = ((dy * dw + dx) * 4) as usize;
            if p00 + 4 > src.len()
                || p01 + 4 > src.len()
                || p10 + 4 > src.len()
                || p11 + 4 > src.len()
                || d + 4 > out.len()
            {
                continue;
            }
            let w00 = (1.0 - wx) * (1.0 - wy);
            let w01 = wx * (1.0 - wy);
            let w10 = (1.0 - wx) * wy;
            let w11 = wx * wy;
            for c in 0..4 {
                let v = src[p00 + c] as f32 * w00
                    + src[p01 + c] as f32 * w01
                    + src[p10 + c] as f32 * w10
                    + src[p11 + c] as f32 * w11;
                out[d + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// Snapshot of one composited HiDef-RAIL frame's geometry. Compared frame-to-frame to
/// decide when the canvas must be resized (and thereby cleared) and every window redrawn:
/// on any bbox move/resize, window move, z-order change, or window add/remove. A pure
/// content change (same layout) skips the resize so the canvas is not cleared/flickered.
#[derive(Clone, PartialEq, Eq)]
struct CompositeLayout {
    /// `(bbox_x, bbox_y, bbox_w, bbox_h)` in desktop coords.
    bbox: (i32, i32, u32, u32),
    /// Per-window `(surface_id, rel_x, rel_y, dst_w, dst_h)` in z-order (back to front),
    /// where `rel_*` are bbox-relative.
    windows: Vec<(u16, u32, u32, u32, u32)>,
}

/// EGFX pipeline handler. One per session, lives inside the DVC processor.
/// Bounding box of the pixels that a codec has actually written, within a tight RGBA block.
///
/// `SurfaceBuf` inits TRANSPARENT and every codec blit copies decoder bytes verbatim, so
/// `alpha == 0` means "no codec ever wrote this pixel" — a hole. That matters only on the WebGL
/// path, where AVC is decoded in JS straight into the GPU texture and never reaches this buffer:
/// the AVC-covered area is therefore a permanent hole here, and anything that SOURCES from it
/// (notably `SURFACE_TO_CACHE`, which the host uses heavily in AVC420 MixedMode) captures the hole
/// and later blits it back. Uploading that to the GPU paints opaque BLACK over pixels the GPU
/// already holds correctly.
///
/// Trimming each upload to its opaque bbox enforces the invariant "never upload pixels the
/// SurfaceBuf doesn't have", and does it WITHOUT tracking AVC coverage: it needs no expiry policy,
/// can't suppress genuine non-AVC content drawn inside the video rect (a menu over a video has real
/// opaque pixels), and self-corrects if the host changes which codec owns a region.
///
/// Returns `None` when nothing in the block was ever written. The bbox is exact for the straight
/// horizontal/vertical edges this actually produces (a cache tile straddling the video boundary);
/// an L-shaped opaque area would include a few transparent pixels, which is bounded and harmless.
fn opaque_bbox(data: &[u8], w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let (mut min_x, mut min_y) = (w, h);
    let (mut max_x, mut max_y) = (0_u32, 0_u32);
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            let idx = ((y * w + x) * 4 + 3) as usize;
            if data.get(idx).is_some_and(|&a| a != 0) {
                any = true;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    any.then(|| (min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
}

/// Present modes handed to the JS WebGL renderer alongside the app-window rects
/// ([`WasmGraphicsHandler::present_webgl`]). They mirror `composite_path_a`'s three modes; the JS
/// side must keep the same numbering (see `SurfaceRenderer.setLayout`).
///
/// Nothing is presented: the pre-first-app logon / "Preparing Windows" desktop, or no app window.
pub(crate) const WEBGL_LAYOUT_BLANK: u8 = 0;
/// The whole surface is presented unclipped: the secure desktop (Ctrl+Alt+Del / lock / UAC), which
/// the host paints full-screen with no RAIL window of its own.
pub(crate) const WEBGL_LAYOUT_FULLSCREEN: u8 = 1;
/// Normal RemoteApp presentation: present only the accompanying app-window rects.
pub(crate) const WEBGL_LAYOUT_CLIP: u8 = 2;

/// Above this many decoded rects in one frame, [`WasmGraphicsHandler::present_webgl`] uploads the
/// dirty bounding box instead — past this point the per-rect JS/WebGL call overhead dominates.
const WEBGL_MAX_RECTS: usize = 256;

pub(crate) struct WasmGraphicsHandler {
    proxy: WasmGraphicsMessageProxy,
    surfaces: HashMap<u16, SurfaceBuf>,
    /// eGFX tile cache. The bool records whether the cached block contains a TRANSPARENT HOLE,
    /// i.e. whether it straddles AVC-covered pixels that never reach the WASM buffer. That decides
    /// which restore path is correct on the WebGL path — see `on_cache_to_surface`.
    cache: HashMap<u16, (SurfaceBuf, bool)>,
    /// surface_id -> output origin (x, y) for surfaces mapped to the output.
    mapped: HashMap<u16, (u32, u32)>,
    /// Added to a RAIL window rect to move it from the host's DESKTOP space (primary-relative,
    /// negative to the left of/above the primary) into eGFX OUTPUT space (bounding-box top-left
    /// is the origin). `(0, 0)` for a single monitor and whenever the primary IS the top-left
    /// monitor -- which is why Path A clipping worked before multi-monitor. See
    /// `monitors_desktop_origin` in `session.rs`.
    desktop_origin: (i32, i32),
    /// surface_id -> RAIL window mapping, for surfaces mapped to a window
    /// instead of the output (HiDef RAIL). Mutually exclusive with `mapped`.
    /// Populated here (piece 2); piece 3's per-window compositing (`on_frame_complete`)
    /// joins it against [`Self::rail`] to place each window.
    pub(crate) window_mapped: HashMap<u16, WindowMapping>,
    /// Shared RAIL window positions (windowId -> desktop pos + z), written by the run loop
    /// from decoded Window List orders. Joined with `window_mapped` to composite every
    /// mapped window at its RAIL position (HiDef RAIL true multi-window compositing).
    rail: RailWindowStore,
    /// Geometry of the last composited HiDef-RAIL frame, to dedupe canvas resizes and to
    /// force a full z-order redraw whenever the layout (bbox / positions / z-order / window
    /// set) changes. `None` until the first composite.
    last_layout: Option<CompositeLayout>,
    /// surface_id -> accumulated dirty rect since the last frame flush.
    dirty: HashMap<u16, Dirty>,
    output_width: u32,
    output_height: u32,
    /// Active session watermark overlay (proxy extension), re-blended each flush.
    watermark: Option<Watermark>,
    /// A watermark PDU has been received this session, so the session MUST be watermarked.
    ///
    /// Latched separately from `watermark` because the two disagree in exactly the case that
    /// matters: a malformed or truncated PDU leaves `watermark` as None while the requirement
    /// stands. Fail CLOSED there -- present nothing rather than clean pixels. Note the proxy
    /// frames `imgSize` as a UINT16, so a tile over 65535 bytes truncates on the wire and lands
    /// in precisely this branch; before this flag that silently produced an unwatermarked session.
    watermark_required: bool,
    /// Latches once the proxy flags any surface capture-protected, so the
    /// fail-closed refusal is signalled exactly once (the proxy re-sends the
    /// PROTECT_SURFACE PDU after every surface map).
    capture_protected: bool,
    /// Green-border diagnostics: number of AVC frames whose region rects have been logged
    /// (at warn!) so the live console shows the first N frames' regions + dest without flooding.
    avc_region_log_count: u32,
    /// Path A (non-HiDef RAIL): the app-window rects `(x, y, w, h)` presented last frame, so a
    /// moved/closed window's vacated area can be cleared. `None` until the first Path A composite.
    /// An empty vec means the last present was the full-screen secure desktop or the blank welcome.
    last_path_a_layout: Option<Vec<(u32, i32, i32, u32, u32)>>,
    /// Path A: has a real app window ever been presentable this session? The pre-first-app logon /
    /// "Preparing Windows" desktop is ITSELF a Non-Monitored (secure) desktop, so the DESKTOP_NONE
    /// flag alone can't tell it apart from a real Ctrl+Alt+Del. This latch is the discriminator:
    /// full-screen the secure desktop only AFTER an app has been up (`secure && had_windows`);
    /// before that, stay blank. Set when the presentable-window set is first non-empty.
    path_a_had_windows: bool,
    /// Path A dirty-region diagnostics: number of composites whose window-center SurfaceBuf pixel
    /// has been logged (proxy's sanity check — "SurfaceBuf inside the video window must be video,
    /// not white"). Bounded so it doesn't flood.
    path_a_diag_count: u32,
    /// HiDef-RAIL join diagnostics: signature of the last logged compositor state, so the
    /// join summary (window-maps vs RAIL positions vs successful joins) is emitted only when
    /// it changes, not per frame. Lets a live console show exactly why a composite is empty.
    hidef_diag_sig: Option<String>,
    /// Latches once this is recognized as a RAIL/RemoteApp session (first window-map or RAIL
    /// window position). From then on the output-mapped desktop/welcome surface is NEVER
    /// presented — only the per-window compositor draws — so the host's partially-painted
    /// startup shell can't flash before the first RemoteApp window appears.
    rail_session: bool,
    /// `?ironwebgl=1`: JS owns the composite. The surface-0 WebGL texture — NOT this handler's
    /// [`SurfaceBuf`] — is the presentation target: the AVC video is decoded in JS and drawn
    /// straight into that texture, so it never lands in `surfaces`. Anything this handler pushes
    /// therefore OVERWRITES the video, which is why the WebGL path must send ONLY pixels a non-AVC
    /// codec just decoded ([`Self::webgl_rects`]) and must never re-extract a window/bbox from a
    /// `SurfaceBuf` that has a video-shaped hole in it (that was the black-blocks artifact).
    webgl_present: bool,
    /// WebGL path: the EXACT rects a non-AVC codec decoded into each surface this frame, in the
    /// order they were painted. The [`Self::dirty`] bounding box is unusable here — its gaps are
    /// AVC-owned pixels that only exist on the GPU, and uploading the box would black them out.
    /// Drained every frame by [`Self::present_webgl`].
    webgl_rects: HashMap<u16, Vec<(u32, u32, u32, u32)>>,
    /// WebGL path: the last (mode, window rects) layout handed to JS, to dedupe the callback.
    /// JS clips the presented surface to these rects, which is what makes a dragged window leave
    /// no ghost — the vacated area simply stops being presented (no clear plumbing needed).
    last_webgl_layout: Option<(u8, u32, u32, Vec<(u32, i32, i32, u32, u32)>)>,
    /// Per-window cache of the last bilinear-scaled RGBA buffer, keyed by surface id. During a
    /// local drag a window is recomposited every mouse-move but its CONTENT is unchanged (only
    /// its position moves), so re-running bilinear on identical pixels each frame is what made
    /// drag laggy. The compositor reuses this buffer whenever the surface isn't dirty and the
    /// scale is unchanged, re-scaling only on a real content update. Invalidated on
    /// surface delete/recreate/close.
    scaled_cache: HashMap<u16, ScaledBuf>,
}

/// A cached bilinear-scaled window buffer plus the geometry it was scaled for, so the compositor
/// can tell whether it is still valid (same source region and destination size).
struct ScaledBuf {
    mapped_w: u32,
    mapped_h: u32,
    dst_w: u32,
    dst_h: u32,
    data: Vec<u8>,
}

impl WasmGraphicsHandler {
    pub(crate) fn new(
        proxy: WasmGraphicsMessageProxy,
        rail: RailWindowStore,
        webgl_present: bool,
        desktop_origin: (i32, i32),
    ) -> Self {
        Self {
            desktop_origin,
            proxy,
            surfaces: HashMap::new(),
            cache: HashMap::new(),
            mapped: HashMap::new(),
            window_mapped: HashMap::new(),
            rail,
            last_layout: None,
            dirty: HashMap::new(),
            output_width: 0,
            output_height: 0,
            watermark: None,
            watermark_required: false,
            capture_protected: false,
            avc_region_log_count: 0,
            last_path_a_layout: None,
            path_a_had_windows: false,
            path_a_diag_count: 0,
            hidef_diag_sig: None,
            rail_session: false,
            scaled_cache: HashMap::new(),
            webgl_present,
            webgl_rects: HashMap::new(),
            last_webgl_layout: None,
        }
    }

    fn mark_dirty(&mut self, surface_id: u16, x: u32, y: u32, w: u32, h: u32) {
        let d = Dirty::union(self.dirty.get(&surface_id).copied(), x, y, w, h);
        self.dirty.insert(surface_id, d);
        // WebGL path: also keep the exact rect. See [`Self::webgl_rects`] — the union box is
        // lossy in a way that matters there (its gaps hold GPU-only AVC pixels).
        if self.webgl_present && w > 0 && h > 0 {
            self.webgl_rects.entry(surface_id).or_default().push((x, y, w, h));
        }
    }

    /// Re-blend the retained watermark onto a freshly extracted output region during the handler's
    /// own flush. The QR is a white module mask; the shared [`blend_watermark_into`] applies a
    /// neutral luminance delta whose sign opposes the background so it stays legible on light *and*
    /// dark content. No-op when no watermark is set.
    /// True when a watermark was mandated but none is usable — present nothing.
    fn watermark_blocked(&self) -> bool {
        self.watermark_required && self.watermark.is_none()
    }

    /// Tell JS a watermark was mandated and could not be prepared, so it can explain the blank
    /// screen instead of leaving the user staring at black.
    ///
    /// CONTRACT with `SurfaceRenderer.setWatermark`: a zero-width tile means "required but
    /// unavailable", never "clear the watermark". Rust only ever sends real tiles otherwise, so
    /// the zero case is free to carry this meaning.
    fn notify_watermark_blocked(&self) {
        self.proxy.send_watermark(Watermark {
            rgba: Vec::new(),
            width: 0,
            height: 0,
            cell_w: 0,
            cell_h: 0,
            off_x: 0,
            off_y: 0,
            opacity: 0,
        });
    }

    fn blend_watermark(&self, data: &mut [u8], out_x: u32, out_y: u32, w: u32, h: u32) {
        if let Some(wm) = &self.watermark {
            blend_watermark_into(wm, data, out_x, out_y, w, h);
        }
    }

    /// Blank the HiDef-RAIL canvas when it was showing composited windows but now has nothing
    /// to present — the last RemoteApp window closed, or none currently has a RAIL position.
    /// Without this the closed window's pixels linger on the canvas and the app looks
    /// un-closeable. Resizing to the desktop size clears the canvas (HTML semantics). Idempotent:
    /// `last_layout` is taken, so once blanked it won't re-clear every idle frame.
    fn blank_if_windows_gone(&mut self) {
        if self.last_layout.take().is_some() && self.output_width > 0 && self.output_height > 0 {
            self.proxy.send_resize(self.output_width, self.output_height);
        }
    }

    /// WebGL (`?ironwebgl=1`) presentation — the counterpart of [`Self::composite_path_a`] for the
    /// path where JS, not this handler, owns the composite.
    ///
    /// WHY A SEPARATE PATH: on `?ironwebgl=1` the AVC video is decoded on the JS main thread and
    /// drawn straight into the surface-0 WebGL texture; it NEVER returns as RGBA and so never
    /// reaches this handler's [`SurfaceBuf`]. The buffer therefore has a video-shaped hole in it
    /// (transparent init, which the present shader forces to opaque BLACK). `composite_path_a`
    /// re-extracts from that buffer — the whole window rect on every layout change, the dirty box
    /// otherwise — so on the WebGL path each of its uploads punched a black rectangle through the
    /// live video. That is the RAIL-drag artifact: drag the window, `layout_changed` fires, and the
    /// whole window is re-uploaded as black.
    ///
    /// So here we send ONLY pixels a non-AVC codec just decoded (the exact rects in
    /// [`Self::webgl_rects`], never the union box — its gaps are AVC-owned), and we do NOT clear,
    /// clip or repaint anything. Clipping to the app windows moves to the GPU: the layout below
    /// tells JS which rects to present, so a vacated area simply stops being presented and needs no
    /// clear at all. Two coordinate spaces stay identical to the CPU path (surface == desktop),
    /// so nothing else about the pipeline changes.
    fn present_webgl(&mut self) {
        // --- 1. Layout: the AUTHORITATIVE surface size, plus what JS should clip to. ---
        //
        // The size is the load-bearing part. JS used to size its GPU surface texture from the DOM
        // canvas (`base.width/height`), which is a DIFFERENT quantity that merely usually agrees
        // with the eGFX surface. When they disagreed the damage was PERMANENT: the host paints the
        // full desktop only in its opening frames and sends small delta regions forever after, so
        // any area that landed at the wrong size is never repainted — it stays at the transparent
        // init, which the present shader forces to opaque BLACK. That is the black-rectangle
        // artifact. The 2D path cannot hit it because its `SurfaceBuf` is sized by Rust from the
        // CreateSurface PDU. So we hand JS the real surface size and it uses nothing else.
        //
        // Sent for NON-RAIL sessions too (mode FULLSCREEN, no window rects) — a plain desktop needs
        // the size just as much; only the clipping is RAIL-specific.
        let wins: Vec<(u32, i32, i32, u32, u32)> = if self.rail_session {
            self.app_windows_in_output_space()
        } else {
            Vec::new()
        };
        let mode = if self.rail_session {
            if !wins.is_empty() {
                self.path_a_had_windows = true;
            }
            let secure = self.rail.secure_desktop();
            if secure && self.path_a_had_windows {
                WEBGL_LAYOUT_FULLSCREEN
            } else if secure || wins.is_empty() {
                WEBGL_LAYOUT_BLANK
            } else {
                WEBGL_LAYOUT_CLIP
            }
        } else {
            WEBGL_LAYOUT_FULLSCREEN
        };
        // Compare in place — this runs on every frame, so building a throwaway tuple to compare
        // against would allocate a Vec per frame just to find nothing changed.
        let (ow, oh) = (self.output_width, self.output_height);
        let changed = match &self.last_webgl_layout {
            Some((m, w, h, last_wins)) => *m != mode || *w != ow || *h != oh || last_wins != &wins,
            None => true,
        };
        if changed && ow > 0 && oh > 0 {
            debug!(
                mode,
                ow,
                oh,
                windows = wins.len(),
                desktop_origin = format!("{:?}", self.desktop_origin),
                rects = format!("{wins:?}"),
                "WebGL: surface layout -> JS"
            );
            self.proxy.send_layout(mode, ow, oh, wins.clone());
            self.last_webgl_layout = Some((mode, ow, oh, wins));
        }

        // --- 2. Upload this frame's freshly decoded non-AVC rects, and nothing else. ---
        self.flush_webgl_uploads();
    }

    /// Push this surface's freshly decoded non-AVC rects to the GPU.
    ///
    /// Split out of `present_webgl` because MS-RDPEGFX applies the commands inside a frame IN
    /// ORDER, and two of them (`SURFACE_TO_CACHE`, `SURFACE_TO_SURFACE`) READ the surface. Batching
    /// every upload to frame-complete breaks that: a cache store running mid-frame would snapshot
    /// a GPU texture that does not yet contain the paints which preceded it in the same frame, and
    /// silently cache stale pixels. So a reader flushes first, then reads.
    fn flush_webgl_uploads(&mut self) {
        let mapped_ids: Vec<u16> = self.mapped.keys().copied().collect();
        for surface_id in mapped_ids {
            let mut rects = self.webgl_rects.remove(&surface_id).unwrap_or_default();
            let bbox = self.dirty.remove(&surface_id);
            if rects.is_empty() {
                continue;
            }
            // Pathological rect counts (a text-heavy repaint can be thousands of tiny ClearCodec
            // blits) cost more in per-rect JS calls than one box upload. Coalescing is safe HERE
            // because a frame that dense has painted essentially all of the box anyway.
            if rects.len() > WEBGL_MAX_RECTS {
                let Some(d) = bbox else { continue };
                rects = vec![(
                    d.min_x,
                    d.min_y,
                    d.max_x.saturating_sub(d.min_x),
                    d.max_y.saturating_sub(d.min_y),
                )];
            }
            let Some((sw, sh)) = self.surfaces.get(&surface_id).map(|s| (s.width, s.height)) else {
                continue;
            };
            let (ox, oy) = self.mapped[&surface_id];
            for (rx, ry, rw, rh) in rects {
                let x = rx.min(sw);
                let y = ry.min(sh);
                let w = rx.saturating_add(rw).min(sw).saturating_sub(x);
                let h = ry.saturating_add(rh).min(sh).saturating_sub(y);
                if w == 0 || h == 0 {
                    continue;
                }
                let Some(probe) = self.surfaces.get(&surface_id).map(|s| s.extract(x, y, w, h)) else {
                    continue;
                };
                // Trim to what a codec actually wrote. Untouched pixels are transparent, and on this
                // path the AVC-covered area is a permanent hole in this buffer — uploading it would
                // paint opaque black over the live video the GPU already holds. See `opaque_bbox`.
                let Some((bx, by, bw, bh)) = opaque_bbox(&probe, w, h) else {
                    continue; // nothing here was ever written — leave the GPU's pixels alone
                };
                let (px, py, pw, ph) = (x + bx, y + by, bw, bh);
                // Crop the block we already extracted rather than extracting a second time.
                let data = if (bx, by, bw, bh) == (0, 0, w, h) {
                    probe
                } else {
                    let mut cropped = Vec::with_capacity((pw * ph * 4) as usize);
                    for row in 0..ph {
                        let start = (((by + row) * w + bx) * 4) as usize;
                        let end = start + (pw * 4) as usize;
                        match probe.get(start..end) {
                            Some(slice) => cropped.extend_from_slice(slice),
                            None => break,
                        }
                    }
                    if cropped.len() != (pw * ph * 4) as usize {
                        continue;
                    }
                    cropped
                };
                // NO watermark blend here. On this path JS owns the watermark, as a sibling canvas
                // layered above the present canvas (`SurfaceRenderer.setWatermark`) -- which is the
                // only way to cover AVC pixels, since those are decoded in JS and never reach this
                // buffer. Blending here as well would watermark the non-AVC chrome TWICE, showing
                // as darker patches exactly where the two overlap.
                self.proxy.send(GraphicsRegion {
                    x: ox + px,
                    y: oy + py,
                    width: pw,
                    height: ph,
                    data,
                    preserve_alpha: false,
                });
            }
        }
    }

    /// Presentable RAIL app-window rects, translated from the host's DESKTOP space into eGFX
    /// OUTPUT space so they can be compared with and clipped against surface geometry. Bottom→top
    /// by z, exactly as `RailWindowStore::app_windows` returns them.
    /// Presentable RAIL app-window rects in OUTPUT space, each with its window id.
    ///
    /// The id rides along because the client needs to know WHICH window an edge belongs to in
    /// order to resize it: the resize affordance is drawn client-side (the host delegates
    /// move/size to us via ALLOWLOCALMOVESIZE and sends no resize cursors), and the resulting
    /// `WindowMove` PDU is addressed by window id.
    fn app_windows_in_output_space(&self) -> Vec<(u32, i32, i32, u32, u32)> {
        let (dx, dy) = self.desktop_origin;
        self.rail
            .app_windows()
            .into_iter()
            .map(|(id, x, y, w, h, _z)| (id, x.saturating_add(dx), y.saturating_add(dy), w, h))
            .collect()
    }

    /// Send an OUTPUT-space rect, sourcing its pixels from whichever output-mapped surface(s)
    /// actually cover it.
    ///
    /// Path A used to read surface 0 directly, because with one monitor surface 0 IS the whole
    /// output and the two coordinate spaces are identical. With several monitors the output is
    /// tiled from one surface per monitor, each at its own origin, so a rect must be split: a
    /// window straddling the seam between two monitors emits one region per surface. Single
    /// monitor still yields exactly one region -- the old behaviour plus one intersection test.
    fn send_output_rect(&mut self, px: u32, py: u32, pw: u32, ph: u32) {
        let mapped: Vec<(u16, (u32, u32))> = self.mapped.iter().map(|(&id, &o)| (id, o)).collect();
        for (id, (ox, oy)) in mapped {
            let Some((sw, sh)) = self.surfaces.get(&id).map(|s| (s.width, s.height)) else {
                continue;
            };
            let ix0 = px.max(ox);
            let iy0 = py.max(oy);
            let ix1 = px.saturating_add(pw).min(ox.saturating_add(sw));
            let iy1 = py.saturating_add(ph).min(oy.saturating_add(sh));
            if ix1 <= ix0 || iy1 <= iy0 {
                continue;
            }
            let (iw, ih) = (ix1 - ix0, iy1 - iy0);
            let Some(mut data) = self.surfaces.get(&id).map(|s| s.extract(ix0 - ox, iy0 - oy, iw, ih)) else {
                continue;
            };
            // Path A has NEVER blended the watermark -- checked against the parent of the
            // multi-monitor commit, so this is an original gap, not a regression. It matters
            // because this path is reachable from the URL (`?ironwebgl=0`), and a watermark a user
            // can switch off by editing the address bar is not a control. The WebGL path gets its
            // watermark from the JS layer instead; only this CPU path needs the blend.
            self.blend_watermark(&mut data, ix0, iy0, iw, ih);
            self.proxy.send(GraphicsRegion {
                x: ix0,
                y: iy0,
                width: iw,
                height: ih,
                data,
                preserve_alpha: false,
            });
        }
    }

    /// Path A (MS-style non-HiDef RAIL) presentation.
    ///
    /// The RemoteApp rides ONE output-mapped desktop surface (surface 0) that already holds the
    /// host's z-ordered window composite. Present ONLY the real app-window rects — clipped from
    /// that surface — so the desktop background and shell chrome don't show (the RemoteApp look).
    /// Windows are drawn bottom→top so overlaps show the top window (matching what the surface
    /// already holds); the full-desktop shell window and 0×0 windows are dropped; and areas
    /// vacated by a moved/closed window are cleared so no trail lingers.
    fn composite_path_a(&mut self) {
        let (ow, oh) = (self.output_width, self.output_height);

        /// Clip a desktop rect to the `ow×oh` surface, returning `(x, y, w, h)` in surface pixels.
        fn clip(x: i32, y: i32, w: u32, h: u32, ow: u32, oh: u32) -> (u32, u32, u32, u32) {
            let x0 = x.max(0) as u32;
            let y0 = y.max(0) as u32;
            let x1 = (i64::from(x) + i64::from(w)).clamp(0, i64::from(ow)) as u32;
            let y1 = (i64::from(y) + i64::from(h)).clamp(0, i64::from(oh)) as u32;
            (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
        }

        // Presentable app-window rects, bottom→top by z. The set is already filtered at feed time
        // (is_presentable: NOT the WS_POPUP+WS_EX_TOOLWINDOW shell/helper class, NOT NOACTIVATE
        // chrome, non-zero size — and DELIBERATELY not gated on WS_VISIBLE, which the wire toggles
        // as noise). A live app window therefore stays in this set continuously; it leaves only on
        // DeleteWindow. When the set is empty no real app is up (startup / app closed) → blank.
        let wins: Vec<(u32, i32, i32, u32, u32)> = self.app_windows_in_output_space();
        if !wins.is_empty() {
            self.path_a_had_windows = true;
        }

        // What changed since the last present, drained for EVERY output-mapped surface.
        //
        // This used to be `self.dirty.remove(&0)` alone, on the premise (stated in the per-window
        // paint below) that "surface 0 IS the desktop". That premise dies under multi-monitor:
        // each physical monitor is its OWN surface, so a RemoteApp on the second monitor dirtied
        // surface 1, `dirty0` was None on every frame, and this function returned early having
        // presented nothing -- a frozen picture while frames kept arriving. Surface 1's dirty
        // region also accumulated forever, since nothing ever drained it.
        let mapped_ids: Vec<u16> = self.mapped.keys().copied().collect();
        let dirty_by_surface: Vec<(u16, Dirty)> = mapped_ids
            .iter()
            .filter_map(|&id| self.dirty.remove(&id).map(|d| (id, d)))
            .collect();
        // Union of every surface's dirty box, in OUTPUT coords, for the "did anything repaint at
        // all" gate and the per-window overlap test below.
        let dirty0: Option<Dirty> = dirty_by_surface
            .iter()
            .filter_map(|&(id, d)| {
                let (ox, oy) = self.mapped.get(&id).copied()?;
                Some(Dirty {
                    min_x: d.min_x.saturating_add(ox),
                    min_y: d.min_y.saturating_add(oy),
                    max_x: d.max_x.saturating_add(ox),
                    max_y: d.max_y.saturating_add(oy),
                })
            })
            .reduce(|a, b| Dirty {
                min_x: a.min_x.min(b.min_x),
                min_y: a.min_y.min(b.min_y),
                max_x: a.max_x.max(b.max_x),
                max_y: a.max_y.max(b.max_y),
            });
        let was_empty = self.last_path_a_layout.as_deref() == Some(&[][..]);
        let secure = self.rail.secure_desktop();

        // SECURE DESKTOP (Ctrl+Alt+Del / lock / UAC) full-screen — gated on `secure && had_windows`.
        //   * `secure` = the RAIL Non-Monitored Desktop order (DESKTOP_NONE), the ONE in-band signal
        //     of a desktop switch. It's LOAD-BEARING: the host leaves the app window WS_VISIBLE and
        //     tracked across the whole CAD (wire-proven — zero app-window orders between enter and
        //     restore), so `wins` stays non-empty and no window-visibility logic could detect CAD.
        //     Hide-tracking (WS_VISIBLE / showState) is wire-proven noise; ignore it entirely.
        //   * `had_windows` is REQUIRED too: the pre-first-app logon / "Preparing Windows" desktop is
        //     ALSO non-monitored, so DESKTOP_NONE fires there as well and can't distinguish it from a
        //     real CAD. Only "has a real app ever been up" tells them apart → full-screen after an
        //     app, blank before.
        // The host paints the secure desktop as a fresh FULL surface-0 frame (whole-desktop
        // invalidate on the switch); present on that frame (dirty0) so we never flash the pre-CAD
        // desktop. The restore (Actively Monitored Desktop order + fresh app Create) clears `secure`.
        if secure && self.path_a_had_windows {
            if dirty0.is_some() {
                self.send_output_rect(0, 0, ow, oh);
            }
            self.last_path_a_layout = Some(Vec::new());
            return;
        }

        // BLANK — either the pre-first-app logon / "Preparing Windows" non-monitored desktop
        // (`secure` set but no app ever up), or simply no presentable app window (app closed, or
        // startup chrome only). The host may be painting surface 0 (welcome wallpaper) but we must
        // NOT present it. Clear once on entry so a returning-from-CAD or closed-app frame doesn't
        // linger; then hold blank.
        if secure || wins.is_empty() {
            if !was_empty {
                self.proxy.send(GraphicsRegion {
                    x: 0,
                    y: 0,
                    width: ow,
                    height: oh,
                    data: vec![0u8; (ow * oh * 4) as usize],
                    preserve_alpha: true, // alpha 0 -> transparent -> reveals black desktop
                });
            }
            self.last_path_a_layout = Some(Vec::new());
            return;
        }

        // Returning to windowed presentation from an empty-set present (the secure desktop filled
        // the whole screen, or the blank welcome): clear the full surface first so no secure-desktop
        // pixels linger OUTSIDE the returning app window. The per-window paint below then redraws
        // each window's content on top.
        if was_empty {
            self.proxy.send(GraphicsRegion {
                x: 0,
                y: 0,
                width: ow,
                height: oh,
                data: vec![0u8; (ow * oh * 4) as usize],
                preserve_alpha: true, // alpha 0 -> transparent -> reveals black desktop
            });
        }

        let layout_changed = self.last_path_a_layout.as_deref() != Some(wins.as_slice());
        if !layout_changed && dirty0.is_none() {
            return; // nothing moved and nothing repainted
        }

        // Clear the area vacated by any window that moved or closed, so it leaves no trail.
        if layout_changed {
            if let Some(prev) = self.last_path_a_layout.take() {
                for (pid, px, py, pw, ph) in prev {
                    if !wins.contains(&(pid, px, py, pw, ph)) {
                        let (cx, cy, cw, ch) = clip(px, py, pw, ph, ow, oh);
                        if cw > 0 && ch > 0 {
                            self.proxy.send(GraphicsRegion {
                                x: cx,
                                y: cy,
                                width: cw,
                                height: ch,
                                data: vec![0u8; (cw * ch * 4) as usize],
                                preserve_alpha: true, // alpha 0 -> transparent -> reveals black desktop
                            });
                        }
                    }
                }
            }
        }

        // Draw each app window bottom→top. On a layout change repaint the WHOLE window; otherwise
        // present only the dirty region CLIPPED to the window — the video updates a small sub-rect,
        // and repainting the whole ~1000×760 window every frame is wasted CPU. Everything here is
        // in OUTPUT space: `wins` has been translated out of the host's desktop space, and the
        // dirty boxes have been translated out of each surface's local space, so they are directly
        // comparable. `send_output_rect` converts back per surface when it reads pixels. (dvc55 regressed here, but the real cause was the
        // video-freeze backpressure — fixed by the decode worker — not this coord math; the sanity
        // log below confirms SurfaceBuf holds video, not white, inside each window.)
        let diag = self.path_a_diag_count < 24 && !wins.is_empty();
        if diag {
            self.path_a_diag_count += 1;
        }
        for &(_id, x, y, w, h) in &wins {
            let (cx, cy, cw, ch) = clip(x, y, w, h, ow, oh);
            if cw == 0 || ch == 0 {
                continue;
            }
            let overlaps =
                dirty0.is_some_and(|d| cx < d.max_x && d.min_x < cx + cw && cy < d.max_y && d.min_y < cy + ch);

            // Proxy sanity check: after an AVC composite the SurfaceBuf pixel at the window centre
            // must be VIDEO, not the white (0xFF) init. `white=true` here means the composite wrote
            // to the wrong space — the exact failure mode behind the dvc55 white window.
            if diag && overlaps {
                // Read the surface that actually COVERS the window centre, not surface 0. Under
                // multi-monitor this probe reported a confident, wrong "centre is black" for
                // windows living on surface 1 -- a diagnostic that misleads is worse than none.
                let (mcx, mcy) = (cx + cw / 2, cy + ch / 2);
                let owner = self.mapped.iter().find_map(|(&id, &(ox, oy))| {
                    let (sw, sh) = self.surfaces.get(&id).map(|s| (s.width, s.height))?;
                    (mcx >= ox && mcy >= oy && mcx < ox + sw && mcy < oy + sh).then_some((id, ox, oy))
                });
                if let Some((s, ox, oy)) = owner.and_then(|(id, ox, oy)| self.surfaces.get(&id).map(|s| (s, ox, oy))) {
                    let px = s.extract(mcx - ox, mcy - oy, 1, 1);
                    let (r, g, b) = (
                        px.first().copied().unwrap_or(0),
                        px.get(1).copied().unwrap_or(0),
                        px.get(2).copied().unwrap_or(0),
                    );
                    debug!(
                        target: "avc_diag",
                        win = format!("({cx},{cy}) {cw}x{ch}"),
                        centre_rgb = format!("{r},{g},{b}"),
                        white = (r == 0xFF && g == 0xFF && b == 0xFF),
                        "Path A: SurfaceBuf centre pixel (want video, not white)"
                    );
                }
            }

            if !layout_changed && !overlaps {
                continue;
            }
            let (px, py, pw, ph) = if layout_changed {
                (cx, cy, cw, ch)
            } else if let Some(d) = dirty0 {
                // Intersect the window rect with the dirty box, all in surface/desktop coords.
                let ix0 = cx.max(d.min_x);
                let iy0 = cy.max(d.min_y);
                let ix1 = (cx + cw).min(d.max_x);
                let iy1 = (cy + ch).min(d.max_y);
                if ix1 <= ix0 || iy1 <= iy0 {
                    continue;
                }
                (ix0, iy0, ix1 - ix0, iy1 - iy0)
            } else {
                continue;
            };
            self.send_output_rect(px, py, pw, ph);
        }

        self.last_path_a_layout = Some(wins);
    }
}

impl GraphicsPipelineHandler for WasmGraphicsHandler {
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        self.output_width = width;
        self.output_height = height;
        // HARD RESET (proxy gen invariant). A ResetGraphics begins a NEW surface generation: the
        // host re-CREATEs, re-MAPs and repaints surface 0 (SolidFill BLACK is on the wire). Drop
        // every surface buffer, output map, cached scale and pending dirty so NO pixel from the old
        // generation can reach the screen, and null the compositor's on-screen latches so the stale
        // composite isn't carried across the resize. Present nothing until the new surface 0 is
        // created + mapped + painted. The old code kept the surfaces and blanked them to WHITE,
        // which both risked flashing white and let a stale composite persist. Session-level RAIL
        // state (window positions, had-windows, the secure flag) is intact — the app is still up
        // across a resolution change. RFX Progressive codec state is reset in the client core.
        self.surfaces.clear();
        self.mapped.clear();
        self.window_mapped.clear();
        self.scaled_cache.clear();
        self.dirty.clear();
        self.webgl_rects.clear();
        // Force a fresh layout push to JS: the canvas resize below reallocates (and clears) the
        // GPU surface texture, so JS must be told what to clip to for the new generation.
        self.last_webgl_layout = None;
        self.last_layout = None;
        self.last_path_a_layout = None;
        // Resize (and thereby clear) the canvas to the new desktop size; the new generation repaints
        // onto a clean surface.
        if width > 0 && height > 0 {
            self.proxy.send_resize(width, height);
        }
    }

    fn on_surface_created(&mut self, surface: &Surface) {
        // A CreateSurface for an id that already exists is a RECREATION (HiDef RAIL does
        // this: surface 0 is first created full-desktop for the startup frame, then
        // re-created per-window at the app's client size). It is a brand-new surface, so
        // every piece of state keyed by the old surface is stale and MUST be dropped —
        // otherwise a stale output mapping keeps the initial full-desktop frame latched on
        // the canvas while the recreated per-window content never gets presented. The new
        // buffer replaces the old below; here we purge the stale mappings/dirty so the
        // subsequent Map*/paint rebinds cleanly and the compositor re-evaluates. On a first
        // (non-reused) create these removes are no-ops.
        if self.surfaces.contains_key(&surface.id) {
            self.mapped.remove(&surface.id);
            self.window_mapped.remove(&surface.id);
            self.dirty.remove(&surface.id);
            self.webgl_rects.remove(&surface.id);
            self.scaled_cache.remove(&surface.id);
            self.last_layout = None;
        }
        self.surfaces.insert(
            surface.id,
            SurfaceBuf::new(u32::from(surface.width), u32::from(surface.height)),
        );
    }

    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
        self.mapped.remove(&surface_id);
        self.window_mapped.remove(&surface_id);
        self.dirty.remove(&surface_id);
        self.webgl_rects.remove(&surface_id);
        self.scaled_cache.remove(&surface_id);
        // The window this surface backed drops out of the next composite automatically (its
        // `window_mapped` entry is gone); the compositor sees the reduced window set as a
        // layout change and clears/redraws. We deliberately do NOT null `last_layout` here:
        // it is the "the canvas currently shows windows" signal the compositor needs to blank
        // the canvas when the LAST window closes — nulling it made a closed final window linger
        // (the app looked un-closeable). `rail` positions are cleared by the DeleteWindow order.

        // Proxy gen invariant: surface 0 is the output-mapped desktop surface (Path A rides it).
        // If it is deleted, its pixels must never keep showing (the drag-ghost leak). Its buffer is
        // already gone above, so no NEW present can source from it; also null the Path A latch and
        // clear the canvas so the pixels ALREADY on screen from the dead generation are wiped until
        // the surface is re-created and repainted. Scoped to surface 0 so ordinary multi-surface
        // delete/recreate churn (never the output surface) can't flicker.
        if surface_id == 0 {
            self.last_path_a_layout = None;
            // WebGL path: THE SURFACE TEXTURE MUST PERSIST. This clear is a full-surface zero-fill
            // sent through the ordinary pixel channel; on the 2D path it harmlessly blanks the
            // canvas (preserve_alpha -> transparent) and the canvas is re-composited from the
            // intact SurfaceBuf right after. On the WebGL path the same region becomes a
            // `texSubImage2D` of all-zero RGBA across the ENTIRE retained surface texture — it
            // WIPES the accumulated desktop — and `preserve_alpha` is meaningless there because the
            // present shader forces alpha opaque, so it lands as BLACK. Nothing repaints it
            // afterwards: past the opening frames the host sends only small delta regions (during a
            // drag, just the cursor), so every un-deltaed pixel stays black permanently. That is the
            // black-block artifact. The WebGL path needs no clear here anyway — it clears the
            // VISIBLE canvas on every present and redraws it from the texture, so dead-generation
            // pixels can never survive a frame on screen.
            if self.webgl_present {
                return;
            }
            if self.output_width > 0 && self.output_height > 0 {
                self.proxy.send(GraphicsRegion {
                    x: 0,
                    y: 0,
                    width: self.output_width,
                    height: self.output_height,
                    data: vec![0u8; (self.output_width * self.output_height * 4) as usize],
                    preserve_alpha: true, // alpha 0 -> transparent -> reveals black desktop
                });
            }
        }
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

    fn on_map_surface_to_window(&mut self, pdu: &MapSurfaceToWindowPdu) {
        // HiDef RAIL (piece 2): record which RAIL window this surface backs.
        // Output and window mapping are mutually exclusive, so drop any stale
        // output mapping (FreeRDP `gdi_MapSurfaceToWindow` rejects a window map
        // on an already output-mapped surface; we prefer the newest server intent).
        // No compositing happens here — piece 3 consumes `window_mapped`.
        self.mapped.remove(&pdu.surface_id);
        self.window_mapped.insert(
            pdu.surface_id,
            WindowMapping {
                window_id: pdu.window_id,
                mapped_width: pdu.mapped_width,
                mapped_height: pdu.mapped_height,
                target: None,
            },
        );
        debug!(
            surface_id = pdu.surface_id,
            window_id = pdu.window_id,
            mapped_width = pdu.mapped_width,
            mapped_height = pdu.mapped_height,
            "eGFX MapSurfaceToWindow (RAIL)"
        );
    }

    fn on_map_surface_to_scaled_window(&mut self, pdu: &MapSurfaceToScaledWindowPdu) {
        // Scaled variant of the above; additionally records the scaled target
        // size for piece 3's per-window resampling. Same output/window exclusivity.
        self.mapped.remove(&pdu.surface_id);
        self.window_mapped.insert(
            pdu.surface_id,
            WindowMapping {
                window_id: pdu.window_id,
                mapped_width: pdu.mapped_width,
                mapped_height: pdu.mapped_height,
                target: Some((pdu.target_width, pdu.target_height)),
            },
        );
        debug!(
            surface_id = pdu.surface_id,
            window_id = pdu.window_id,
            mapped_width = pdu.mapped_width,
            mapped_height = pdu.mapped_height,
            target_width = pdu.target_width,
            target_height = pdu.target_height,
            "eGFX MapSurfaceToScaledWindow (RAIL)"
        );
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

    fn on_avc_frame(&mut self, frame: &AvcFrame<'_>) {
        // The client does not decode H.264; forward the compressed main sub-stream to the run
        // loop, which hands it to the browser WebCodecs decoder. The H.264 picture is coded at
        // the FULL surface size and aligned to the surface origin, so ONLY `frame.regions`
        // (the metablock sub-rects) carry valid video — the rest is YUV(0,0,0) padding that
        // decodes to green. Forward the region rects in SURFACE space (which is also the
        // source coord space in the coded picture); the decoder blits only those, and the RGBA
        // returns through the per-surface `SurfaceBuf` path (`RdpInputEvent::AvcRegion`) so it
        // scales/positions via the compositor exactly like ClearCodec/Planar — fixing both the
        // green border and the HiDef RAIL offset/scale in one path.
        let dr = &frame.destination_rectangle;

        // Convert the region rects to (x, y, w, h). MS-RDPEGFX AVC region rects carry
        // EXCLUSIVE right/bottom (matching the WireToSurface1 dest rect and FreeRDP), so
        // width = right - left — NOT right - left + 1. (Verified against the s2_idr ground
        // truth: last video pixel is (895,703) for a (576,224)-(896,704) rect = 320×480.)
        // Using +1 yields odd dimensions that VideoFrame.copyTo rejects (chroma alignment)
        // and paints a 1px green seam. If the server sent no regions (shouldn't happen for
        // AVC), fall back to the whole destination rect so the frame is still shown.
        let mut regions: Vec<(u32, u32, u32, u32)> = frame
            .regions
            .iter()
            .map(|r| {
                (
                    u32::from(r.left),
                    u32::from(r.top),
                    u32::from(r.right.saturating_sub(r.left)),
                    u32::from(r.bottom.saturating_sub(r.top)),
                )
            })
            .filter(|&(_, _, w, h)| w > 0 && h > 0)
            .collect();
        if regions.is_empty() {
            regions.push((
                u32::from(dr.left),
                u32::from(dr.top),
                u32::from(dr.right.saturating_sub(dr.left)),
                u32::from(dr.bottom.saturating_sub(dr.top)),
            ));
        }

        // Output origin is only consumed by the GPU direct-draw path (multi-monitor,
        // output-mapped surfaces). The SurfaceBuf path positions via the compositor and ignores
        // it, so a window-mapped RAIL surface (absent from `self.mapped`) safely reports (0,0).
        let (origin_x, origin_y) = self.mapped.get(&frame.surface_id).copied().unwrap_or((0, 0));

        // Green-border diagnostic: log the first ~40 frames' region rects + dest (at warn!, so
        // it shows in a live console). If a frame ever reports a full-surface region or the
        // empty-regions fallback, that is the source of the green padding being blitted.
        if self.avc_region_log_count < 40 {
            self.avc_region_log_count += 1;
            let window_mapped = self.window_mapped.contains_key(&frame.surface_id);
            debug!(
                target: "avc_diag",
                surface_id = frame.surface_id,
                window_mapped,
                origin_x,
                origin_y,
                n_regions = regions.len(),
                regions = format!("{regions:?}"),
                dst = format!("({},{})-({},{})", dr.left, dr.top, dr.right, dr.bottom),
                "AVC frame regions"
            );
        }

        self.proxy.send_avc(AvcFrameEvent {
            surface_id: frame.surface_id,
            frame_id: frame.frame_id,
            origin_x,
            origin_y,
            regions,
            main_stream: frame.main_stream.to_vec(),
        });
    }

    fn on_alpha_update(&mut self, surface_id: u16, dest_rect: &ExclusiveRectangle, alpha_stream: &[u8]) {
        // ALPHA (RDPGFX_CODECID_ALPHA) rewrites ONLY the alpha channel of pixels already
        // present in the surface (drawn by a prior color codec in the same frame). RGB is
        // preserved. Mirrors FreeRDP `gdi_SurfaceCommand_Alpha` / `gdi_apply_alpha`.
        let x = u32::from(dest_rect.left);
        let y = u32::from(dest_rect.top);
        let w = u32::from(dest_rect.right.saturating_sub(dest_rect.left));
        let h = u32::from(dest_rect.bottom.saturating_sub(dest_rect.top));
        if w == 0 || h == 0 {
            return;
        }

        // Decode the ALPHA stream into a row-major buffer of one alpha byte per pixel
        // across the dest rect (pure parser in ironrdp-graphics).
        let alphas = match decode_alpha_stream(alpha_stream, w as u16, h as u16) {
            Ok(a) => a,
            Err(e) => {
                warn!(surface_id, error = %e, "ALPHA stream decode failed");
                return;
            }
        };

        let Some(surface) = self.surfaces.get_mut(&surface_id) else {
            warn!(surface_id, "ALPHA update for unknown surface");
            return;
        };
        let (sw, sh) = (surface.width, surface.height);

        for row in 0..h {
            let dy = y + row;
            if dy >= sh {
                break;
            }
            for col in 0..w {
                let dx = x + col;
                if dx >= sw {
                    break; // past the surface's right edge; next row
                }
                let Some(&a) = alphas.get((row * w + col) as usize) else {
                    continue;
                };
                // Write ONLY the alpha byte (RGBA layout: pixel*4 + 3), leaving RGB intact.
                let a_off = (((dy * sw + dx) * 4) + 3) as usize;
                if let Some(slot) = surface.data.get_mut(a_off) {
                    *slot = a;
                }
            }
        }

        // Mark the touched region dirty so it is presented on frame completion, exactly
        // like the color-decode paths. NOTE: `Canvas::draw` (canvas.rs) currently force-
        // sets alpha = 0xFF on blit for the opaque desktop path, so this per-pixel alpha
        // will NOT visibly reach the screen until per-window compositing honors surface
        // alpha. The surface buffer is updated correctly here; wiring the present path to
        // respect alpha for windowed (RAIL) surfaces is a separate step (HiDef RAIL).
        self.mark_dirty(surface_id, x, y, w, h);
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
        // Cache-poison diagnostic: same hazard as SurfaceToCache — a surface-to-surface copy
        // sourcing AVC-painted pixels copies the hole on the WebGL path. Counted below.
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

        // WebGL path: this copy MUST happen on the GPU, because the pixels it moves live only in
        // the GPU texture. `block` above was extracted from the WASM SurfaceBuf, which on this path
        // has a video-shaped hole where AVC painted — so blitting it and marking it dirty would
        // upload the HOLE (opaque black) AND fail to actually move the content, leaving the
        // original pixels in place. That is exactly the observed artifact when a window is dragged:
        // duplicated content plus black blocks, both persisting after the drag stops, because the
        // host uses a screen-to-screen copy to relocate the window instead of re-encoding it.
        // Note this is NOT the tile cache — SurfaceToCache/CacheToSurface are unused in this
        // session (wire-confirmed); SurfaceToSurface is a separate primitive.
        if self.webgl_present {
            // Flush first, for the same reason as the cache store: this copy READS the GPU texture,
            // and a frame's paints are otherwise not there yet when a mid-frame command runs.
            self.flush_webgl_uploads();
            // The GPU texture is ONE output-space framebuffer, but these coordinates are
            // SURFACE-local. They coincide only when the surface's output origin is (0,0) --
            // i.e. single monitor. Under multi-monitor each screen is its own surface, so a copy
            // on the second surface would read from and write to the wrong screen entirely
            // (window-shaped black blocks and stray outlines while dragging). Translate both ends
            // by their OWN surface's origin, which also keeps a cross-surface copy correct.
            let (src_ox, src_oy) = self.mapped.get(&pdu.source_surface_id).copied().unwrap_or((0, 0));
            let (dst_ox, dst_oy) = self.mapped.get(&pdu.destination_surface_id).copied().unwrap_or((0, 0));
            self.proxy.send_copy(
                sx.saturating_add(src_ox),
                sy.saturating_add(src_oy),
                w,
                h,
                points
                    .iter()
                    .map(|p| {
                        (
                            u32::from(p.x).saturating_add(dst_ox),
                            u32::from(p.y).saturating_add(dst_oy),
                        )
                    })
                    .collect(),
            );
            return;
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
            // Computed HERE, once per store, rather than per restore: restores outnumber stores
            // roughly 2:1 in a live session (measured 1047 vs 440).
            let has_hole = buf.data.chunks_exact(4).any(|px| px[3] == 0);
            self.cache.insert(pdu.cache_slot, (buf, has_hole));
        }
        // FLUSH FIRST. MS-RDPEGFX applies a frame's commands in order, so this store must capture
        // the paints that preceded it in this same frame. The WebGL path batches uploads to
        // frame-complete, so without this the GPU texture is one frame stale here and we would
        // cache the wrong pixels -- which showed up as small wrong blocks in window title bars.
        if self.webgl_present {
            self.flush_webgl_uploads();
        }
        // WebGL path: ALSO snapshot on the GPU, and treat that copy as authoritative.
        //
        // The CPU cache above is caching whatever the WASM SurfaceBuf holds -- and where AVC video
        // painted, that buffer holds a transparent HOLE, not the picture. Restoring such a block
        // later paints the hole back, which uploads as opaque black over live video.
        //
        // An older comment on `on_surface_to_surface` recorded that these cache commands were
        // "unused in this session (wire-confirmed)". That was true with the AVC444 GPO on -- the
        // host then encodes the whole surface as AVC444 and has nothing to cache. With that GPO
        // OFF the host switches to AVC420 plus heavy tile caching (measured: 902 CacheToSurface /
        // 491 SurfaceToCache against 104 AVC420 frames in one short session), which is exactly the
        // configuration that produced black rectangles all over the desktop.
        if self.webgl_present {
            let (ox, oy) = self.mapped.get(&pdu.surface_id).copied().unwrap_or((0, 0));
            self.proxy
                .send_cache_store(pdu.cache_slot, sx.saturating_add(ox), sy.saturating_add(oy), w, h);
        }
    }

    fn on_cache_to_surface(&mut self, pdu: &CacheToSurfacePdu) {
        let Some((w, h, block, has_hole)) = self
            .cache
            .get(&pdu.cache_slot)
            .map(|(c, hole)| (c.width, c.height, c.data.clone(), *hole))
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
        // WebGL path: restore from the GPU cache and DO NOT mark dirty.
        //
        // `mark_dirty` also pushes to `webgl_rects`, and a restore-heavy session (measured 1047
        // restores) would blow past WEBGL_MAX_RECTS, which swaps the exact rects for ONE
        // dirty-bbox-sized upload -- a box that spans the AVC hole and paints it black. That is a
        // large black rectangle, i.e. a worse version of the bug being fixed here.
        let _ = has_hole;
        if self.webgl_present {
            let (ox, oy) = self.mapped.get(&pdu.surface_id).copied().unwrap_or((0, 0));
            self.proxy.send_cache_restore(
                pdu.cache_slot,
                points
                    .iter()
                    .map(|p| {
                        (
                            u32::from(p.x).saturating_add(ox),
                            u32::from(p.y).saturating_add(oy),
                        )
                    })
                    .collect(),
            );
            return;
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
                "malformed watermark PDU — BLOCKING the session (fail closed)"
            );
            // THE single enforcement point. `WatermarkPdu::decode` is deliberately lenient so that
            // every malformation -- truncated body, wrapped UINT16 imgSize, header too short to
            // parse -- arrives HERE as a short image or a zero width, instead of failing to decode
            // and being dropped by the shared eGFX loop (which would present an unwatermarked
            // session). Do not "fix" that leniency without moving this check with it.
            // Do NOT just return: that would present an unwatermarked session, which is the one
            // outcome a watermark exists to prevent. Keep the requirement, leave `watermark` None,
            // and let `watermark_blocked` stop the present paths.
            self.watermark_required = true;
            self.notify_watermark_blocked();
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
        let wm = Watermark {
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
        };
        // Forward to the run loop so it can re-blend the mark onto out-of-band
        // AVC regions (which composite outside this handler's surface buffers and
        // therefore miss the flush-time re-blend below).
        self.watermark_required = true;
        self.proxy.send_watermark(wm.clone());
        self.watermark = Some(wm);
        // Repaint mapped surfaces fully so the watermark shows without waiting for
        // the server to touch every region.
        //
        // NOT on the WebGL path: marking the whole surface dirty makes `present_webgl` re-upload
        // the entire SurfaceBuf, and on that path the buffer has a video-shaped HOLE (AVC is
        // decoded in JS straight into the GPU texture and never lands here). Re-uploading it would
        // paint that hole over the live video as opaque black — the same failure as the
        // surface-delete clear above. Same rule: only ever send pixels a codec actually just
        // decoded.
        if self.webgl_present {
            return;
        }
        let mapped_ids: Vec<u16> = self.mapped.keys().copied().collect();
        for id in mapped_ids {
            if let Some(s) = self.surfaces.get(&id) {
                let (w, h) = (s.width, s.height);
                self.mark_dirty(id, 0, 0, w, h);
            }
        }
    }

    fn on_protect_surface(&mut self, pdu: &ProtectSurfacePdu) {
        // Fail closed. A browser canvas cannot be excluded from OS/browser screen
        // capture (unlike a native client's SetWindowDisplayAffinity), so instead
        // of showing protected content unprotected we refuse the session and tell
        // the user to use the native client. Signal once; the proxy re-sends this
        // after every surface map.
        if pdu.enable != 0 && !self.capture_protected {
            self.capture_protected = true;
            warn!("Session is capture-protected; refusing (browser cannot enforce screen-capture protection)");
            self.proxy.refuse_protected_session();
        }
    }

    fn on_frame_complete(&mut self, _frame_id: u32) {
        // FAIL CLOSED. One guard ahead of all three present paths (webgl / path A / CPU flush), so
        // no path can be added later that quietly presents unwatermarked pixels.
        if self.watermark_blocked() {
            if self.output_width > 0 && self.output_height > 0 {
                self.proxy.send(GraphicsRegion {
                    x: 0,
                    y: 0,
                    width: self.output_width,
                    height: self.output_height,
                    data: vec![0u8; (self.output_width * self.output_height * 4) as usize],
                    preserve_alpha: false,
                });
            }
            return;
        }

        // Latch a RAIL/RemoteApp session on the first window-map or RAIL window position. From
        // then on we present ONLY via the per-window compositor below and NEVER flush the
        // output-mapped desktop — so the host's partially-painted welcome/shell surface can't
        // flash during the ~1s startup gap before the first RemoteApp window maps (the proxy
        // confirmed those startup artifacts were us falling back to the unmapped desktop while
        // `joined=0`). On the latch, blank the canvas once to wipe any desktop frame the output
        // path already showed, so the gap is clean until the first window joins.
        if !self.rail_session && (!self.window_mapped.is_empty() || !self.rail.positions().is_empty()) {
            self.rail_session = true;
            if self.output_width > 0 && self.output_height > 0 {
                self.proxy.send_resize(self.output_width, self.output_height);
            }
        }

        // WebGL present (`?ironwebgl=1`): JS owns the composite (it holds the AVC video that never
        // reaches our SurfaceBuf), so we only feed it freshly decoded non-AVC rects plus the window
        // layout to clip to — no compositing here at all. See `present_webgl`. HiDef RAIL (window-
        // mapped surfaces) is not supported on this path and falls through to the CPU compositor.
        if self.webgl_present && self.window_mapped.is_empty() {
            self.present_webgl();
            return;
        }

        // Path A (MS-style non-HiDef RAIL): the RemoteApp rides ONE output-mapped desktop surface;
        // present only the app-window rects clipped from it (skip the desktop background + shell).
        // `composite_path_a` reads the surface + RAIL window rects directly, so we return before the
        // AVD desktop flush and the HiDef per-window compositor below. `rail_session` (latched on
        // the first RAIL order) with NO window-mapped surfaces is exactly the Path A case.
        if self.rail_session && self.window_mapped.is_empty() {
            self.composite_path_a();
            return;
        }

        // Flush the dirty region of every output-MAPPED surface to the run loop — but NOT in a
        // RAIL session (the desktop stays hidden; only the compositor presents, once a window
        // has joined). Unmapped surfaces keep their accumulated dirty region so it is painted
        // once they become mapped (FreeRDP's invalidRegion persists until blitted).
        // Path A (MS-style non-HiDef RAIL) and normal desktop/AVD sessions flush their
        // output-mapped surface(s) here. ONLY HiDef (per-window `MapSurfaceToWindow` surfaces
        // present) suppresses the desktop flush in a RAIL session — there the desktop is hidden
        // and only the per-window compositor presents. In Path A there are no window-mapped
        // surfaces, so the RemoteApp rides the single output-mapped desktop surface, which we DO
        // flush (Phase 2 will clip it to the RAIL Window-List rects).
        let mapped_ids: Vec<u16> = if self.rail_session && !self.window_mapped.is_empty() {
            Vec::new()
        } else {
            self.mapped.keys().copied().collect()
        };
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
            // Multi-monitor diagnostics: reveals whether a second output-mapped surface
            // (origin ox>0, e.g. 1280 for a side-by-side layout) actually flushes its
            // decoded region to the canvas, and at which virtual-desktop x-range. If a
            // monitor-2 window shows white, either no line appears here for that origin
            // (surface never mapped/dirtied) or one does but its source is unfilled.
            // Second-monitor flushes log at info! (visible at the default level) so a live
            // multimon session surfaces them without the per-frame primary-monitor flood.
            if ox > 0 {
                trace!(
                    surface_id,
                    origin_x = ox,
                    origin_y = oy,
                    out_x = ox + x,
                    out_y = oy + y,
                    w,
                    h,
                    surface_w = surface.width,
                    surface_h = surface.height,
                    "eGFX flush region to canvas (monitor 2)"
                );
            } else {
                trace!(
                    surface_id,
                    origin_x = ox,
                    out_x = ox + x,
                    w,
                    h,
                    surface_w = surface.width,
                    "eGFX flush region to canvas"
                );
            }
            // Re-blend the persistent watermark on top so it survives this region's
            // overwrite of the canvas.
            self.blend_watermark(&mut data, ox + x, oy + y, w, h);
            self.proxy.send(GraphicsRegion {
                x: ox + x,
                y: oy + y,
                width: w,
                height: h,
                data,
                preserve_alpha: false,
            });
        }

        // Plain desktop / AVD (non-RAIL, output-mapped, no window surfaces): the full-surface flush
        // above is the whole presentation — there is no per-window compositor to run. (Path A RAIL
        // already returned via `composite_path_a`; only HiDef, with window-mapped surfaces, falls
        // through to the per-window compositor below.)
        if self.window_mapped.is_empty() {
            return;
        }

        // --- HiDef RAIL: true multi-window compositing ---
        // In HiDef RAIL the host renders EACH RemoteApp window as its OWN eGFX surface
        // (MapSurfaceToWindow/ScaledWindow) with NO output/desktop surface, so the loop above
        // flushes nothing. Join every window-mapped surface to its RAIL window position (fed
        // by the run loop from Window List orders) and composite ALL mapped windows at their
        // desktop positions, cropping the canvas to their bounding box. Entirely gated on
        // window-mapped surfaces existing AND at least one having a RAIL position, so a normal
        // output-mapped desktop / legacy-RAIL / multimon session never enters here (its present
        // path above is untouched).
        let rail_positions = self.rail.positions();

        // Decisive join diagnostic (throttled to state changes): the frozen-startup-frame
        // symptom is the compositor producing nothing, which happens when window-maps and
        // RAIL positions fail to join. Emit at warn! (console-visible) a one-line summary of
        // BOTH sides of the join and the match count so the exact failure is unambiguous:
        //   maps=[surface->window] rail=[window@pos] joined=N/M
        // UNGATED (fires whenever EITHER side is non-empty, before the compositor early-out
        // below) so an empty window_mapped OR an empty rail set is itself visible — a silent
        // console would otherwise be ambiguous between "no maps" and "no positions".
        if !self.window_mapped.is_empty() || !rail_positions.is_empty() {
            let mut maps: Vec<(u16, u64)> = self.window_mapped.iter().map(|(&s, m)| (s, m.window_id)).collect();
            maps.sort_unstable();
            let mut rails: Vec<(u64, i32, i32)> = rail_positions.iter().map(|(&w, p)| (w, p.x, p.y)).collect();
            rails.sort_unstable();
            let joined = self
                .window_mapped
                .values()
                .filter(|m| rail_positions.contains_key(&m.window_id))
                .count();
            let sig = format!(
                "maps={:?} rail={:?} joined={}/{}",
                maps.iter().map(|(s, w)| format!("s{s}->w{w:#x}")).collect::<Vec<_>>(),
                rails
                    .iter()
                    .map(|(w, x, y)| format!("w{w:#x}@{x},{y}"))
                    .collect::<Vec<_>>(),
                joined,
                maps.len(),
            );
            if self.hidef_diag_sig.as_deref() != Some(sig.as_str()) {
                self.hidef_diag_sig = Some(sig.clone());
                warn!(target: "hidef_rail", "HiDef RAIL join: {sig}");
            }
        }

        // Nothing window-mapped -> normal output-mapped desktop / legacy-RAIL / multimon
        // session; its present path above is untouched and there is nothing to composite. In a
        // RAIL session this is also where the LAST window closing lands: blank any lingering
        // window image so the app doesn't look un-closeable.
        if self.window_mapped.is_empty() {
            self.blank_if_windows_gone();
            return;
        }

        // Presentable windows: window-mapped surfaces whose windowId has a known RAIL
        // position. The eGFX windowId is u64; the RAIL windowId is a u32 widened to u64 at
        // insert time, so the join is a direct HashMap lookup by numeric value.
        struct PresentWin {
            surface_id: u16,
            /// Valid painted region of the surface (its mapped sub-rect).
            mapped_w: u32,
            mapped_h: u32,
            /// Window client size to draw at (scaled target, else mapped).
            dst_w: u32,
            dst_h: u32,
            pos: RailWindowPos,
        }
        let mut wins: Vec<PresentWin> = Vec::new();
        for (&surface_id, m) in &self.window_mapped {
            let Some(&pos) = rail_positions.get(&m.window_id) else {
                continue; // surface with no RAIL position (e.g. the 0,0 shell) — ignore
            };
            let Some(surface) = self.surfaces.get(&surface_id) else {
                continue;
            };
            // Source = the mapped sub-rect [0,0,mapped_w,mapped_h], clamped to the surface.
            let mapped_w = m.mapped_width.min(surface.width);
            let mapped_h = m.mapped_height.min(surface.height);
            // Destination (window client) size: the scaled target if present, else mapped;
            // a degenerate 0 target falls back to the mapped size.
            let (mut dst_w, mut dst_h) = m.target.unwrap_or((mapped_w, mapped_h));
            if dst_w == 0 {
                dst_w = mapped_w;
            }
            if dst_h == 0 {
                dst_h = mapped_h;
            }
            if mapped_w == 0 || mapped_h == 0 || dst_w == 0 || dst_h == 0 {
                continue;
            }
            wins.push(PresentWin {
                surface_id,
                mapped_w,
                mapped_h,
                dst_w,
                dst_h,
                pos,
            });
        }
        if wins.is_empty() {
            // No window has BOTH a surface and a RAIL position. If we were showing windows and
            // they've all just lost their positions (e.g. closing), blank the lingering image.
            self.blank_if_windows_gone();
            return;
        }

        // Composite back-to-front: lowest z first; tie-break on surface_id for determinism.
        wins.sort_by_key(|w| (w.pos.z, w.surface_id));

        // Bounding box over all presentable windows, in desktop coords (x/y may be negative).
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for w in &wins {
            min_x = min_x.min(w.pos.x);
            min_y = min_y.min(w.pos.y);
            max_x = max_x.max(w.pos.x.saturating_add(w.dst_w as i32));
            max_y = max_y.max(w.pos.y.saturating_add(w.dst_h as i32));
        }
        let bbox_w = (max_x - min_x).max(0) as u32;
        let bbox_h = (max_y - min_y).max(0) as u32;
        if bbox_w == 0 || bbox_h == 0 {
            return;
        }

        // Present each window at its REAL desktop position on a FULL desktop-sized canvas, so
        // the frontend fit-scales the whole desktop to the viewport. A RemoteApp window then
        // appears at its natural size on a black desktop (classic RAIL sizing), instead of a
        // lone window cropped to its bounding box and blown up to fill the screen. Fall back to
        // the window bounding box only if the desktop size is not yet known (no ResetGraphics).
        let desktop_mode = self.output_width > 0 && self.output_height > 0;
        let (canvas_w, canvas_h, org_x, org_y) = if desktop_mode {
            (self.output_width, self.output_height, 0_i32, 0_i32)
        } else {
            (bbox_w, bbox_h, min_x, min_y)
        };

        // Layout signature: canvas + each window's canvas-relative dst rect, in z-order. Drives
        // both the resize dedup and the "force full redraw" decision.
        let layout = CompositeLayout {
            bbox: (org_x, org_y, canvas_w, canvas_h),
            windows: wins
                .iter()
                .map(|w| {
                    (
                        w.surface_id,
                        (w.pos.x - org_x).max(0) as u32,
                        (w.pos.y - org_y).max(0) as u32,
                        w.dst_w,
                        w.dst_h,
                    )
                })
                .collect(),
        };
        let layout_changed = self.last_layout.as_ref() != Some(&layout);
        let any_dirty = wins.iter().any(|w| self.dirty.contains_key(&w.surface_id));
        if !layout_changed && !any_dirty {
            return; // identical layout, no content changed — nothing to resend this frame
        }

        // In desktop mode the canvas IS the desktop, so browser mouse coords are already
        // desktop-absolute — no input offset needed, record (0,0). In the bbox fallback the
        // canvas is the bbox sub-region, so `Session::apply_inputs` adds (min_x,min_y) to
        // recover desktop-absolute coords (mirrors `canvas = desktop - origin`).
        self.rail.set_bbox_origin(org_x, org_y);

        debug!(
            windows = wins.len(),
            desktop_mode, canvas_w, canvas_h, org_x, org_y, layout_changed, "HiDef RAIL: composite"
        );
        // Green-border diagnostic: when the window layout changes, log each composited window's
        // placement (surface, desktop pos, mapped source size, scaled dst size). A video window
        // appearing twice, or at an unexpected top-left origin, shows up here.
        if layout_changed {
            for w in &wins {
                debug!(
                    target: "avc_diag",
                    surface_id = w.surface_id,
                    pos_x = w.pos.x,
                    pos_y = w.pos.y,
                    z = w.pos.z,
                    mapped_w = w.mapped_w,
                    mapped_h = w.mapped_h,
                    dst_w = w.dst_w,
                    dst_h = w.dst_h,
                    "composite window placement"
                );
            }
        }

        // Incremental redraw (the drag hot path). Redrawing EVERY window every mouse-move is what
        // keeps drag from being buttery, since only the dragged window actually moved. Instead:
        //   - resize (which clears the whole canvas) ONLY when the canvas dimensions change;
        //   - otherwise erase just the VACATED area of each window that moved/closed, and redraw
        //     only windows whose rect intersects the "damage" region (the changed old+new rects).
        // Back-to-front order within the damaged region keeps overlapping windows correct; a
        // window entirely outside the damage is left untouched on the canvas.
        let prev_canvas = self.last_layout.as_ref().map(|l| (l.bbox.2, l.bbox.3));
        let canvas_resized = prev_canvas != Some((canvas_w, canvas_h));

        // Current canvas-relative rects, and the previous frame's rects keyed by surface.
        let cur_rects: Vec<(u16, u32, u32, u32, u32)> = wins
            .iter()
            .map(|w| {
                (
                    w.surface_id,
                    (w.pos.x - org_x).max(0) as u32,
                    (w.pos.y - org_y).max(0) as u32,
                    w.dst_w,
                    w.dst_h,
                )
            })
            .collect();
        let prev_windows: Vec<(u16, u32, u32, u32, u32)> =
            self.last_layout.as_ref().map(|l| l.windows.clone()).unwrap_or_default();

        let mut clear_rects: Vec<(u32, u32, u32, u32)> = Vec::new();
        let mut damage: Vec<(u32, u32, u32, u32)> = Vec::new();
        if !canvas_resized {
            let prev_map: HashMap<u16, (u32, u32, u32, u32)> =
                prev_windows.iter().map(|&(s, x, y, w, h)| (s, (x, y, w, h))).collect();
            for &(s, nx, ny, nw, nh) in &cur_rects {
                let dirty = self.dirty.contains_key(&s);
                match prev_map.get(&s) {
                    Some(&prev) if prev == (nx, ny, nw, nh) && !dirty => {} // unchanged — no damage
                    Some(&(px, py, pw, ph)) => {
                        if (px, py, pw, ph) != (nx, ny, nw, nh) {
                            clear_rects.push((px, py, pw, ph)); // erase the trail at the old spot
                            damage.push((px, py, pw, ph));
                        }
                        damage.push((nx, ny, nw, nh));
                    }
                    None => damage.push((nx, ny, nw, nh)), // newly appeared window
                }
            }
            // Windows present last frame but gone now: erase them.
            for &(s, px, py, pw, ph) in &prev_windows {
                if !cur_rects.iter().any(|&(cs, ..)| cs == s) {
                    clear_rects.push((px, py, pw, ph));
                    damage.push((px, py, pw, ph));
                }
            }
        }

        if canvas_resized {
            // Resizing clears the whole canvas, so everything is redrawn below.
            self.proxy.send_resize(canvas_w, canvas_h);
        } else {
            for (x, y, w, h) in &clear_rects {
                self.proxy.send(GraphicsRegion {
                    x: *x,
                    y: *y,
                    width: *w,
                    height: *h,
                    data: vec![0u8; *w as usize * *h as usize * 4],
                    preserve_alpha: true, // alpha 0 -> transparent -> reveals the black desktop
                });
            }
        }

        // Draw back-to-front. `put_image_data` OVERWRITES (no alpha blend), so a back-to-front
        // redraw of the damaged region composites overlapping OPAQUE windows correctly.
        for w in &wins {
            let rel_x = (w.pos.x - org_x).max(0) as u32;
            let rel_y = (w.pos.y - org_y).max(0) as u32;
            // Skip windows outside the damaged region (steady/other windows during a drag).
            if !canvas_resized
                && !damage
                    .iter()
                    .any(|&d| rects_overlap(d, (rel_x, rel_y, w.dst_w, w.dst_h)))
            {
                continue;
            }
            // Reuse the cached scaled buffer when this window's CONTENT is unchanged (not dirty)
            // and the scale matches — the drag hot path, where only the position moves. Re-scale
            // (bilinear) only on a real content update. `dirty` still holds this frame's changed
            // surfaces here (it is cleared below), so it is the content-changed signal.
            let content_changed = self.dirty.contains_key(&w.surface_id);
            let cached_data: Option<Vec<u8>> = if content_changed {
                None
            } else {
                self.scaled_cache.get(&w.surface_id).and_then(|c| {
                    (c.mapped_w == w.mapped_w && c.mapped_h == w.mapped_h && c.dst_w == w.dst_w && c.dst_h == w.dst_h)
                        .then(|| c.data.clone())
                })
            };
            let data = match cached_data {
                Some(d) => d,
                None => {
                    let Some(surface) = self.surfaces.get(&w.surface_id) else {
                        continue;
                    };
                    let src = surface.extract(0, 0, w.mapped_w, w.mapped_h);
                    let scaled = if w.mapped_w == w.dst_w && w.mapped_h == w.dst_h {
                        src // 1:1 map — no resample
                    } else {
                        scale_bilinear_rgba(&src, w.mapped_w, w.mapped_h, w.dst_w, w.dst_h)
                    };
                    self.scaled_cache.insert(
                        w.surface_id,
                        ScaledBuf {
                            mapped_w: w.mapped_w,
                            mapped_h: w.mapped_h,
                            dst_w: w.dst_w,
                            dst_h: w.dst_h,
                            data: scaled.clone(),
                        },
                    );
                    scaled
                }
            };
            debug!(
                surface_id = w.surface_id,
                window_id = self
                    .window_mapped
                    .get(&w.surface_id)
                    .map(|m| m.window_id)
                    .unwrap_or_default(),
                pos_x = w.pos.x,
                pos_y = w.pos.y,
                z = w.pos.z,
                dst_w = w.dst_w,
                dst_h = w.dst_h,
                rel_x,
                rel_y,
                "HiDef RAIL: window"
            );
            self.proxy.send(GraphicsRegion {
                x: rel_x,
                y: rel_y,
                width: w.dst_w,
                height: w.dst_h,
                data,
                preserve_alpha: true,
            });
        }

        // We flushed each presentable window's FULL dst rect, so clear their dirty regions.
        for w in &wins {
            self.dirty.remove(&w.surface_id);
        }
        self.last_layout = Some(layout);
    }

    fn on_close(&mut self) {
        self.surfaces.clear();
        self.cache.clear();
        self.mapped.clear();
        self.window_mapped.clear();
        self.rail.clear();
        self.last_layout = None;
        self.dirty.clear();
        // BOTH, and the pairing matters. Clearing only the tile would leave `watermark_required`
        // latched with no watermark to satisfy it, i.e. `watermark_blocked()` true forever -- the
        // session would go blank on channel close and stay blank until a new watermark PDU
        // happened to arrive. The requirement is SESSION state, and this is where a session ends,
        // so it resets with everything else; the next session re-establishes it from its own PDU.
        self.watermark = None;
        self.watermark_required = false;
        self.rail_session = false;
        self.scaled_cache.clear();
        self.last_path_a_layout = None;
        self.path_a_had_windows = false;
        self.webgl_rects.clear();
        self.last_webgl_layout = None;
    }
}

#[cfg(test)]
mod opaque_bbox_tests {
    use super::opaque_bbox;

    /// Build a `w`x`h` RGBA block where `opaque` decides each pixel's alpha.
    fn block(w: u32, h: u32, opaque: impl Fn(u32, u32) -> bool) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                if opaque(x, y) {
                    let i = ((y * w + x) * 4) as usize;
                    v[i..i + 4].copy_from_slice(&[10, 20, 30, 255]);
                }
            }
        }
        v
    }

    #[test]
    fn fully_written_block_is_not_trimmed() {
        let b = block(8, 8, |_, _| true);
        assert_eq!(opaque_bbox(&b, 8, 8), Some((0, 0, 8, 8)));
    }

    /// A cache tile sourced entirely from the AVC region: never written here, so uploading it
    /// would paint black over the live video. Must be skipped outright.
    #[test]
    fn never_written_block_is_skipped() {
        let b = block(8, 8, |_, _| false);
        assert_eq!(opaque_bbox(&b, 8, 8), None);
    }

    /// THE border case from the proxy's AVC420 dump: a 64px tile straddling the video's top edge,
    /// so the upper rows hold real chrome and the lower rows are the AVC hole. Only the chrome rows
    /// may be uploaded — trimming them away is what left a permanent black strip above the video.
    #[test]
    fn tile_straddling_the_video_top_edge_keeps_only_the_chrome_rows() {
        let b = block(64, 64, |_, y| y < 32);
        assert_eq!(opaque_bbox(&b, 64, 64), Some((0, 0, 64, 32)));
    }

    /// Same, for a tile straddling the LEFT edge (the x=1600 column in the dump).
    #[test]
    fn tile_straddling_the_video_left_edge_keeps_only_the_chrome_columns() {
        let b = block(64, 64, |x, _| x < 32);
        assert_eq!(opaque_bbox(&b, 64, 64), Some((0, 0, 32, 64)));
    }
}

#[cfg(test)]
mod hidef_repro {
    //! Offline, deterministic reproduction of the HiDef RemoteApp "frozen startup frame"
    //! bug, driving the REAL parser -> codec -> compositor against the proxy's captured
    //! `Delete(0) -> Create(0, window-size)` fixture (no AVD, no browser).
    //!
    //! The fixture is a decompressed (post-ZGFX) RDPGFX command stream. It lives outside
    //! the repo (proxy hand-off, `~/Downloads`), so the test SKIPS (passes) when absent —
    //! it is a diagnostic harness, not a CI gate. Run it explicitly with:
    //!   cargo test -p ironrdp-web hidef_repro -- --nocapture --ignored
    use ironrdp_core::ReadCursor;
    use ironrdp_egfx::client::GraphicsPipelineClient;
    use ironrdp_egfx::pdu::GfxPdu;
    use ironrdp_pdu::decode_cursor;

    use super::*;

    const FIXTURE: &str = r"C:\Users\maord\Downloads\REPRO-delete-recreate-surface0.bin";

    // The three RAIL windows the proxy's dump identified, with their desktop offsets. The
    // minimal fixture only paints surface 0 -> window 0x1011a; the others are injected too so
    // the same harness also drives the full-session capture unchanged.
    const RAIL_WINDOWS: &[(u64, i32, i32)] = &[(0x1_011a, 490, 281), (0xc_00d4, 682, 393), (0x8_021a, 487, 279)];

    #[test]
    #[ignore = "reads an out-of-repo proxy fixture; run manually with --ignored --nocapture"]
    fn delete_recreate_surface0_replay() {
        let Ok(bytes) = std::fs::read(FIXTURE) else {
            eprintln!("SKIP: fixture not found at {FIXTURE}");
            return;
        };
        eprintln!("loaded {} bytes from {FIXTURE}", bytes.len());

        // --- Step 0: isolate the DECODER. Walk the stream ourselves and histogram GfxPdu
        // variants. Answers: does the fixture carry MapSurfaceToScaledWindow, and does OUR
        // GfxPdu::decode parse the whole stream (or bail partway)?
        {
            let mut cursor = ReadCursor::new(&bytes);
            let mut hist: std::collections::BTreeMap<&'static str, u32> = std::collections::BTreeMap::new();
            let mut n = 0u32;
            while !cursor.is_empty() {
                match decode_cursor::<GfxPdu>(&mut cursor) {
                    Ok(pdu) => {
                        *hist.entry(gfx_variant(&pdu)).or_insert(0) += 1;
                        n += 1;
                    }
                    Err(e) => {
                        eprintln!("DECODER STOPPED after {n} PDUs parsed: {e}");
                        break;
                    }
                }
            }
            eprintln!("--- PDU histogram ({n} parsed) ---");
            for (k, v) in &hist {
                eprintln!("  {k}: {v}");
            }
            eprintln!(
                "  MapSurfaceToWindow present: {}",
                hist.contains_key("MapSurfaceToWindow") || hist.contains_key("MapSurfaceToScaledWindow")
            );
        }

        // --- Step 1: drive the REAL client + compositor. Inject RAIL positions up-front so a
        // populated window_mapped WILL join (isolating "did window_mapped populate?" from "did
        // the live RAIL feed arrive?"). Then observe what actually gets presented via the
        // channel.
        let (tx, mut rx) = mpsc::unbounded();
        let proxy = WasmGraphicsMessageProxy::new(tx);
        let rail = RailWindowStore::default();
        // `set_rail_window` gained w/h when Path A started clipping to real window rects; this
        // fixture predates that. The size only has to be non-zero for `is_presentable`, and the
        // assertions below key off region WIDTH (>2000px = the pre-delete full-desktop frame), so
        // a nominal window-sized rect keeps the original classification intact.
        for (i, &(wid, x, y)) in RAIL_WINDOWS.iter().enumerate() {
            rail.set_rail_window(wid, x, y, 1024, 768, (i + 1) as u32);
        }
        // webgl_present = false: this fixture exercises the CPU compositor path.
        let handler = WasmGraphicsHandler::new(proxy, rail, false, (0, 0));
        let mut client = GraphicsPipelineClient::new(Box::new(handler), None);
        client
            .process_pdu_bytes_for_test(&bytes)
            .expect("replay should not hard-error");

        // Drain the presented events. Classify each: a >2000px-wide region is the pre-delete
        // full-desktop frame; a GraphicsResize + preserve_alpha region is a composited window.
        let mut n_resize = 0u32;
        let mut n_desktop = 0u32;
        let mut n_window = 0u32;
        let mut last_resize = None;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                RdpInputEvent::GraphicsResize { width, height } => {
                    n_resize += 1;
                    last_resize = Some((width, height));
                    eprintln!("PRESENT resize -> {width}x{height}");
                }
                RdpInputEvent::Graphics(r) => {
                    if r.width > 2000 {
                        n_desktop += 1;
                    }
                    if r.preserve_alpha {
                        n_window += 1;
                    }
                    eprintln!(
                        "PRESENT region x={} y={} {}x{} preserve_alpha={}",
                        r.x, r.y, r.width, r.height, r.preserve_alpha
                    );
                }
                _ => {}
            }
        }

        eprintln!("--- SUMMARY ---");
        eprintln!("desktop(full-screen) regions: {n_desktop}");
        eprintln!("window(composited) regions:   {n_window}");
        eprintln!("canvas resizes:               {n_resize} (last={last_resize:?})");
        eprintln!(
            "VERDICT: {}",
            if n_window > 0 {
                "compositor PRESENTED a window -> parser+compositor OK; field freeze is the LIVE RAIL feed (positions not arriving/joining)"
            } else {
                "NO window ever composited -> window_mapped never populated (parser/dispatch) OR no frame_complete after window paints"
            }
        );
    }

    fn gfx_variant(p: &GfxPdu) -> &'static str {
        match p {
            GfxPdu::WireToSurface1(_) => "WireToSurface1",
            GfxPdu::WireToSurface2(_) => "WireToSurface2",
            GfxPdu::DeleteEncodingContext(_) => "DeleteEncodingContext",
            GfxPdu::SolidFill(_) => "SolidFill",
            GfxPdu::SurfaceToSurface(_) => "SurfaceToSurface",
            GfxPdu::SurfaceToCache(_) => "SurfaceToCache",
            GfxPdu::CacheToSurface(_) => "CacheToSurface",
            GfxPdu::EvictCacheEntry(_) => "EvictCacheEntry",
            GfxPdu::CreateSurface(_) => "CreateSurface",
            GfxPdu::DeleteSurface(_) => "DeleteSurface",
            GfxPdu::StartFrame(_) => "StartFrame",
            GfxPdu::EndFrame(_) => "EndFrame",
            GfxPdu::FrameAcknowledge(_) => "FrameAcknowledge",
            GfxPdu::ResetGraphics(_) => "ResetGraphics",
            GfxPdu::MapSurfaceToOutput(_) => "MapSurfaceToOutput",
            GfxPdu::CacheImportOffer(_) => "CacheImportOffer",
            GfxPdu::CacheImportReply(_) => "CacheImportReply",
            GfxPdu::CapabilitiesAdvertise(_) => "CapabilitiesAdvertise",
            GfxPdu::CapabilitiesConfirm(_) => "CapabilitiesConfirm",
            GfxPdu::MapSurfaceToWindow(_) => "MapSurfaceToWindow",
            GfxPdu::QoeFrameAcknowledge(_) => "QoeFrameAcknowledge",
            GfxPdu::MapSurfaceToScaledOutput(_) => "MapSurfaceToScaledOutput",
            GfxPdu::MapSurfaceToScaledWindow(_) => "MapSurfaceToScaledWindow",
            GfxPdu::ProtectSurface(_) => "ProtectSurface",
            GfxPdu::Watermark(_) => "Watermark",
            _ => "Other",
        }
    }
}
