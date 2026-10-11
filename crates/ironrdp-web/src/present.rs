//! The JS side of the WebGL present path: how each graphics-handler event becomes a call into the
//! page's renderer.
//!
//! Shared by the live [`crate::session::Session`] run loop and by [`crate::gfx_replica::GfxReplica`],
//! which renders a graphics stream whose session lives elsewhere. Keeping the argument shapes in one
//! place is what guarantees the two drive the renderer identically. Every call is best-effort: a
//! throwing callback is logged and never influences protocol or frame-ack state.

use tracing::warn;
use wasm_bindgen::JsValue;

use crate::graphics::Watermark;
use crate::session::AvcFrameEvent;

/// Hands a decoded NON-AVC region's RGBA to the renderer (surface-0 texture upload).
pub(crate) fn surface_region(cb: &js_sys::Function, x: u32, y: u32, width: u32, height: u32, data: &[u8]) {
    let args = js_sys::Array::from_iter([
        JsValue::from_f64(f64::from(x)),
        JsValue::from_f64(f64::from(y)),
        JsValue::from_f64(f64::from(width)),
        JsValue::from_f64(f64::from(height)),
        js_sys::Uint8Array::from(data).into(),
    ]);
    if let Err(err) = cb.apply(&JsValue::NULL, &args) {
        warn!(?err, "surface_present callback threw");
    }
}

/// The presentation layout: a mode plus the RemoteApp window rects, flattened as FIVE ints per
/// window (id, x, y, w, h) -- the id is what lets the client address a resize to the right window.
pub(crate) fn surface_layout(
    cb: &js_sys::Function,
    mode: u8,
    surface_w: u32,
    surface_h: u32,
    rects: &[(u32, i32, i32, u32, u32)],
) {
    let flat = js_sys::Int32Array::new_with_length(u32::try_from(rects.len() * 5).unwrap_or(0));
    for (i, &(id, x, y, w, h)) in rects.iter().enumerate() {
        let base = u32::try_from(i * 5).unwrap_or(0);
        flat.set_index(base, i32::try_from(id).unwrap_or(0));
        flat.set_index(base + 1, x);
        flat.set_index(base + 2, y);
        flat.set_index(base + 3, i32::try_from(w).unwrap_or(0));
        flat.set_index(base + 4, i32::try_from(h).unwrap_or(0));
    }
    let args = js_sys::Array::from_iter([
        JsValue::from_f64(f64::from(mode)),
        JsValue::from_f64(f64::from(surface_w)),
        JsValue::from_f64(f64::from(surface_h)),
        flat.into(),
    ]);
    if let Err(err) = cb.apply(&JsValue::NULL, &args) {
        warn!(?err, "surface_layout callback threw");
    }
}

fn flat_points(points: &[(u32, u32)]) -> js_sys::Int32Array {
    let flat = js_sys::Int32Array::new_with_length(u32::try_from(points.len() * 2).unwrap_or(0));
    for (i, &(x, y)) in points.iter().enumerate() {
        let base = u32::try_from(i * 2).unwrap_or(0);
        flat.set_index(base, i32::try_from(x).unwrap_or(0));
        flat.set_index(base + 1, i32::try_from(y).unwrap_or(0));
    }
    flat
}

/// eGFX SurfaceToSurface on the GPU: copy the block at (`src_x`,`src_y`) to each point.
pub(crate) fn surface_copy(cb: &js_sys::Function, src_x: u32, src_y: u32, width: u32, height: u32, points: &[(u32, u32)]) {
    let args = js_sys::Array::from_iter([
        JsValue::from_f64(f64::from(src_x)),
        JsValue::from_f64(f64::from(src_y)),
        JsValue::from_f64(f64::from(width)),
        JsValue::from_f64(f64::from(height)),
        flat_points(points).into(),
    ]);
    if let Err(err) = cb.apply(&JsValue::NULL, &args) {
        warn!(?err, "surface_copy callback threw");
    }
}

/// eGFX SurfaceToCache on the GPU. One callback carries store and restore: `points` empty means
/// STORE, so the two can never disagree about slot semantics.
pub(crate) fn surface_cache_store(cb: &js_sys::Function, cache_slot: u16, src_x: u32, src_y: u32, width: u32, height: u32) {
    let args = js_sys::Array::from_iter([
        JsValue::from_f64(f64::from(cache_slot)),
        JsValue::from_f64(f64::from(src_x)),
        JsValue::from_f64(f64::from(src_y)),
        JsValue::from_f64(f64::from(width)),
        JsValue::from_f64(f64::from(height)),
        js_sys::Int32Array::new_with_length(0).into(),
    ]);
    if let Err(err) = cb.apply(&JsValue::NULL, &args) {
        warn!(?err, "surface_cache callback threw (store)");
    }
}

/// eGFX CacheToSurface on the GPU: blit a cache slot to each destination point.
pub(crate) fn surface_cache_restore(cb: &js_sys::Function, cache_slot: u16, points: &[(u32, u32)]) {
    let args = js_sys::Array::from_iter([
        JsValue::from_f64(f64::from(cache_slot)),
        JsValue::from_f64(0.0),
        JsValue::from_f64(0.0),
        JsValue::from_f64(0.0),
        JsValue::from_f64(0.0),
        flat_points(points).into(),
    ]);
    if let Err(err) = cb.apply(&JsValue::NULL, &args) {
        warn!(?err, "surface_cache callback threw (restore)");
    }
}

/// The session watermark tile, for the GPU path to overdraw on every frame.
pub(crate) fn watermark(cb: &js_sys::Function, wm: &Watermark) {
    let args = js_sys::Array::from_iter([
        js_sys::Uint8Array::from(wm.rgba.as_slice()).into(),
        JsValue::from_f64(f64::from(wm.width)),
        JsValue::from_f64(f64::from(wm.height)),
        JsValue::from_f64(f64::from(wm.cell_w)),
        JsValue::from_f64(f64::from(wm.cell_h)),
        JsValue::from_f64(f64::from(wm.off_x)),
        JsValue::from_f64(f64::from(wm.off_y)),
        JsValue::from_f64(f64::from(wm.opacity)),
    ]);
    if let Err(err) = cb.apply(&JsValue::NULL, &args) {
        warn!(?err, "AVC watermark callback threw");
    }
}

/// A compressed AVC main sub-stream for the browser's WebCodecs decoder. `regions` is a flat
/// `[x, y, w, h, ...]` so only the valid sub-rects are drawn (never the green YUV padding).
pub(crate) fn avc_frame(cb: &js_sys::Function, frame: &AvcFrameEvent) {
    let data = js_sys::Uint8Array::from(frame.main_stream.as_slice());
    let mut flat: Vec<u32> = Vec::with_capacity(frame.regions.len() * 4);
    for (x, y, w, h) in &frame.regions {
        flat.extend_from_slice(&[*x, *y, *w, *h]);
    }
    let regions = js_sys::Uint32Array::from(flat.as_slice());
    let args = js_sys::Array::from_iter([
        JsValue::from_f64(f64::from(frame.surface_id)),
        JsValue::from_f64(f64::from(frame.frame_id)),
        JsValue::from_f64(f64::from(frame.origin_x)),
        JsValue::from_f64(f64::from(frame.origin_y)),
        regions.into(),
        data.into(),
    ]);
    if let Err(err) = cb.apply(&JsValue::NULL, &args) {
        warn!(?err, "AVC decode callback threw");
    }
}
