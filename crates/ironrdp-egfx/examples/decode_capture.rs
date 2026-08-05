//! Offline eGFX decode harness.
//!
//! Replays the exact graphics-channel (Microsoft::Windows::RDS::Graphics) bytes
//! captured from a live session through the REAL ironrdp decode path
//! (zgfx -> egfx PDU decode -> RFX Progressive) and dumps each completed frame
//! to a BMP, plus per-frame colour statistics. No proxy, no VM, no browser --
//! deterministic and reproducible from a capture.
//!
//! Input file format (`egfx_stream.bin`): repeated `[u32 LE length][segment bytes]`,
//! where each segment is one reassembled drdynvc dynamic-channel message exactly
//! as it would be handed to `GraphicsPipelineClient::process()`.
//!
//! Run:  cargo run -p ironrdp-egfx --example decode_capture -- egfx_stream.bin ./egfx_out

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;

use ironrdp_dvc::DvcProcessor;
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineClient, GraphicsPipelineHandler, Surface};
use ironrdp_egfx::pdu::{CacheToSurfacePdu, SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface2Pdu};

const GFX_CHANNEL_ID: u32 = 7;

struct SurfaceBuf {
    width: u32,
    height: u32,
    data: Vec<u8>, // RGBA8888, tightly packed
}

