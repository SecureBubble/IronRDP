//! golden_diff — decode a proxy `egfx-back-*.bin` with ironrdp's ProgressiveDecoder
//! and diff it, tile-by-tile, against a FreeRDP golden PPM (the ground-truth oracle).
//!
//! The `.bin` is the DECOMPRESSED (post-zgfx) RDPGFX PDU stream, framed exactly as
//! `egfx_golden.c` reads it: RDPGFX_HEADER { cmdId:u16, flags:u16, pduLength:u32 },
//! body = pduLength-8. We only need CREATE_SURFACE + WIRETOSURFACE_2(progressive).
//!
//! Usage: golden_diff <egfx-back.bin> <golden-surfN.ppm> [surfaceId=0]
//!   - writes iron-surfN.ppm next to nothing (stdout only reports stats)
//!   - prints the worst-diverging 64x64 tiles, classifying gray-vs-colorwrong.

use std::io::Write as _;

use ironrdp_graphics::progressive::ProgressiveDecoder;

fn rd16(p: &[u8]) -> u16 {
    u16::from(p[0]) | (u16::from(p[1]) << 8)
}
fn rd32(p: &[u8]) -> u32 {
    u32::from(p[0]) | (u32::from(p[1]) << 8) | (u32::from(p[2]) << 16) | (u32::from(p[3]) << 24)
}

const CMD_WIRETOSURFACE_2: u16 = 0x0002;
const CMD_CREATE_SURFACE: u16 = 0x0009;
const CMD_DELETE_SURFACE: u16 = 0x000a;
const CODEC_PROGRESSIVE: u16 = 0x0009;

