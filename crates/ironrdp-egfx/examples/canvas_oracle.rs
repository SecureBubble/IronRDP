//! canvas_oracle — full-desktop eGFX renderer that models BOTH the surface buffer
//! and the visible canvas (with the real dirty-region → canvas flush), so we can
//! diff each against a FreeRDP golden and localize any dropped flush.
//!
//! It composites every op the way the shipped `WasmGraphicsHandler` does:
//!   WireToSurface1 (ClearCodec / Uncompressed), WireToSurface2 (progressive),
//!   SolidFill, SurfaceToSurface, SurfaceToCache, CacheToSurface, Map, EndFrame.
//! Each surface-modifying op marks a dirty bbox; EndFrame flushes each mapped
//! surface's dirty bbox to the canvas — mirroring `on_frame_complete`.
//!
//! Usage: canvas_oracle <egfx-back.bin> <golden.ppm> [surfaceId=0]
//!   writes oracle-surface.ppm/.bmp and oracle-canvas.ppm/.bmp, then prints:
//!   - surface-vs-golden tile diff (tests decode/compositing)
//!   - canvas-vs-golden  tile diff (tests the flush)
//!   - canvas-vs-surface drop localization (surface painted but canvas didn't get it)

use ironrdp_core::decode as pdu_decode;
use ironrdp_egfx::pdu::{
    CacheToSurfacePdu, Codec1Type, CreateSurfacePdu, MapSurfaceToOutputPdu, MapSurfaceToScaledOutputPdu, SolidFillPdu,
    SurfaceToCachePdu, SurfaceToSurfacePdu, WireToSurface1Pdu,
};
use ironrdp_graphics::clearcodec::ClearCodecDecoder;
use ironrdp_graphics::progressive::ProgressiveDecoder;
use std::collections::HashMap;
use std::io::Write as _;

fn rd16(p: &[u8]) -> u16 {
    u16::from(p[0]) | (u16::from(p[1]) << 8)
}
fn rd32(p: &[u8]) -> u32 {
    u32::from(p[0]) | (u32::from(p[1]) << 8) | (u32::from(p[2]) << 16) | (u32::from(p[3]) << 24)
}

struct Surf {
    w: usize,
    h: usize,
    rgba: Vec<u8>,
}
impl Surf {
    fn new(w: usize, h: usize) -> Self {
        // Match SurfaceBuf::new: opaque white init.
        Surf { w, h, rgba: vec![0xFF; w * h * 4] }
    }
    // Mirror SurfaceBuf::blit.
    fn blit(&mut self, x: usize, y: usize, w: usize, h: usize, src: &[u8], src_stride_px: usize) {
        for row in 0..h {
            let dy = y + row;
            if dy >= self.h || x >= self.w {
                if dy >= self.h {
                    break;
                }
                continue;
            }
            let copy_w = w.min(self.w - x);
            let src_off = row * src_stride_px * 4;
            let dst_off = (dy * self.w + x) * 4;
            let bytes = copy_w * 4;
            if src_off + bytes > src.len() || dst_off + bytes > self.rgba.len() {
                continue;
            }
            self.rgba[dst_off..dst_off + bytes].copy_from_slice(&src[src_off..src_off + bytes]);
        }
    }
    // Mirror SurfaceBuf::extract.
    fn extract(&self, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
        let mut out = vec![0u8; w * h * 4];
        for row in 0..h {
            let sy = y + row;
            if sy >= self.h || x >= self.w {
                continue;
            }
            let copy_w = w.min(self.w - x);
            let src_off = (sy * self.w + x) * 4;
            let dst_off = row * w * 4;
            let bytes = copy_w * 4;
            if src_off + bytes > self.rgba.len() || dst_off + bytes > out.len() {
                continue;
            }
            out[dst_off..dst_off + bytes].copy_from_slice(&self.rgba[src_off..src_off + bytes]);
        }
        out
    }
}

