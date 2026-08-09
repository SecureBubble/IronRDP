//! Replay real Windows RFX-Progressive WireToSurface2 payloads through one
//! persistent `ProgressiveDecoder` and diff each frame against a FreeRDP golden.
//!
//! Sibling of `rlex_check`/`nsc_check`, adapted for the STATEFUL progressive path:
//! progressive frames are not independent (DIFFERENCE tiles add onto retained
//! coefficients, UPGRADE passes refine them), so the whole set is replayed in
//! wire order through a single decoder and diffed after each PDU.
//!
//! Usage:
//!   prog_check <prog-set-dir>
//!
//! Directory contents (mirrors nsc-set discipline):
//!   frame-NNN.rfxprog       raw WireToSurface2 bitmapData (what decode_bitmap consumes)
//!   frame-NNN-golden.ppm    binary P6 RGB, FULL surface after replaying 0..N
//!   manifest.csv            optional; `kind` column (first|difference|upgrade) is
//!                           printed if present, otherwise ignored
//!
//! Surface dimensions are taken from the golden PPM (full-surface each frame).
//! Our decoder keys tile state by surface_id alone, so codec_context_id is a
//! don't-care here (we pass the frame index).
//!
//! Prints per-frame max per-channel abs diff + bad-pixel count and an overall
//! worst. The whole point: prove ironrdp's current base-quant (`2^(quant-1)`)
//! reconstructs real Windows progressive bytes with max_diff 0.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use ironrdp_graphics::progressive::ProgressiveDecoder;

const SURFACE_ID: u16 = 1;
const TILE: usize = 64;

fn read_ppm(path: &Path) -> (u32, u32, Vec<u8>) {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(bytes.starts_with(b"P6"), "{} is not P6 PPM", path.display());
    let mut idx = 2;
    let mut nums = [0u32; 3]; // w, h, maxval
    let mut got = 0;
    while got < 3 {
        while idx < bytes.len() && (bytes[idx] as char).is_whitespace() {
            idx += 1;
        }
        if idx < bytes.len() && bytes[idx] == b'#' {
            while idx < bytes.len() && bytes[idx] != b'\n' {
                idx += 1;
            }
            continue;
        }
        let start = idx;
        while idx < bytes.len() && (bytes[idx] as char).is_ascii_digit() {
            idx += 1;
        }
        nums[got] = std::str::from_utf8(&bytes[start..idx]).unwrap().parse().unwrap();
        got += 1;
    }
    idx += 1; // single whitespace after maxval
    (nums[0], nums[1], bytes[idx..].to_vec())
}