struct Surf {
    w: usize,
    h: usize,
    rgb: Vec<u8>, // w*h*3
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <egfx-back.bin> <golden.ppm> [surfaceId]", args[0]);
        std::process::exit(2);
    }
    let bin_path = &args[1];
    let golden_path = &args[2];
    let want_sid: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);

    let buf = std::fs::read(bin_path).expect("read bin");
    println!("loaded {} bytes from {bin_path}", buf.len());

    let mut decoder = ProgressiveDecoder::new();
    let mut surfaces: std::collections::HashMap<u16, Surf> = std::collections::HashMap::new();
    let mut off = 0usize;
    let mut cmd_hist: std::collections::BTreeMap<u16, u32> = std::collections::BTreeMap::new();
    let (mut n_wts2, mut n_create, mut n_dec_err, mut n_tiles) = (0u32, 0u32, 0u32, 0u32);

    while off + 8 <= buf.len() {
        let cmd_id = rd16(&buf[off..]);
        let pdu_len = rd32(&buf[off + 4..]) as usize;
        if pdu_len < 8 || off + pdu_len > buf.len() {
            println!("stop: bad PDU at off={off} cmdId=0x{cmd_id:04x} pduLen={pdu_len}");
            break;
        }
        let body = &buf[off + 8..off + pdu_len];

        *cmd_hist.entry(cmd_id).or_insert(0u32) += 1;

        match cmd_id {
            CMD_CREATE_SURFACE if body.len() >= 7 => {
                let sid = rd16(body);
                let w = rd16(&body[2..]) as usize;
                let h = rd16(&body[4..]) as usize;
                if w > 0 && h > 0 {
                    surfaces.insert(sid, Surf { w, h, rgb: vec![0u8; w * h * 3] });
                    n_create += 1;
                    eprintln!("  === CREATE_SURFACE id={sid} {w}x{h} (wts2 so far={n_wts2}) ===");
                    println!("[wts2 seen so far={n_wts2}] CREATE_SURFACE id={sid} {w}x{h}");
                    // Match FreeRDP egfx_golden: progressive_create_surface_context
                    // resets tile state on each (re)create. Gated so we can A/B it.
                    if std::env::var("RESET_ON_CREATE").is_ok() {
                        decoder.reset();
                    }
                }
            }
            CMD_DELETE_SURFACE if body.len() >= 2 => {
                surfaces.remove(&rd16(body));
            }
            CMD_WIRETOSURFACE_2 if body.len() >= 13 => {
                let sid = rd16(body);
                let codec_id = rd16(&body[2..]);
                let ctx_id = rd32(&body[4..]);
                let mut bmp_len = rd32(&body[9..]) as usize;
                let data = &body[13..];
                if bmp_len > data.len() {
                    bmp_len = data.len();
                }
                if codec_id == CODEC_PROGRESSIVE {
                    if std::env::var("SHOW_CTX").is_ok() && n_wts2 < 30 {
                        eprintln!("  wts2 #{n_wts2} sid={sid} ctxId={ctx_id} bmpLen={bmp_len}");
                    }
                    // FreeRDP keys the progressive context by surface_id alone.
                    // The server bumps codecContextId every frame; keying by it
                    // discards temporal `current` state. FORCE_CTX0 emulates the
                    // surface-only keying to A/B the fix.
                    let eff_ctx = if std::env::var("FORCE_CTX0").is_ok() { 0 } else { ctx_id };
                    if let Some(s) = surfaces.get_mut(&sid) {
                        let (w, h) = (s.w as u16, s.h as u16);
                        match decoder.decode_bitmap(sid, eff_ctx, w, h, &data[..bmp_len]) {
                            Ok(tiles) => {
                                n_wts2 += 1;
                                for t in tiles {
                                    n_tiles += 1;
                                    blit_tile(s, &t);
                                }
                            }
                            Err(e) => {
                                n_dec_err += 1;
                                if n_dec_err <= 8 {
                                    eprintln!("  decode err sid={sid} frame~{n_wts2}: {e}");
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        off += pdu_len;
    }

    println!(
        "\nsummary: create={n_create} wts2-progressive={n_wts2} tiles={n_tiles} decodeErrors={n_dec_err}"
    );
    print!("cmd histogram:");
    for (k, v) in &cmd_hist {
        print!(" 0x{k:04x}={v}");
    }
    println!();

    let Some(s) = surfaces.get(&want_sid) else {
        eprintln!("surface {want_sid} not found");
        return;
    };

    // Write our PPM next to the golden.
    let out_ppm = format!("iron-surf{want_sid}-{}x{}.ppm", s.w, s.h);
    write_ppm(&out_ppm, s);
    write_bmp(&format!("iron-surf{want_sid}.bmp"), s);
    println!("wrote {out_ppm} + iron-surf{want_sid}.bmp");

    // Load golden and diff.
    let (gw, gh, grgb) = read_ppm(golden_path);
    if gw != s.w || gh != s.h {
        eprintln!("SIZE MISMATCH: iron {}x{} vs golden {gw}x{gh}", s.w, s.h);
        return;
    }
    diff_tiles(s, &grgb);

    // [DIAG] per-pixel white-on-dark dots in the PROGRESSIVE-only surface. Golden
    // dark (<150) => wallpaper (progressive), not the unpainted (white) window area,
    // so this isolates whether the white speckles come from the progressive path.
    let mut dots = 0u64;
    for i in 0..(s.w * s.h) {
        let o = i * 3;
        let iron_white = s.rgb[o] >= 250 && s.rgb[o + 1] >= 250 && s.rgb[o + 2] >= 250;
        let gmax = grgb[o].max(grgb[o + 1]).max(grgb[o + 2]);
        if iron_white && gmax < 150 {
            dots += 1;
        }
    }
    println!("PROGRESSIVE-only white-on-dark dots: {dots}");
}

fn blit_tile(s: &mut Surf, t: &ironrdp_graphics::progressive::DecodedTile) {
    let x0 = usize::from(t.x_idx) * 64;
    let y0 = usize::from(t.y_idx) * 64;
    for ty in 0..64 {
        let dy = y0 + ty;
        if dy >= s.h {
            break;
        }
        for tx in 0..64 {
            let dx = x0 + tx;
            if dx >= s.w {
                break;
            }
            let src = (ty * 64 + tx) * 4; // RGBA
            let dst = (dy * s.w + dx) * 3; // RGB
            s.rgb[dst] = t.pixels[src];
            s.rgb[dst + 1] = t.pixels[src + 1];
            s.rgb[dst + 2] = t.pixels[src + 2];
        }
    }
}

fn write_ppm(path: &str, s: &Surf) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).expect("create ppm"));
    write!(f, "P6\n{} {}\n255\n", s.w, s.h).unwrap();
    f.write_all(&s.rgb).unwrap();
}

/// Write RGB surface as a bottom-up 24bpp BMP (native BGR, no per-pixel host loop needed).
fn write_bmp(path: &str, s: &Surf) {
    let row_bytes = s.w * 3;
    let pad = (4 - (row_bytes % 4)) % 4;
    let stride = row_bytes + pad;
    let img_size = stride * s.h;
    let file_size = 54 + img_size;
    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(file_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(s.w as i32).to_le_bytes());
    out.extend_from_slice(&(s.h as i32).to_le_bytes()); // positive = bottom-up
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(img_size as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..s.h).rev() {
        let ro = y * s.w * 3;
        for x in 0..s.w {
            let o = ro + x * 3;
            out.push(s.rgb[o + 2]); // B
            out.push(s.rgb[o + 1]); // G
            out.push(s.rgb[o]); // R
        }
        out.extend(std::iter::repeat(0u8).take(pad));
    }
    std::fs::write(path, out).expect("write bmp");
}

fn read_ppm(path: &str) -> (usize, usize, Vec<u8>) {
    let data = std::fs::read(path).expect("read golden ppm");
    // Parse "P6\n<w> <h>\n255\n" header (whitespace-tolerant).
    assert_eq!(&data[0..2], b"P6", "golden not P6");
    let mut i = 2;
    let mut nums = [0usize; 3];
    let mut ni = 0;
    while ni < 3 {
        while i < data.len() && (data[i] as char).is_whitespace() {
            i += 1;
        }
        // skip comments
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
    i += 1; // single whitespace after maxval
    (nums[0], nums[1], data[i..].to_vec())
}

/// Per-channel mean of a tile region in an RGB buffer.
fn tile_mean(rgb: &[u8], w: usize, h: usize, x0: usize, y0: usize) -> [f64; 3] {
    let (mut sr, mut sg, mut sb, mut n) = (0f64, 0f64, 0f64, 0f64);
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
            sr += f64::from(rgb[o]);
            sg += f64::from(rgb[o + 1]);
            sb += f64::from(rgb[o + 2]);
            n += 1.0;
        }
    }
    if n == 0.0 {
        return [0.0; 3];
    }
    [sr / n, sg / n, sb / n]
}

fn sat(c: [f64; 3]) -> f64 {
    c.iter().cloned().fold(f64::MIN, f64::max) - c.iter().cloned().fold(f64::MAX, f64::min)
}

fn diff_tiles(s: &Surf, golden: &[u8]) {
    let tiles_wide = s.w.div_ceil(64);
    let tiles_high = s.h.div_ceil(64);
    let mut worst: Vec<(f64, usize, usize, [f64; 3], [f64; 3])> = Vec::new();
    let (mut sum_diff, mut ntile) = (0f64, 0f64);
    let (mut gray_bug, mut color_bug) = (0u32, 0u32);

    for tyi in 0..tiles_high {
        for txi in 0..tiles_wide {
            let (x0, y0) = (txi * 64, tyi * 64);
            let mi = tile_mean(&s.rgb, s.w, s.h, x0, y0);
            let go = tile_mean(golden, s.w, s.h, x0, y0);
            let d = (mi[0] - go[0]).abs() + (mi[1] - go[1]).abs() + (mi[2] - go[2]).abs();
            sum_diff += d;
            ntile += 1.0;
            // Classify: golden is colorful (sat>25) but ours is gray (sat<10) => chroma->0.
            if sat(go) > 25.0 && sat(mi) < 10.0 {
                gray_bug += 1;
            } else if d > 60.0 {
                color_bug += 1;
            }
            worst.push((d, x0, y0, mi, go));
        }
    }
    worst.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    // Separate "matching" tiles from outliers so window-region ClearCodec content
    // (which the progressive-only golden can't represent) doesn't mask a clean
    // wallpaper. Report the mean over the matching set.
    let outlier_thresh = 60.0;
    let matching: Vec<f64> = worst.iter().map(|t| t.0).filter(|d| *d <= outlier_thresh).collect();
    let outliers = worst.len() - matching.len();
    let match_mean = if matching.is_empty() {
        0.0
    } else {
        matching.iter().sum::<f64>() / matching.len() as f64
    };
    let lt5 = worst.iter().filter(|t| t.0 < 5.0).count();
    let lt15 = worst.iter().filter(|t| t.0 < 15.0).count();
    println!(
        "\n=== TILE DIFF vs golden ===  tiles={} mean|Δ|/tile={:.1}",
        ntile as u32,
        sum_diff / ntile
    );
    println!(
        "matching tiles (Δ<=60): {}  mean|Δ|={:.2}   |   outliers (Δ>60): {}",
        matching.len(),
        match_mean,
        outliers
    );
    println!("distribution: Δ<5 -> {lt5} tiles,  Δ<15 -> {lt15} tiles (of {})", worst.len());
    println!("gray-bug tiles (golden colorful, ours gray): {gray_bug}");
    println!("color-wrong tiles (|Δ|>60, not gray):        {color_bug}");
    println!("\nworst 25 tiles (x,y  iron(r,g,b)  golden(r,g,b)  |Δ|):");
    for (d, x, y, mi, go) in worst.iter().take(25) {
        println!(
            "  ({:4},{:4})  iron({:3.0},{:3.0},{:3.0})  gold({:3.0},{:3.0},{:3.0})  Δ={:.0}",
            x, y, mi[0], mi[1], mi[2], go[0], go[1], go[2], d
        );
    }
}
