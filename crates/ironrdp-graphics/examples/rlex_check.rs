//! Decode ClearCodec RLEX strip payloads and diff against FreeRDP goldens.
//!
//! Usage:
//!   rlex_check <one.clearcodec> <one-golden.ppm> [width height]
//!   rlex_check <rlex-set-dir>          (dir with manifest.csv + strip-NNN.clearcodec + strip-NNN-golden.ppm)
//!
//! `.clearcodec` = the raw ClearCodec WireToSurface1 bitmapData (what decode_clearcodec receives).
//! golden `.ppm`  = binary P6 RGB, width x height.
//! Our decoder returns BGRA; we compare RGB channels and report max per-channel abs diff.

use std::fs;
use std::path::Path;

use ironrdp_graphics::clearcodec::ClearCodecDecoder;

fn read_ppm(path: &Path) -> (u32, u32, Vec<u8>) {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    // Parse P6 header: "P6\n<w> <h>\n<maxval>\n" (whitespace-tolerant).
    assert!(bytes.starts_with(b"P6"), "{} is not P6 PPM", path.display());
    let mut idx = 2;
    let mut nums = [0u32; 3]; // w, h, maxval
    let mut got = 0;
    while got < 3 {
        // skip whitespace / comments
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

/// Returns max per-channel abs diff, and (x, y) of the first mismatching pixel.
fn diff_one(path_cc: &Path, path_ppm: &Path, w: u16, h: u16) -> (u8, Option<(u32, u32)>, u32) {
    let cc = fs::read(path_cc).unwrap_or_else(|e| panic!("read {}: {e}", path_cc.display()));
    let (gw, gh, grgb) = read_ppm(path_ppm);
    assert_eq!((gw, gh), (u32::from(w), u32::from(h)), "golden dims mismatch for {}", path_cc.display());

    let mut dec = ClearCodecDecoder::new();
    let bgra = match dec.decode(&cc, w, h) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  DECODE ERROR {}: {e:?}", path_cc.display());
            return (255, Some((0, 0)), 0);
        }
    };

    let n = usize::from(w) * usize::from(h);
    let mut max_diff = 0u8;
    let mut first_bad = None;
    let mut bad_count = 0u32;
    for i in 0..n {
        // ours BGRA -> RGB
        let (ob, og, or) = (bgra[i * 4], bgra[i * 4 + 1], bgra[i * 4 + 2]);
        let (gr, gg, gb) = (grgb[i * 3], grgb[i * 3 + 1], grgb[i * 3 + 2]);
        let d = or.abs_diff(gr).max(og.abs_diff(gg)).max(ob.abs_diff(gb));
        if d != 0 {
            bad_count += 1;
            if first_bad.is_none() {
                first_bad = Some((i as u32 % u32::from(w), i as u32 / u32::from(w)));
                eprintln!(
                    "  first bad px#{i} (x={}, y={}): ours BGR=({ob},{og},{or}) golden RGB=({gr},{gg},{gb})",
                    i as u32 % u32::from(w),
                    i as u32 / u32::from(w)
                );
            }
        }
        max_diff = max_diff.max(d);
    }
    (max_diff, first_bad, bad_count)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: rlex_check <file.clearcodec> <golden.ppm> [w h] | <rlex-set-dir>");
        std::process::exit(2);
    }

    // Directory mode: manifest.csv columns idx,frame,rect,w,h,payloadLen
    let p = Path::new(&args[0]);
    if p.is_dir() {
        let manifest = fs::read_to_string(p.join("manifest.csv")).expect("read manifest.csv");
        let mut total = 0u32;
        let mut failed = 0u32;
        let mut worst = 0u8;
        for (li, line) in manifest.lines().enumerate() {
            if li == 0 || line.trim().is_empty() {
                continue; // header
            }
            let cols: Vec<&str> = line.split(',').collect();
            // idx,frame,l,t,r,b,w,h,payloadLen,rlexBlocks
            let idx: u32 = cols[0].trim().parse().unwrap();
            let w: u16 = cols[6].trim().parse().unwrap();
            let h: u16 = cols[7].trim().parse().unwrap();
            let cc = p.join(format!("strip-{idx:03}.clearcodec"));
            let ppm = p.join(format!("strip-{idx:03}-golden.ppm"));
            if !cc.exists() {
                continue;
            }
            let (md, _fb, bad) = diff_one(&cc, &ppm, w, h);
            total += 1;
            worst = worst.max(md);
            if md != 0 {
                failed += 1;
                println!("strip {idx:03} ({w}x{h}): max_diff={md} bad_px={bad}  <-- FAIL");
            }
        }
        println!("\n=== {total} strips, {failed} FAILED, worst max_diff={worst} ===");
    } else {
        let ppm = Path::new(&args[1]);
        let (w, h) = if args.len() >= 4 {
            (args[2].parse().unwrap(), args[3].parse().unwrap())
        } else {
            let (gw, gh, _) = read_ppm(ppm);
            (gw as u16, gh as u16)
        };
        let (md, _fb, bad) = diff_one(p, ppm, w, h);
        println!("{}: max_diff={md} bad_px={bad}", p.display());
    }
}