/// Optional manifest: map idx -> kind label. Best-effort; header column named
/// "kind" (case-insensitive) is used if present.
fn read_kinds(dir: &Path) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = fs::read_to_string(dir.join("manifest.csv")) else {
        return out;
    };
    let mut kind_col = None;
    let mut idx_col = 0usize;
    for (li, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if li == 0 {
            for (c, name) in cols.iter().enumerate() {
                match name.to_ascii_lowercase().as_str() {
                    "kind" => kind_col = Some(c),
                    "idx" => idx_col = c,
                    _ => {}
                }
            }
            continue;
        }
        if let (Some(kc), Some(idx)) = (kind_col, cols.get(idx_col).and_then(|v| v.parse::<u32>().ok())) {
            if let Some(k) = cols.get(kc) {
                out.insert(idx, (*k).to_string());
            }
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: prog_check <prog-set-dir>");
        std::process::exit(2);
    }
    let dir = Path::new(&args[0]);
    assert!(dir.is_dir(), "{} is not a directory", dir.display());

    // Enumerate frames from the payload files themselves (contiguous NNN order).
    let mut indices: Vec<u32> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("frame-")
                .and_then(|r| r.strip_suffix(".rfxprog"))
                .and_then(|n| n.parse::<u32>().ok())
        })
        .collect();
    indices.sort_unstable();
    assert!(!indices.is_empty(), "no frame-NNN.rfxprog files in {}", dir.display());

    let kinds = read_kinds(dir);

    // Surface dimensions come from the first golden (full-surface goldens).
    let first_golden = dir.join(format!("frame-{:03}-golden.ppm", indices[0]));
    let (surf_w, surf_h, _) = read_ppm(&first_golden);
    let (surf_w, surf_h) = (surf_w as usize, surf_h as usize);
    println!("prog-set: {} frames, surface {surf_w}x{surf_h}\n", indices.len());

    // Persistent decoder + persistent RGBA surface buffer (composited across frames).
    let mut decoder = ProgressiveDecoder::new();
    let mut surface = vec![0u8; surf_w * surf_h * 4];

    let tiles_wide = surf_w.div_ceil(TILE);
    let mut total = 0u32;
    let mut failed = 0u32;
    let mut worst = 0u8;

    for &idx in &indices {
        let payload = dir.join(format!("frame-{idx:03}.rfxprog"));
        let bitmap = fs::read(&payload).unwrap_or_else(|e| panic!("read {}: {e}", payload.display()));

        let decoded = match decoder.decode_bitmap(SURFACE_ID, idx, surf_w as u16, surf_h as u16, &bitmap) {
            Ok(t) => t,
            Err(e) => {
                println!("frame {idx:03}: DECODE ERROR: {e}  <-- FAIL");
                failed += 1;
                total += 1;
                worst = 255;
                continue;
            }
        };

        let decoded_len = decoded.len();

        // Per-tile diagnostic: which freshly-decoded tiles are badly wrong?
        // (Set PROG_TILE_DEBUG=1.) Reads the golden for this frame lazily below;
        // here we just stash the decoded tile coords + a max-diff we fill in after
        // compositing (compare the tile region against the golden).
        let tile_debug = std::env::var("PROG_TILE_DEBUG").is_ok();

        // Composite decoded tiles into the persistent surface, committing ONLY
        // the region-clipped spans (tile.clips) — exactly what the real client
        // does, and what FreeRDP does (blit only tile ∩ region rects). A tile
        // with no clips is decode-only and commits nothing.
        for tile in &decoded {
            let tx = usize::from(tile.x_idx) * TILE;
            let ty = usize::from(tile.y_idx) * TILE;
            debug_assert!(usize::from(tile.x_idx) < tiles_wide);
            for clip in &tile.clips {
                let cx = usize::from(clip.x);
                let cy = usize::from(clip.y);
                let cw = usize::from(clip.w);
                let ch = usize::from(clip.h);
                for row in 0..ch {
                    let sy = cy + row;
                    // source offset within the 64x64 tile
                    let ly = (cy - ty) + row;
                    let lx = cx - tx;
                    let src = (ly * TILE + lx) * 4;
                    let dst = (sy * surf_w + cx) * 4;
                    surface[dst..dst + cw * 4].copy_from_slice(&tile.pixels[src..src + cw * 4]);
                }
            }
        }

        // Diff against the golden for this frame, if present.
        let golden = dir.join(format!("frame-{idx:03}-golden.ppm"));
        if !golden.exists() {
            println!("frame {idx:03}: (no golden, skipped diff)");
            continue;
        }
        let (gw, gh, grgb) = read_ppm(&golden);
        assert_eq!((gw as usize, gh as usize), (surf_w, surf_h), "golden dims changed at frame {idx}");

        if tile_debug {
            for tile in &decoded {
                let tx = usize::from(tile.x_idx) * TILE;
                let ty = usize::from(tile.y_idx) * TILE;
                let mut tmax = 0u8;
                let mut osum = 0u64;
                let mut gsum = 0u64;
                let mut cnt = 0u64;
                for row in 0..TILE {
                    let sy = ty + row;
                    if sy >= surf_h {
                        break;
                    }
                    let w = TILE.min(surf_w - tx);
                    for col in 0..w {
                        let gi = (sy * surf_w + tx + col) * 3;
                        let si = (sy * surf_w + tx + col) * 4;
                        let d = surface[si]
                            .abs_diff(grgb[gi])
                            .max(surface[si + 1].abs_diff(grgb[gi + 1]))
                            .max(surface[si + 2].abs_diff(grgb[gi + 2]));
                        tmax = tmax.max(d);
                        osum += u64::from(surface[si]) + u64::from(surface[si + 1]) + u64::from(surface[si + 2]);
                        gsum += u64::from(grgb[gi]) + u64::from(grgb[gi + 1]) + u64::from(grgb[gi + 2]);
                        cnt += 3;
                    }
                }
                if tmax > 32 {
                    println!(
                        "   frame {idx:03} tile({},{}) self-max={tmax} mean o/g={:.0}/{:.0}",
                        tile.x_idx,
                        tile.y_idx,
                        osum as f64 / cnt as f64,
                        gsum as f64 / cnt as f64
                    );
                }
            }
        }

        let n = surf_w * surf_h;
        let mut max_diff = 0u8;
        let mut bad = 0u32;
        let mut first_bad = None;
        let mut sum_abs = 0u64; // sum of per-channel abs diff over all channels
        let mut gt8 = 0u32; // pixels with max-channel diff > 8
        let mut gt32 = 0u32; //   ... > 32
        let mut gt96 = 0u32; //   ... > 96
        for i in 0..n {
            let (or, og, ob) = (surface[i * 4], surface[i * 4 + 1], surface[i * 4 + 2]);
            let (gr, gg, gb) = (grgb[i * 3], grgb[i * 3 + 1], grgb[i * 3 + 2]);
            sum_abs += u64::from(or.abs_diff(gr)) + u64::from(og.abs_diff(gg)) + u64::from(ob.abs_diff(gb));
            let d = or.abs_diff(gr).max(og.abs_diff(gg)).max(ob.abs_diff(gb));
            if d != 0 {
                bad += 1;
                if first_bad.is_none() {
                    first_bad = Some((i % surf_w, i / surf_w, (or, og, ob), (gr, gg, gb)));
                }
            }
            if d > 8 {
                gt8 += 1;
            }
            if d > 32 {
                gt32 += 1;
            }
            if d > 96 {
                gt96 += 1;
            }
            max_diff = max_diff.max(d);
        }
        let mad = sum_abs as f64 / (n as f64 * 3.0); // mean abs diff per channel
        let pct = |c: u32| 100.0 * f64::from(c) / n as f64;

        // Mean luma (BT.601-ish) of our surface vs golden — trajectory check:
        // README says golden brightness climbs 65->103 as upgrades refine.
        let mut osum = 0u64;
        let mut gsum = 0u64;
        for i in 0..n {
            osum += u64::from(surface[i * 4]) + u64::from(surface[i * 4 + 1]) + u64::from(surface[i * 4 + 2]);
            gsum += u64::from(grgb[i * 3]) + u64::from(grgb[i * 3 + 1]) + u64::from(grgb[i * 3 + 2]);
        }
        let omean = osum as f64 / (n as f64 * 3.0);
        let gmean = gsum as f64 / (n as f64 * 3.0);

        total += 1;
        worst = worst.max(max_diff);
        let kind = kinds.get(&idx).map(|k| k.as_str()).unwrap_or("?");
        let status = if max_diff <= 2 { "ok " } else { "FAIL" };
        let (x, y, o, g) = first_bad.unwrap_or((0, 0, (0, 0, 0), (0, 0, 0)));
        if max_diff > 2 {
            failed += 1;
        }
        let _ = (bad, x, y, o, g);
        println!(
            "frame {idx:03} [{kind:>10}] tiles={decoded_len:>4}  max={max_diff:>3} mad={mad:5.2}  >8:{:5.1}% >32:{:5.1}% >96:{:5.1}%  mean o/g={omean:6.1}/{gmean:6.1}  {status}",
            pct(gt8), pct(gt32), pct(gt96)
        );
    }

    println!("\n=== {total} frames diffed, {failed} FAILED, worst max_diff={worst} ===");
    if failed == 0 && worst == 0 {
        println!("PASS: ironrdp current base-quant (2^(quant-1)) reconstructs real Windows progressive bytes exactly.");
    }
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
