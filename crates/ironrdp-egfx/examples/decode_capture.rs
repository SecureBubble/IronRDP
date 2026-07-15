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
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, SolidFillPdu, SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface2Pdu,
};
use ironrdp_graphics::progressive::ProgressiveDecoder;

const TILE: u32 = 64;
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
            data: vec![0; (width.saturating_mul(height).saturating_mul(4)) as usize],
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
}

struct NativeHandler {
    surfaces: HashMap<u16, SurfaceBuf>,
    cache: HashMap<u16, SurfaceBuf>,
    mapped: HashMap<u16, (u32, u32)>,
    progressive: ProgressiveDecoder,
    outdir: String,
    frame_no: u32,
    stats: Stats,
}

impl NativeHandler {
    fn new(outdir: String) -> Self {
        Self {
            surfaces: HashMap::new(),
            cache: HashMap::new(),
            mapped: HashMap::new(),
            progressive: ProgressiveDecoder::new(),
            outdir,
            frame_no: 0,
            stats: Stats::default(),
        }
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
    let avg = if n > 0 { [sum[0] / n, sum[1] / n, sum[2] / n] } else { [0; 3] };
    ((mn, mx), avg)
}

impl GraphicsPipelineHandler for NativeHandler {
    fn on_reset_graphics(&mut self, width: u32, height: u32) {
        eprintln!("[reset_graphics] {width}x{height}");
    }
    fn on_surface_created(&mut self, surface: &Surface) {
        self.stats.surfaces_created += 1;
        eprintln!("[surface_created] id={} {}x{}", surface.id, surface.width, surface.height);
        self.surfaces
            .insert(surface.id, SurfaceBuf::new(u32::from(surface.width), u32::from(surface.height)));
    }
    fn on_surface_deleted(&mut self, surface_id: u16) {
        self.surfaces.remove(&surface_id);
        self.mapped.remove(&surface_id);
    }
    fn on_surface_mapped(&mut self, surface_id: u16, ox: u32, oy: u32) {
        eprintln!("[surface_mapped] id={surface_id} -> ({ox},{oy})");
        self.mapped.insert(surface_id, (ox, oy));
    }
    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        self.stats.bitmap_updates += 1;
        if update.data.is_empty() {
            return;
        }
        let x = u32::from(update.destination_rectangle.left);
        let y = u32::from(update.destination_rectangle.top);
        let (w, h) = (u32::from(update.width), u32::from(update.height));
        if let Some(s) = self.surfaces.get_mut(&update.surface_id) {
            s.blit(x, y, w, h, &update.data, w);
        }
    }
    fn on_wire_to_surface2(&mut self, pdu: &WireToSurface2Pdu) {
        self.stats.wire2 += 1;
        dump_progressive_structure(self.stats.wire2, pdu.codec_context_id, &pdu.bitmap_data);
        let Some((sw, sh)) = self.surfaces.get(&pdu.surface_id).map(|s| (s.width, s.height)) else {
            eprintln!("[wire2] surface {} unknown -- skip", pdu.surface_id);
            return;
        };
        let tiles = match self.progressive.decode_bitmap(
            pdu.codec_context_id,
            sw.min(u32::from(u16::MAX)) as u16,
            sh.min(u32::from(u16::MAX)) as u16,
            &pdu.bitmap_data,
        ) {
            Ok(t) => t,
            Err(e) => {
                self.stats.wire2_failed += 1;
                eprintln!("[wire2] PROGRESSIVE DECODE FAILED surface={} : {e}", pdu.surface_id);
                return;
            }
        };
        self.stats.tiles += tiles.len() as u64;
        // colour sanity on the first decoded tile of this frame
        if let Some(t0) = tiles.first() {
            let ((mn, mx), avg) = colour_stats(&t0.pixels);
            eprintln!(
                "[wire2] surface={} ctx={} tiles={} tile0 RGB min={:?} max={:?} avg={:?} data_len={}",
                pdu.surface_id,
                pdu.codec_context_id,
                tiles.len(),
                mn,
                mx,
                avg,
                pdu.bitmap_data.len()
            );
        }
        if let Some(surface) = self.surfaces.get_mut(&pdu.surface_id) {
            for tile in &tiles {
                let tx = u32::from(tile.x_idx) * TILE;
                let ty = u32::from(tile.y_idx) * TILE;
                surface.blit(tx, ty, TILE, TILE, &tile.pixels, TILE);
            }
        }
    }
    fn on_solid_fill(&mut self, pdu: &SolidFillPdu) {
        self.stats.solid_fills += 1;
        let c = &pdu.fill_pixel;
        let rgba = [c.r, c.g, c.b, 0xff];
        if let Some(s) = self.surfaces.get_mut(&pdu.surface_id) {
            for rect in &pdu.rectangles {
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
    }
    fn on_surface_to_surface(&mut self, pdu: &SurfaceToSurfacePdu) {
        let r = &pdu.source_rectangle;
        let (sx, sy) = (u32::from(r.left), u32::from(r.top));
        let w = u32::from(r.right.saturating_sub(r.left));
        let h = u32::from(r.bottom.saturating_sub(r.top));
        let Some(block) = self.surfaces.get(&pdu.source_surface_id).map(|s| s.extract(sx, sy, w, h)) else {
            return;
        };
        if let Some(dst) = self.surfaces.get_mut(&pdu.destination_surface_id) {
            for p in &pdu.destination_points {
                dst.blit(u32::from(p.x), u32::from(p.y), w, h, &block, w);
            }
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
        if let Some(dst) = self.surfaces.get_mut(&pdu.surface_id) {
            for p in &pdu.destination_points {
                dst.blit(u32::from(p.x), u32::from(p.y), w, h, &block, w);
            }
        }
    }
    fn on_frame_complete(&mut self, frame_id: u32) {
        self.stats.frames += 1;
        self.frame_no += 1;
        // Dump every mapped surface to a BMP so the frame can be eyeballed.
        for (sid, &(_ox, _oy)) in &self.mapped {
            if let Some(s) = self.surfaces.get(sid) {
                let path = format!("{}/frame_{:04}_surface_{}.bmp", self.outdir, self.frame_no, sid);
                write_bmp(&path, s.width, s.height, &s.data);
                let ((mn, mx), avg) = colour_stats(&s.data);
                eprintln!(
                    "[frame_complete] id={frame_id} -> {path} ({}x{}) RGB min={mn:?} max={mx:?} avg={avg:?}",
                    s.width, s.height
                );
            }
        }
    }
}

/// Parse the RFX Progressive stream and print its block/region/tile structure,
/// so we can see WHY a frame fails (empty quant table, missing CONTEXT, etc.).
fn dump_progressive_structure(n: u32, ctx_id: u32, bitmap_data: &[u8]) {
    use ironrdp_pdu::codecs::rfx::progressive::{decode_progressive_stream, ProgressiveBlock, ProgressiveTile};
    let blocks = match decode_progressive_stream(bitmap_data) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("  <wire2 #{n} ctx={ctx_id}> STREAM PARSE FAILED: {e}  (len={})", bitmap_data.len());
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

    // Split length-prefixed segments.
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

    let mut client = GraphicsPipelineClient::new(Box::new(NativeHandler::new(outdir.clone())), None);

    for (i, seg) in segments.iter().enumerate() {
        match client.process(GFX_CHANNEL_ID, seg) {
            Ok(_resp) => {}
            Err(e) => eprintln!("segment {i} (len {}) process error: {e}", seg.len()),
        }
    }

    eprintln!("done. BMPs written to {outdir}/");
}