#[derive(Clone, Copy)]
struct Dirty {
    min_x: u32,
    min_y: u32,
    max_x: u32,
    max_y: u32,
}

fn mark_dirty(dirty: &mut HashMap<u16, Dirty>, sid: u16, x: u32, y: u32, w: u32, h: u32) {
    let (nx0, ny0, nx1, ny1) = (x, y, x + w, y + h);
    let d = match dirty.get(&sid).copied() {
        Some(d) => Dirty {
            min_x: d.min_x.min(nx0),
            min_y: d.min_y.min(ny0),
            max_x: d.max_x.max(nx1),
            max_y: d.max_y.max(ny1),
        },
        None => Dirty { min_x: nx0, min_y: ny0, max_x: nx1, max_y: ny1 },
    };
    dirty.insert(sid, d);
}

const CMD_WS1: u16 = 0x0001;
const CMD_WS2: u16 = 0x0002;
const CMD_SOLIDFILL: u16 = 0x0004;
const CMD_S2S: u16 = 0x0005;
const CMD_S2C: u16 = 0x0006;
const CMD_C2S: u16 = 0x0007;
const CMD_CREATE: u16 = 0x0009;
const CMD_DELETE: u16 = 0x000a;
const CMD_ENDFRAME: u16 = 0x000c;
const CMD_MAP: u16 = 0x000f;
const CMD_MAP_SCALED: u16 = 0x0017;
const CODEC_PROGRESSIVE: u16 = 0x0009;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <egfx-back.bin> <golden.ppm> [surfaceId]", args[0]);
        std::process::exit(2);
    }
    let buf = std::fs::read(&args[1]).expect("read bin");
    let want_sid: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    println!("loaded {} bytes", buf.len());

    let mut surfaces: HashMap<u16, Surf> = HashMap::new();
    let mut cache: HashMap<u16, (usize, usize, Vec<u8>)> = HashMap::new();
    let mut mapped: HashMap<u16, (u32, u32)> = HashMap::new();
    let mut dirty: HashMap<u16, Dirty> = HashMap::new();
    let mut prog = ProgressiveDecoder::new();
    let mut clear = ClearCodecDecoder::new();
    let mut canvas: Option<Surf> = None;

    let (mut n_end, mut n_flush, mut n_ws1_skip) = (0u32, 0u32, 0u32);
    let mut off = 0usize;
    while off + 8 <= buf.len() {
        let cmd = rd16(&buf[off..]);
        let plen = rd32(&buf[off + 4..]) as usize;
        if plen < 8 || off + plen > buf.len() {
            println!("stop: bad PDU off={off} cmd=0x{cmd:04x} plen={plen}");
            break;
        }
        let body = &buf[off + 8..off + plen];
        match cmd {
            CMD_CREATE => {
                if let Ok(p) = pdu_decode::<CreateSurfacePdu>(body) {
                    let (w, h) = (usize::from(p.width), usize::from(p.height));
                    surfaces.insert(p.surface_id, Surf::new(w, h));
                    if canvas.is_none() {
                        canvas = Some(Surf::new(w, h));
                    }
                    println!("CREATE id={} {w}x{h}", p.surface_id);
                }
            }
            CMD_DELETE => {
                if body.len() >= 2 {
                    let sid = rd16(body);
                    surfaces.remove(&sid);
                    mapped.remove(&sid);
                    dirty.remove(&sid);
                }
            }
            CMD_MAP => {
                if let Ok(p) = pdu_decode::<MapSurfaceToOutputPdu>(body) {
                    mapped.insert(p.surface_id, (p.output_origin_x, p.output_origin_y));
                }
            }
            CMD_MAP_SCALED => {
                if let Ok(p) = pdu_decode::<MapSurfaceToScaledOutputPdu>(body) {
                    mapped.insert(p.surface_id, (p.output_origin_x, p.output_origin_y));
                }
            }
            CMD_WS1 => {
                if let Ok(p) = pdu_decode::<WireToSurface1Pdu>(body) {
                    let r = &p.destination_rectangle;
                    let (x, y) = (usize::from(r.left), usize::from(r.top));
                    let w = usize::from(r.right.saturating_sub(r.left));
                    let h = usize::from(r.bottom.saturating_sub(r.top));
                    let rgba = match p.codec_id {
                        Codec1Type::ClearCodec => match clear.decode(&p.bitmap_data, w as u16, h as u16) {
                            Ok(bgra) => Some(bgra.chunks_exact(4).flat_map(|px| [px[2], px[1], px[0], 0xFF]).collect::<Vec<u8>>()),
                            Err(e) => {
                                eprintln!("  clearcodec err @({x},{y}) {w}x{h}: {e}");
                                None
                            }
                        },
                        Codec1Type::Uncompressed => Some(
                            p.bitmap_data.chunks_exact(4).flat_map(|px| [px[2], px[1], px[0], 0xFF]).collect::<Vec<u8>>(),
                        ),
                        _ => {
                            n_ws1_skip += 1;
                            None
                        }
                    };
                    if let (Some(data), Some(s)) = (rgba, surfaces.get_mut(&p.surface_id)) {
                        s.blit(x, y, w, h, &data, w);
                        mark_dirty(&mut dirty, p.surface_id, x as u32, y as u32, w as u32, h as u32);
                    }
                }
            }
            CMD_WS2 => {
                if body.len() >= 13 {
                    let sid = rd16(body);
                    let codec = rd16(&body[2..]);
                    let ctx = rd32(&body[4..]);
                    let mut blen = rd32(&body[9..]) as usize;
                    let data = &body[13..];
                    if blen > data.len() {
                        blen = data.len();
                    }
                    if codec == CODEC_PROGRESSIVE {
                        if let Some(s) = surfaces.get_mut(&sid) {
                            let (sw, sh) = (s.w as u16, s.h as u16);
                            if let Ok(tiles) = prog.decode_bitmap(sid, ctx, sw, sh, &data[..blen]) {
                                for t in tiles {
                                    let (x0, y0) = (usize::from(t.x_idx) * 64, usize::from(t.y_idx) * 64);
                                    let tw = 64.min(s.w.saturating_sub(x0));
                                    let th = 64.min(s.h.saturating_sub(y0));
                                    s.blit(x0, y0, tw, th, &t.pixels, 64);
                                    mark_dirty(&mut dirty, sid, x0 as u32, y0 as u32, tw as u32, th as u32);
                                }
                            }
                        }
                    }
                }
            }
            CMD_SOLIDFILL => {
                if let Ok(p) = pdu_decode::<SolidFillPdu>(body) {
                    let rgba = [p.fill_pixel.r, p.fill_pixel.g, p.fill_pixel.b, 0xFF];
                    if let Some(s) = surfaces.get_mut(&p.surface_id) {
                        for rect in &p.rectangles {
                            let (x, y) = (usize::from(rect.left), usize::from(rect.top));
                            let w = usize::from(rect.right.saturating_sub(rect.left));
                            let h = usize::from(rect.bottom.saturating_sub(rect.top));
                            let row: Vec<u8> = rgba.iter().copied().cycle().take(w * 4).collect();
                            for r in 0..h {
                                s.blit(x, y + r, w, 1, &row, w);
                            }
                            mark_dirty(&mut dirty, p.surface_id, x as u32, y as u32, w as u32, h as u32);
                        }
                    }
                }
            }
            CMD_S2S => {
                if let Ok(p) = pdu_decode::<SurfaceToSurfacePdu>(body) {
                    let r = &p.source_rectangle;
                    let (sx, sy) = (usize::from(r.left), usize::from(r.top));
                    let w = usize::from(r.right.saturating_sub(r.left));
                    let h = usize::from(r.bottom.saturating_sub(r.top));
                    let block = surfaces.get(&p.source_surface_id).map(|s| s.extract(sx, sy, w, h));
                    if let (Some(block), Some(dst)) = (block, surfaces.get_mut(&p.destination_surface_id)) {
                        for pt in &p.destination_points {
                            dst.blit(usize::from(pt.x), usize::from(pt.y), w, h, &block, w);
                            mark_dirty(&mut dirty, p.destination_surface_id, u32::from(pt.x), u32::from(pt.y), w as u32, h as u32);
                        }
                    }
                }
            }
            CMD_S2C => {
                if let Ok(p) = pdu_decode::<SurfaceToCachePdu>(body) {
                    let r = &p.source_rectangle;
                    let (sx, sy) = (usize::from(r.left), usize::from(r.top));
                    let w = usize::from(r.right.saturating_sub(r.left));
                    let h = usize::from(r.bottom.saturating_sub(r.top));
                    if let Some(s) = surfaces.get(&p.surface_id) {
                        cache.insert(p.cache_slot, (w, h, s.extract(sx, sy, w, h)));
                    }
                }
            }
            CMD_C2S => {
                if let Ok(p) = pdu_decode::<CacheToSurfacePdu>(body) {
                    if let Some((w, h, block)) = cache.get(&p.cache_slot).cloned() {
                        if let Some(dst) = surfaces.get_mut(&p.surface_id) {
                            for pt in &p.destination_points {
                                dst.blit(usize::from(pt.x), usize::from(pt.y), w, h, &block, w);
                                mark_dirty(&mut dirty, p.surface_id, u32::from(pt.x), u32::from(pt.y), w as u32, h as u32);
                            }
                        }
                    }
                }
            }
            CMD_ENDFRAME => {
                n_end += 1;
                // Mirror on_frame_complete: flush each MAPPED surface's dirty bbox to canvas.
                if let Some(cv) = canvas.as_mut() {
                    let ids: Vec<u16> = mapped.keys().copied().collect();
                    for sid in ids {
                        let Some(d) = dirty.remove(&sid) else { continue };
                        let (ox, oy) = mapped[&sid];
                        let Some(s) = surfaces.get(&sid) else { continue };
                        let x = d.min_x.min(s.w as u32);
                        let y = d.min_y.min(s.h as u32);
                        let w = d.max_x.min(s.w as u32).saturating_sub(x);
                        let h = d.max_y.min(s.h as u32).saturating_sub(y);
                        if w == 0 || h == 0 {
                            continue;
                        }
                        let region = s.extract(x as usize, y as usize, w as usize, h as usize);
                        cv.blit((ox + x) as usize, (oy + y) as usize, w as usize, h as usize, &region, w as usize);
                        n_flush += 1;
                    }
                }
            }
            _ => {}
        }
        off += plen;
    }

    println!("frames(EndFrame)={n_end} flushes={n_flush} ws1_skipped(non-clear/uncompressed)={n_ws1_skip}");

    let Some(surf) = surfaces.get(&want_sid) else {
        eprintln!("surface {want_sid} not found");
        return;
    };
    let canvas = canvas.expect("no canvas");
    write_ppm("oracle-surface.ppm", surf.w, surf.h, &surf.rgba);
    write_bmp("oracle-surface.bmp", surf.w, surf.h, &surf.rgba);
    write_ppm("oracle-canvas.ppm", canvas.w, canvas.h, &canvas.rgba);
    write_bmp("oracle-canvas.bmp", canvas.w, canvas.h, &canvas.rgba);
    println!("wrote oracle-surface.{{ppm,bmp}} + oracle-canvas.{{ppm,bmp}}");

    // Direct drop localization: where did the canvas NOT receive what the surface has?
    let mut drop_tiles = 0u32;
    let (tw, th) = (surf.w.div_ceil(64), surf.h.div_ceil(64));
    for tyi in 0..th {
        for txi in 0..tw {
            if tile_ne(&surf.rgba, &canvas.rgba, surf.w, surf.h, txi * 64, tyi * 64) {
                drop_tiles += 1;
            }
        }
    }
    println!("\n=== CANVAS vs SURFACE (flush drops) ===  differing 64px tiles: {drop_tiles} / {}", tw * th);

    let (gw, gh, g) = read_ppm(&args[2]);
    if gw != surf.w || gh != surf.h {
        eprintln!("golden size {gw}x{gh} != surface {}x{}", surf.w, surf.h);
        return;
    }

    // [DIAG] Per-pixel white-speckle count: pixels iron decoded near-white that the
    // golden did NOT — these are the scattered white specks the tile-mean diff hides.
    {
        let (mut light, mut dark, mut samples) = (0u64, 0u64, Vec::new());
        for i in 0..(surf.w * surf.h) {
            let (io, go) = (i * 4, i * 3);
            let iron_white = surf.rgba[io] >= 250 && surf.rgba[io + 1] >= 250 && surf.rgba[io + 2] >= 250;
            let gmax = g[go].max(g[go + 1]).max(g[go + 2]);
            if iron_white && gmax < 245 {
                light += 1;
                // Distinct dot: iron pure-white on a genuinely NON-light background.
                if gmax < 150 {
                    dark += 1;
                    if samples.len() < 15 {
                        let (x, y) = (i % surf.w, i / surf.w);
                        samples.push((x, y, g[go], g[go + 1], g[go + 2]));
                    }
                }
            }
        }
        println!("\n=== WHITE PIXELS iron>=250 that golden isn't ===");
        println!("  total (golden<245): {light}   |   DISTINCT DOTS (golden<150 dark bg): {dark}");
        for (x, y, r, gg, b) in &samples {
            println!("  dot ({x},{y}) golden=({r},{gg},{b})");
        }
    }
    println!("\n=== SURFACE vs GOLDEN (decode/compositing) ===");
    tile_diff(&surf.rgba, &g, surf.w, surf.h);
    println!("\n=== CANVAS vs GOLDEN (what is actually presented) ===");
    tile_diff(&canvas.rgba, &g, surf.w, surf.h);
}