impl SurfaceBuf {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            // White init (FreeRDP gdi_CreateSurface memset 0xFF).
            data: vec![0xFF; (width.saturating_mul(height).saturating_mul(4)) as usize],
        }
    }
    fn blit(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8], src_stride_px: u32) {
        let (sw, sh) = (self.width, self.height);
        for row in 0..h {
            let dy = y + row;
            if dy >= sh {
                break;
            }
            if x >= sw {
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

/// Write a tightly-packed RGBA buffer to a 32bpp top-down BMP (no deps).
fn write_bmp(path: &str, w: u32, h: u32, rgba: &[u8]) {
    let row = (w * 4) as usize;
    let img = row * h as usize;
    let total = 54 + img;
    let mut b = Vec::with_capacity(total);
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&(total as u32).to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
    b.extend_from_slice(&40u32.to_le_bytes()); // info header size
    b.extend_from_slice(&(w as i32).to_le_bytes());
    b.extend_from_slice(&(-(h as i32)).to_le_bytes()); // negative => top-down
    b.extend_from_slice(&1u16.to_le_bytes()); // planes
    b.extend_from_slice(&32u16.to_le_bytes()); // bpp
    b.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    b.extend_from_slice(&(img as u32).to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes());
    b.extend_from_slice(&0i32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    for px in rgba.chunks_exact(4) {
        b.extend_from_slice(&[px[2], px[1], px[0], 0xFF]); // RGBA -> BGRA
    }
    if let Err(e) = fs::File::create(path).and_then(|mut f| f.write_all(&b)) {
        eprintln!("  ! failed to write {path}: {e}");
    }
}

#[derive(Default)]
struct Stats {
    segments: u32,
    surfaces_created: u32,
    wire2: u32,
    wire2_failed: u32,
    tiles: u64,
    frames: u32,
    bitmap_updates: u32,
    solid_fills: u32,
    s2s: u32,
    s2c: u32,
    c2s: u32,
    c2s_miss: u32,
    wire2_per_surface: std::collections::BTreeMap<u16, u32>,
}

/// Accumulated dirty rect in surface-local coords (exclusive max) — mirrors the
/// browser handler so the harness composites exactly like the live client.
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

struct NativeHandler {
    surfaces: HashMap<u16, SurfaceBuf>,
    cache: HashMap<u16, SurfaceBuf>,
    mapped: HashMap<u16, (u32, u32)>,
    dirty: HashMap<u16, Dirty>,
    /// slot -> (s2c order#, source surface, x, y, w, h) — provenance of each cache entry.
    cache_src: HashMap<u16, (u32, u16, u16, u16, u16, u16)>,
    outdir: String,
    frame_no: u32,
    stats: Stats,
    /// Persistent output canvas (like the browser's <canvas>): accumulates the
    /// dirty region of every mapped surface at frame-complete.
    canvas: Vec<u8>,
    canvas_w: u32,
    canvas_h: u32,
    op_seq: u32,
}

impl NativeHandler {
    fn new(outdir: String) -> Self {
        Self {
            surfaces: HashMap::new(),
            cache: HashMap::new(),
            mapped: HashMap::new(),
            dirty: HashMap::new(),
            cache_src: HashMap::new(),
            outdir,
            frame_no: 0,
            stats: Stats::default(),
            canvas: Vec::new(),
            canvas_w: 0,
            canvas_h: 0,
            op_seq: 0,
        }
    }

    fn mark_dirty(&mut self, surface_id: u16, x: u32, y: u32, w: u32, h: u32) {
        let d = Dirty::union(self.dirty.get(&surface_id).copied(), x, y, w, h);
        self.dirty.insert(surface_id, d);
    }
}

/// Min/max per channel over an RGBA buffer -> quick "are the colours sane?" check.
fn colour_stats(rgba: &[u8]) -> (([u8; 3], [u8; 3]), [u64; 3]) {
    let mut mn = [255u8; 3];
    let mut mx = [0u8; 3];
    let mut sum = [0u64; 3];
    let mut n = 0u64;
    for px in rgba.chunks_exact(4) {
        for c in 0..3 {
            mn[c] = mn[c].min(px[c]);
            mx[c] = mx[c].max(px[c]);
            sum[c] += u64::from(px[c]);
        }
        n += 1;
    }
    let avg = if n > 0 {
        [sum[0] / n, sum[1] / n, sum[2] / n]
    } else {
        [0; 3]
    };
    ((mn, mx), avg)
}

impl Drop for NativeHandler {
    fn drop(&mut self) {
        // DIAG: dump each raw decoded surface (bypasses the canvas compositor so we
        // see exactly what ClearCodec/Progressive produced, independent of map/dirty).
        for (id, surf) in &self.surfaces {
            let path = format!("{}/surface_{}.bmp", self.outdir, id);
            write_bmp(&path, surf.width, surf.height, &surf.data);
            // PPM (P6 RGB) for exact diffing against FreeRDP goldens. Surfaces are RGBA.
            let ppm_path = format!("{}/surface_{}.ppm", self.outdir, id);
            let mut ppm = format!("P6\n{} {}\n255\n", surf.width, surf.height).into_bytes();
            for px in surf.data.chunks_exact(4) {
                ppm.push(px[0]);
                ppm.push(px[1]);
                ppm.push(px[2]);
            }
            let _ = fs::write(&ppm_path, &ppm);
            eprintln!("[surface dump] id={id} {}x{} -> {path} + .ppm", surf.width, surf.height);
        }
        let s = &self.stats;
        eprintln!(
            "\n=== STATS === surfaces_created={} wire2={} (per-surface {:?}) tiles={} frames={} \
             solid_fills={} surface_to_surface={} surface_to_cache={} cache_to_surface={} (miss={}) bitmap_updates={}",
            s.surfaces_created,
            s.wire2,
            s.wire2_per_surface,
            s.tiles,
            s.frames,
            s.solid_fills,
            s.s2s,
            s.s2c,
            s.c2s,
            s.c2s_miss,
            s.bitmap_updates
        );
    }
}

impl GraphicsPipelineHandler for NativeHandler {
    fn on_capabilities_confirmed(&mut self, caps: &ironrdp_egfx::pdu::CapabilitySet) {
        eprintln!("[CAPS CONFIRMED] {caps:?}");
    }
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        eprintln!("[reset_graphics] {width}x{height}");
        // DIAG MODE: KEEP surface content across resets (do not blank to white) so a
        // full-session replay renders its final settled state instead of going blank.
        if std::env::var("KEEP_ON_RESET").is_err() {
            for s in self.surfaces.values_mut() {
                s.data.iter_mut().for_each(|b| *b = 0xFF);
            }
        }
        self.dirty.clear();
        // The RFX Progressive codec is now reset in the client core's ResetGraphics handling.
        if width > self.canvas_w || height > self.canvas_h {
            self.canvas_w = self.canvas_w.max(width);
            self.canvas_h = self.canvas_h.max(height);
            self.canvas = vec![0; (self.canvas_w * self.canvas_h * 4) as usize];
        }
    }
    fn on_surface_created(&mut self, surface: &Surface) {
        self.stats.surfaces_created += 1;
        eprintln!(
            "[surface_created] id={} {}x{}",
            surface.id, surface.width, surface.height
        );
        self.surfaces.insert(
            surface.id,
            SurfaceBuf::new(u32::from(surface.width), u32::from(surface.height)),
        );
    }
    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
        self.mapped.remove(&surface_id);
    }
    fn on_map_surface_to_scaled_output(&mut self, pdu: &ironrdp_egfx::pdu::MapSurfaceToScaledOutputPdu) {
        eprintln!("[MAP-SCALED-OUTPUT] {pdu:?}");
        self.on_surface_mapped(pdu.surface_id, pdu.output_origin_x, pdu.output_origin_y);
    }
    fn on_map_surface_to_scaled_window(&mut self, pdu: &ironrdp_egfx::pdu::MapSurfaceToScaledWindowPdu) {
        eprintln!("[MAP-SCALED-WINDOW] {pdu:?}");
    }
    fn on_unhandled_pdu(&mut self, pdu: &ironrdp_egfx::pdu::GfxPdu) {
        eprintln!("[UNHANDLED-PDU] {:?}", core::mem::discriminant(pdu));
    }
    fn on_surface_mapped(&mut self, surface_id: u16, ox: u32, oy: u32) {
        eprintln!("[surface_mapped] id={surface_id} -> ({ox},{oy})");
        self.mapped.insert(surface_id, (ox, oy));
        // Flush the whole surface once on map. With WHITE surface init this
        // delivers the white background the server never explicitly paints (it
        // relies on the surface being white per FreeRDP gdi_CreateSurface) to the
        // output canvas — instead of leaving it as the canvas's black. With white
        // init this cannot cause the old black flood.
        if let Some(s) = self.surfaces.get(&surface_id) {
            let (w, h) = (s.width, s.height);
            self.mark_dirty(surface_id, 0, 0, w, h);
        }
    }
    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        self.stats.bitmap_updates += 1;
        if update.data.is_empty() {
            return;
        }
        let x = u32::from(update.destination_rectangle.left);
        let y = u32::from(update.destination_rectangle.top);
        let (w, h) = (u32::from(update.width), u32::from(update.height));
        // DIAG: flag green-dominant tiles (suspected NSCodec mis-decode).
        {
            let ((_mn, _mx), avg) = colour_stats(&update.data);
            if avg[1] > avg[0] + 50 && avg[1] > avg[2] + 50 && avg[1] > 100 {
                eprintln!(
                    "[GREEN bmp#{}] codec={:?} rect=({x},{y} {w}x{h}) avg={:?}",
                    self.stats.bitmap_updates, update.codec_id, avg
                );
            }
            // Yellow: high R + high G, low B.
            if avg[0] > 150 && avg[1] > 150 && avg[2] + 60 < avg[0] {
                eprintln!(
                    "[YELLOW bmp#{}] codec={:?} rect=({x},{y} {w}x{h}) avg={:?}",
                    self.stats.bitmap_updates, update.codec_id, avg
                );
            }
        }
        // DIAG: first N bitmap updates — codec + avg colour.
        if self.stats.bitmap_updates <= 20 {
            let ((mn, mx), avg) = colour_stats(&update.data);
            eprintln!(
                "[bmp#{}] codec={:?} rect=({x},{y} {w}x{h}) avg={:?} min={:?} max={:?}",
                self.stats.bitmap_updates, update.codec_id, avg, mn, mx
            );
        }
        // DIAG: trace every decoded update covering the gray tile (1216,128).
        if x <= 1216 && 1216 < x + w && y <= 128 && 128 < y + h {
            let ((_mn, _mx), avg) = colour_stats(&update.data);
            eprintln!(
                "[UPD@(1216,128)] frame={} codec={:?} rect=({x},{y} {w}x{h}) avg={:?}",
                self.frame_no, update.codec_id, avg
            );
        }
        // DIAG: trace every update to one representative toolbar tile over time.
        if x == 960 && y == 384 {
            let ((_mn, _mx), avg) = colour_stats(&update.data);
            let px: Vec<_> = update.data.chunks(4).take(4).map(|p| (p[0], p[1], p[2], p[3])).collect();
            eprintln!(
                "[tile 960,384] frame={} codec={:?} {}x{} len={} avg={:?} px0..3={:?}",
                self.frame_no, update.codec_id, w, h, update.data.len(), avg, px
            );
        }
        if let Some(s) = self.surfaces.get_mut(&update.surface_id) {
            s.blit(x, y, w, h, &update.data, w);
        }
        self.mark_dirty(update.surface_id, x, y, w, h);
    }
    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        // RFX Progressive now decodes in the client core (GraphicsPipelineClient::
        // handle_wire_to_surface2), which emits each decoded tile through on_bitmap_updated
        // (compositing happens there, exactly like the production web compositor). Here we
        // only record that the WireToSurface2 arrived. A decode failure surfaces as a terminal
        // error from client.process() in main, not here.
        self.stats.wire2 += 1;
        *self.stats.wire2_per_surface.entry(pdu.surface_id).or_default() += 1;
        if !self.surfaces.contains_key(&pdu.surface_id) {
            eprintln!("[wire2] surface {} unknown", pdu.surface_id);
        }
        // DIAG: dump tile-type structure + raw block-type words for every wire2.
        {
            let head: Vec<String> = pdu
                .bitmap_data
                .chunks(2)
                .take(8)
                .map(|c| format!("{:04x}", u16::from_le_bytes([c[0], *c.get(1).unwrap_or(&0)])))
                .collect();
            eprint!("[wire2 frame={} ctx={} len={} head={:?}] ", self.frame_no, pdu.codec_context_id, pdu.bitmap_data.len(), head);
            dump_progressive_structure(self.stats.wire2, pdu.codec_context_id, &pdu.bitmap_data);
        }
    }
    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        self.stats.solid_fills += 1;
        self.op_seq += 1;
        let c = &pdu.fill_pixel;
        if c.r < 20 && c.g < 20 && c.b < 20 {
            if let Some(r) = pdu.rectangles.first() {
                eprintln!(
                    "[op {}] SolidFill BLACK ({},{},{}) at ({},{})",
                    self.op_seq, c.r, c.g, c.b, r.left, r.top
                );
            }
        }
        let c = &pdu.fill_pixel;
        let rgba = [c.r, c.g, c.b, 0xff];
        {
            let sample: Vec<_> = pdu
                .rectangles
                .iter()
                .take(3)
                .map(|r| {
                    (
                        r.left,
                        r.top,
                        r.right.saturating_sub(r.left),
                        r.bottom.saturating_sub(r.top),
                    )
                })
                .collect();
            eprintln!(
                "[solid_fill #{}] surface={} rgb=({},{},{}) rects={} sample={:?}",
                self.stats.solid_fills,
                pdu.surface_id,
                c.r,
                c.g,
                c.b,
                pdu.rectangles.len(),
                sample
            );
        }
        let rects: Vec<_> = pdu.rectangles.clone();
        if let Some(s) = self.surfaces.get_mut(&pdu.surface_id) {
            for rect in &rects {
                let x = u32::from(rect.left);
                let y = u32::from(rect.top);
                let w = u32::from(rect.right.saturating_sub(rect.left));
                let h = u32::from(rect.bottom.saturating_sub(rect.top));
                let (sw, sh) = (s.width, s.height);
                for row in 0..h {
                    let dy = y + row;
                    if dy >= sh {
                        break;
                    }
                    let cw = w.min(sw.saturating_sub(x));
                    for col in 0..cw {
                        let off = (((dy * sw) + x + col) * 4) as usize;
                        if off + 4 <= s.data.len() {
                            s.data[off..off + 4].copy_from_slice(&rgba);
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
        self.stats.s2s += 1;
        let r = &pdu.source_rectangle;
        let (sx, sy) = (u32::from(r.left), u32::from(r.top));
        let w = u32::from(r.right.saturating_sub(r.left));
        let h = u32::from(r.bottom.saturating_sub(r.top));
        eprintln!(
            "[surface_to_surface] src={} -> dst={} rect=({sx},{sy} {w}x{h}) pts={}",
            pdu.source_surface_id,
            pdu.destination_surface_id,
            pdu.destination_points.len()
        );
        let Some(block) = self
            .surfaces
            .get(&pdu.source_surface_id)
            .map(|s| s.extract(sx, sy, w, h))
        else {
            return;
        };
        let pts: Vec<_> = pdu.destination_points.clone();
        if let Some(dst) = self.surfaces.get_mut(&pdu.destination_surface_id) {
            for p in &pts {
                dst.blit(u32::from(p.x), u32::from(p.y), w, h, &block, w);
            }
        }
        for p in &pts {
            self.mark_dirty(pdu.destination_surface_id, u32::from(p.x), u32::from(p.y), w, h);
        }
    }
    fn on_surface_to_cache(&mut self, pdu: &SurfaceToCachePdu) {
        self.stats.s2c += 1;
        self.op_seq += 1;
        if pdu.cache_slot == 2 {
            let r = &pdu.source_rectangle;
            eprintln!("[op {}] SurfaceToCache slot=2 from ({},{})", self.op_seq, r.left, r.top);
        }
        if self.stats.s2c <= 16 {
            let r = &pdu.source_rectangle;
            eprintln!(
                "[s2c #{}] slot={} key={:#x} surface={} src=({},{} {}x{})",
                self.stats.s2c,
                pdu.cache_slot,
                pdu.cache_key,
                pdu.surface_id,
                r.left,
                r.top,
                r.right.saturating_sub(r.left),
                r.bottom.saturating_sub(r.top)
            );
        }
        let r = &pdu.source_rectangle;
        let (sx, sy) = (u32::from(r.left), u32::from(r.top));
        let w = u32::from(r.right.saturating_sub(r.left));
        let h = u32::from(r.bottom.saturating_sub(r.top));
        self.cache_src.insert(
            pdu.cache_slot,
            (self.stats.s2c, pdu.surface_id, r.left, r.top, w as u16, h as u16),
        );
        if let Some(src) = self.surfaces.get(&pdu.surface_id) {
            let mut buf = SurfaceBuf::new(w, h);
            buf.data = src.extract(sx, sy, w, h);
            self.cache.insert(pdu.cache_slot, buf);
        }
    }
    fn on_cache_to_surface(&mut self, pdu: &CacheToSurfacePdu) {
        self.stats.c2s += 1;
        let Some((w, h, block)) = self
            .cache
            .get(&pdu.cache_slot)
            .map(|c| (c.width, c.height, c.data.clone()))
        else {
            self.stats.c2s_miss += 1;
            if self.stats.c2s_miss <= 12 {
                eprintln!(
                    "[cache_to_surface MISS] slot={} surface={} pts={}",
                    pdu.cache_slot,
                    pdu.surface_id,
                    pdu.destination_points.len()
                );
            }
            return;
        };
        if self.stats.c2s <= 16 {
            let p0 = pdu.destination_points.first();
            eprintln!(
                "[c2s #{}] slot={} surface={} dims={}x{} pts={} first=({:?})",
                self.stats.c2s,
                pdu.cache_slot,
                pdu.surface_id,
                w,
                h,
                pdu.destination_points.len(),
                p0.map(|p| (p.x, p.y))
            );
        }
        // DIAG: trace the specific gray wallpaper tile at (1216,128).
        if std::env::var("GRAY_DEBUG").is_ok() {
            for p in &pdu.destination_points {
                if p.x == 1216 && p.y == 128 {
                    let ((_mn, _mx), avg) = colour_stats(&block);
                    eprintln!(
                        "[GRAY (1216,128)] frame={} slot={} avg={:?} src={:?}",
                        self.frame_no,
                        pdu.cache_slot,
                        avg,
                        self.cache_src.get(&pdu.cache_slot)
                    );
                }
            }
        }
        // Trace cache blits landing in the window content region; report the
        // cached tile's average colour + provenance to catch black-tile blits.
        for p in &pdu.destination_points {
            if (800..2000).contains(&p.x) && (300..950).contains(&p.y) {
                let (avg, _) = {
                    let ((_mn, _mx), a) = colour_stats(&block);
                    (a, ())
                };
                if avg[0] < 40 && avg[1] < 40 && avg[2] < 40 {
                    eprintln!(
                        "[frame {}] c2s->win BLACK slot={} dst=({},{}) cached_from={:?}",
                        self.frame_no,
                        pdu.cache_slot,
                        p.x,
                        p.y,
                        self.cache_src.get(&pdu.cache_slot)
                    );
                }
                break;
            }
        }
        let pts: Vec<_> = pdu.destination_points.clone();
        if std::env::var("NO_CACHE").is_err() {
            if let Some(dst) = self.surfaces.get_mut(&pdu.surface_id) {
                for p in &pts {
                    dst.blit(u32::from(p.x), u32::from(p.y), w, h, &block, w);
                }
            }
            for p in &pts {
                self.mark_dirty(pdu.surface_id, u32::from(p.x), u32::from(p.y), w, h);
            }
        }
    }
    fn on_frame_complete(&mut self, _frame_id: u32) {
        self.stats.frames += 1;
        self.frame_no += 1;
        // Composite the dirty region of every mapped surface onto the persistent
        // output canvas — exactly like WasmGraphicsHandler.on_frame_complete ->
        // RdpInputEvent::Graphics -> gui.draw in the live client.
        // Only flush the dirty region of MAPPED surfaces; keep unmapped surfaces'
        // dirty so it paints once they are mapped (FreeRDP's invalidRegion persists
        // until blitted). Never blit the whole surface on map.
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
            let data = surface.extract(x, y, w, h);
            // blit onto canvas at (ox+x, oy+y)
            let (cx, cy) = (ox + x, oy + y);
            for row in 0..h {
                let dy = cy + row;
                if dy >= self.canvas_h || cx >= self.canvas_w {
                    continue;
                }
                let cw = w.min(self.canvas_w - cx);
                let src_off = ((row * w) * 4) as usize;
                let dst_off = ((dy * self.canvas_w + cx) * 4) as usize;
                let bytes = (cw * 4) as usize;
                if src_off + bytes <= data.len() && dst_off + bytes <= self.canvas.len() {
                    self.canvas[dst_off..dst_off + bytes].copy_from_slice(&data[src_off..src_off + bytes]);
                }
            }
        }
        // Overwrite a single canvas BMP each frame (final state = last write);
        // snapshot every 40 frames for a timeline.
        if self.canvas_w > 0 {
            let latest = format!("{}/canvas_latest.bmp", self.outdir);
            write_bmp(&latest, self.canvas_w, self.canvas_h, &self.canvas);
            if self.frame_no % 40 == 0 {
                let snap = format!("{}/canvas_{:04}.bmp", self.outdir, self.frame_no);
                write_bmp(&snap, self.canvas_w, self.canvas_h, &self.canvas);
            }
        }
        // Bisect support: dump surf0 (the SURFACE, matching FreeRDP per-frame goldens)
        // after each EndFrame. Golden files are 0-indexed frameNNNN; our frame_no is
        // 1-indexed here, so idx = frame_no - 1.
        if std::env::var("DUMP_FRAMES").is_ok() {
            if let Some(surf) = self.surfaces.get(&0) {
                let idx = self.frame_no.saturating_sub(1);
                let path = format!("{}/surf-{:04}.ppm", self.outdir, idx);
                let mut ppm = format!("P6\n{} {}\n255\n", surf.width, surf.height).into_bytes();
                for px in surf.data.chunks_exact(4) {
                    ppm.push(px[0]);
                    ppm.push(px[1]);
                    ppm.push(px[2]);
                }
                let _ = fs::write(&path, &ppm);
            }
        }
    }
}

/// Parse the RFX Progressive stream and print its block/region/tile structure,
/// so we can see WHY a frame fails (empty quant table, missing CONTEXT, etc.).
fn dump_progressive_structure(n: u32, ctx_id: u32, bitmap_data: &[u8]) {
    use ironrdp_pdu::codecs::rfx::progressive::{ProgressiveBlock, ProgressiveTile, decode_progressive_stream};
    let blocks = match decode_progressive_stream(bitmap_data) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "  <wire2 #{n} ctx={ctx_id}> STREAM PARSE FAILED: {e}  (len={})",
                bitmap_data.len()
            );
            return;
        }
    };
    let mut kinds = Vec::new();
    for b in &blocks {
        match b {
            ProgressiveBlock::Sync(_) => kinds.push("Sync".to_owned()),
            ProgressiveBlock::FrameBegin(_) => kinds.push("FrameBegin".to_owned()),
            ProgressiveBlock::FrameEnd(_) => kinds.push("FrameEnd".to_owned()),
            ProgressiveBlock::Context(_) => kinds.push("Context".to_owned()),
            ProgressiveBlock::Region(r) => {
                let mut simple = 0;
                let mut first = 0;
                let mut upgrade = 0;
                let mut qualities = std::collections::BTreeSet::new();
                for t in &r.tiles {
                    match t {
                        ProgressiveTile::Simple(_) => simple += 1,
                        ProgressiveTile::First(s) => {
                            first += 1;
                            qualities.insert(s.quality);
                        }
                        ProgressiveTile::Upgrade(s) => {
                            upgrade += 1;
                            qualities.insert(s.quality);
                        }
                    }
                }
                kinds.push(format!(
                    "Region{{quant={} progQuant={} tiles={}(simple={simple} first={first} upgrade={upgrade}) qualities={:?}}}",
                    r.quant_vals.len(),
                    r.quant_prog_vals.len(),
                    r.tiles.len(),
                    qualities
                ));
            }
        }
    }
    eprintln!("  <wire2 #{n} ctx={ctx_id}> blocks: [{}]", kinds.join(", "));
}