fn tile_mean(rgba: &[u8], w: usize, h: usize, x0: usize, y0: usize) -> [f64; 3] {
    let (mut s, mut n) = ([0f64; 3], 0f64);
    for ty in 0..64 {
        let dy = y0 + ty;
        if dy >= h {
            break;
        }
        for tx in 0..64 {
            let dx = x0 + tx;
            if dx >= w {
                break;
            }
            let o = (dy * w + dx) * 4;
            s[0] += f64::from(rgba[o]);
            s[1] += f64::from(rgba[o + 1]);
            s[2] += f64::from(rgba[o + 2]);
            n += 1.0;
        }
    }
    if n == 0.0 {
        [0.0; 3]
    } else {
        [s[0] / n, s[1] / n, s[2] / n]
    }
}

// golden is RGB (3 bytes); ours is RGBA (4).
fn tile_mean_rgb(rgb: &[u8], w: usize, h: usize, x0: usize, y0: usize) -> [f64; 3] {
    let (mut s, mut n) = ([0f64; 3], 0f64);
    for ty in 0..64 {
        let dy = y0 + ty;
        if dy >= h {
            break;
        }
        for tx in 0..64 {
            let dx = x0 + tx;
            if dx >= w {
                break;
            }
            let o = (dy * w + dx) * 3;
            s[0] += f64::from(rgb[o]);
            s[1] += f64::from(rgb[o + 1]);
            s[2] += f64::from(rgb[o + 2]);
            n += 1.0;
        }
    }
    if n == 0.0 {
        [0.0; 3]
    } else {
        [s[0] / n, s[1] / n, s[2] / n]
    }
}