fn main() {
    let mut args = std::env::args().skip(1);
    let input = args.next().unwrap_or_else(|| "egfx_stream.bin".to_owned());
    let outdir = args.next().unwrap_or_else(|| "egfx_out".to_owned());
    let _ = fs::create_dir_all(&outdir);

    let bytes = match fs::read(&input) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {input}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("read {} bytes from {input}; output -> {outdir}/", bytes.len());

    let mut client = GraphicsPipelineClient::new(Box::new(NativeHandler::new(outdir.clone())), None);

    if input.ends_with(".egfxpdu") {
        // Flat re-export: concatenated [cmdId(2 LE)][len(4 LE)][body(len)]. Reconstruct the
        // RDPGFX PDU framing [cmdId][flags=0][pduLength=8+len][body] and feed uncompressed.
        let mut flat = Vec::new();
        let mut off = 0usize;
        let mut n = 0u32;
        while off + 6 <= bytes.len() {
            let cmd = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
            let len = u32::from_le_bytes([bytes[off + 2], bytes[off + 3], bytes[off + 4], bytes[off + 5]]) as usize;
            off += 6;
            if off + len > bytes.len() {
                eprintln!("flat: truncated PDU #{n} cmd=0x{cmd:04x} want {len}");
                break;
            }
            flat.extend_from_slice(&cmd.to_le_bytes());
            flat.extend_from_slice(&0u16.to_le_bytes()); // flags
            flat.extend_from_slice(&(8u32 + len as u32).to_le_bytes()); // pduLength incl. header
            flat.extend_from_slice(&bytes[off..off + len]);
            off += len;
            n += 1;
        }
        eprintln!("flat: {n} PDUs, {} reconstructed bytes", flat.len());
        if let Err(e) = client.process_pdu_bytes_for_test(&flat) {
            eprintln!("flat process error: {e}");
        }
    } else {
        // Split length-prefixed zgfx segments.
        let mut segments: Vec<&[u8]> = Vec::new();
        let mut off = 0usize;
        while off + 4 <= bytes.len() {
            let len = u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]) as usize;
            off += 4;
            if off + len > bytes.len() {
                eprintln!("truncated segment at offset {off} (want {len})");
                break;
            }
            segments.push(&bytes[off..off + len]);
            off += len;
        }
        eprintln!("{} segments", segments.len());
        for (i, seg) in segments.iter().enumerate() {
            match client.process(GFX_CHANNEL_ID, seg) {
                Ok(_resp) => {}
                Err(e) => eprintln!("segment {i} (len {}) process error: {e}", seg.len()),
            }
        }
    }

    drop(client); // triggers surface dump (BMP + PPM)
    eprintln!("done. output written to {outdir}/");
}