fn tile_ne(a: &[u8], b: &[u8], w: usize, h: usize, x0: usize, y0: usize) -> bool {
    for ty in 0..64 {
        let dy = y0 + ty;
        if dy >= h {
            break;
        }
        for tx in 0..64 {
            let dx = x0 + tx;
            if dx >= w {
                break;
            }
            let o = (dy * w + dx) * 4;
            if a[o] != b[o] || a[o + 1] != b[o + 1] || a[o + 2] != b[o + 2] {
                return true;
            }
        }
    }
    false
}

fn tile_diff(ours_rgba: &[u8], golden_rgb: &[u8], w: usize, h: usize) {
    let (tw, th) = (w.div_ceil(64), h.div_ceil(64));
    let mut worst: Vec<(f64, usize, usize, [f64; 3], [f64; 3])> = Vec::new();
    let (mut sum, mut n) = (0f64, 0f64);
    for tyi in 0..th {
        for txi in 0..tw {
            let (x0, y0) = (txi * 64, tyi * 64);
            let mi = tile_mean(ours_rgba, w, h, x0, y0);
            let go = tile_mean_rgb(golden_rgb, w, h, x0, y0);
            let d = (mi[0] - go[0]).abs() + (mi[1] - go[1]).abs() + (mi[2] - go[2]).abs();
            sum += d;
            n += 1.0;
            worst.push((d, x0, y0, mi, go));
        }
    }
    worst.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    let matching = worst.iter().filter(|t| t.0 <= 60.0).count();
    let lt5 = worst.iter().filter(|t| t.0 < 5.0).count();
    println!(
        "tiles={} mean|Δ|/tile={:.1}  matching(Δ<=60)={} (Δ<5: {})  outliers={}",
        n as u32,
        sum / n,
        matching,
        lt5,
        worst.len() - matching
    );
    for (d, x, y, mi, go) in worst.iter().take(12) {
        println!(
            "  ({:4},{:4}) ours({:3.0},{:3.0},{:3.0}) gold({:3.0},{:3.0},{:3.0}) Δ={:.0}",
            x, y, mi[0], mi[1], mi[2], go[0], go[1], go[2], d
        );
    }
}

fn write_ppm(path: &str, w: usize, h: usize, rgba: &[u8]) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    write!(f, "P6\n{w} {h}\n255\n").unwrap();
    let mut rgb = Vec::with_capacity(w * h * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[0..3]);
    }
    f.write_all(&rgb).unwrap();
}

fn write_bmp(path: &str, w: usize, h: usize, rgba: &[u8]) {
    let row = w * 3;
    let pad = (4 - (row % 4)) % 4;
    let stride = row + pad;
    let img = stride * h;
    let mut out = Vec::with_capacity(54 + img);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + img) as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(img as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        for x in 0..w {
            let o = (y * w + x) * 4;
            out.push(rgba[o + 2]);
            out.push(rgba[o + 1]);
            out.push(rgba[o]);
        }
        out.extend(std::iter::repeat(0u8).take(pad));
    }
    std::fs::write(path, out).unwrap();
}

fn read_ppm(path: &str) -> (usize, usize, Vec<u8>) {
    let data = std::fs::read(path).expect("read golden");
    assert_eq!(&data[0..2], b"P6");
    let mut i = 2;
    let mut nums = [0usize; 3];
    let mut ni = 0;
    while ni < 3 {
        while i < data.len() && (data[i] as char).is_whitespace() {
            i += 1;
        }
        if data[i] == b'#' {
            while i < data.len() && data[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let mut v = 0usize;
        while i < data.len() && data[i].is_ascii_digit() {
            v = v * 10 + usize::from(data[i] - b'0');
            i += 1;
        }
        nums[ni] = v;
        ni += 1;
    }
    i += 1;
    (nums[0], nums[1], data[i..].to_vec())
}
