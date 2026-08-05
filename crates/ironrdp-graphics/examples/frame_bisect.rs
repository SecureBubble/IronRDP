//! Per-frame bisect: diff our replayed surf0 dumps against FreeRDP per-frame goldens.
//!
//! Usage: frame_bisect <our-dir> <golden-dir> [threshold=40]
//!   our-dir:    contains surf-0000.ppm, surf-0001.ppm, ... (from decode_capture DUMP_FRAMES=1)
//!   golden-dir: contains frameNNNN-end<ord>.ppm (sorted by name = frame order)
//!
//! For each frame it counts pixels whose max-channel abs diff exceeds `threshold`
//! (so the subtle progressive color residual is ignored and only streak-level errors
//! flag the culprit). The FIRST frame with a big-count spike is where reconstruction
//! diverges — the ops between it and the previous EndFrame contain the bug.

use std::fs;
use std::path::{Path, PathBuf};

fn read_ppm(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let bytes = fs::read(path).ok()?;
    if !bytes.starts_with(b"P6") {
        return None;
    }
    let mut idx = 2;
    let mut nums = [0u32; 3];
    let mut got = 0;
    while got < 3 {
        while idx < bytes.len() && (bytes[idx] as char).is_whitespace() {
            idx += 1;
        }
        let start = idx;
        while idx < bytes.len() && (bytes[idx] as char).is_ascii_digit() {
            idx += 1;
        }
        nums[got] = std::str::from_utf8(&bytes[start..idx]).ok()?.parse().ok()?;
        got += 1;
    }
    idx += 1;
    Some((nums[0], nums[1], bytes[idx..].to_vec()))
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 2 {
        eprintln!("usage: frame_bisect <our-dir> <golden-dir> [threshold=40]");
        std::process::exit(2);
    }
    let our_dir = Path::new(&a[0]);
    let golden_dir = Path::new(&a[1]);
    let threshold: u8 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    // Optional 4th arg: mask that drains the progressive residual so only outline
    // residue is counted. Either a P5 PGM, or a perop CSV (build the union of all
    // WTS1 ClearCodec rects +2px). Width fixed at 3200 (capture surface).
    const MW: usize = 3200;
    const MH: usize = 1308;
    let mask: Option<Vec<u8>> = a.get(3).map(|p| {
        if p.ends_with(".csv") {
            // outline-rects CSV: frame,l,t,r,b,w,h,subcodec  (l,t,r,b = cols 1..4, subcodec = last)
            // Optional env MASK_SUBCODEC filters to one subcodec (e.g. NSCodec / RLEX / RAW).
            let filter = std::env::var("MASK_SUBCODEC").ok();
            let text = fs::read_to_string(p).unwrap();
            let mut m = vec![0u8; MW * MH];
            let mut rects = 0u32;
            for (i, line) in text.lines().enumerate() {
                if i == 0 { continue; }
                let c: Vec<&str> = line.split(',').collect();
                if c.len() < 8 { continue; }
                let subcodec = c[c.len() - 1].trim();
                if let Some(f) = &filter {
                    if subcodec != f { continue; }
                }
                let (l, t, r, b): (i64, i64, i64, i64) = (
                    c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0),
                    c[3].parse().unwrap_or(0), c[4].parse().unwrap_or(0),
                );
                rects += 1;
                let (x0, y0) = ((l - 2).max(0) as usize, (t - 2).max(0) as usize);
                let (x1, y1) = ((r + 2).min(MW as i64) as usize, (b + 2).min(MH as i64) as usize);
                for y in y0..y1 {
                    for x in x0..x1 {
                        m[y * MW + x] = 255;
                    }
                }
            }
            let cov = m.iter().filter(|&&v| v != 0).count();
            eprintln!("mask from CSV: {rects} rects (filter={filter:?}), coverage {:.1}%", 100.0 * cov as f64 / (MW * MH) as f64);
            m
        } else {
            let bytes = fs::read(p).unwrap_or_else(|e| panic!("read mask {p}: {e}"));
            let mut idx = 2;
            let mut nums = [0u32; 3];
            let mut got = 0;
            while got < 3 {
                while idx < bytes.len() && (bytes[idx] as char).is_whitespace() { idx += 1; }
                let s = idx;
                while idx < bytes.len() && (bytes[idx] as char).is_ascii_digit() { idx += 1; }
                nums[got] = std::str::from_utf8(&bytes[s..idx]).unwrap().parse().unwrap();
                got += 1;
            }
            idx += 1;
            bytes[idx..].to_vec()
        }
    });

    // Sorted golden list = frame order.
    let mut goldens: Vec<PathBuf> = fs::read_dir(golden_dir)
        .expect("read golden dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "ppm").unwrap_or(false))
        .collect();
    goldens.sort();
    eprintln!("{} golden frames, threshold={threshold}", goldens.len());

    let mut prev_big = 0u64;
    let mut first_spike: Option<usize> = None;
    for (i, gpath) in goldens.iter().enumerate() {
        let our_path = our_dir.join(format!("surf-{i:04}.ppm"));
        let (Some((gw, gh, g)), Some((ow, oh, o))) = (read_ppm(gpath), read_ppm(&our_path)) else {
            println!("frame {i:04}: MISSING (ours={} golden={})", our_path.display(), gpath.display());
            continue;
        };
        if (gw, gh) != (ow, oh) {
            println!("frame {i:04}: DIM MISMATCH ours {ow}x{oh} vs golden {gw}x{gh}");
            continue;
        }
        let n = (gw * gh) as usize;
        let mut big = 0u64;
        let mut first_bad: Option<(u32, u32)> = None;
        let mut worst = 0u8;
        for p in 0..n.min(g.len() / 3).min(o.len() / 3) {
            if let Some(m) = &mask {
                if m.get(p).copied().unwrap_or(0) == 0 {
                    continue; // outside the mask (progressive-only area) — ignore
                }
            }
            let d = o[p * 3].abs_diff(g[p * 3]).max(o[p * 3 + 1].abs_diff(g[p * 3 + 1])).max(o[p * 3 + 2].abs_diff(g[p * 3 + 2]));
            if d > threshold {
                big += 1;
                if first_bad.is_none() {
                    first_bad = Some((p as u32 % gw, p as u32 / gw));
                }
            }
            worst = worst.max(d);
        }
        let spike = big > prev_big + 500 && big > 500;
        let name = gpath.file_name().unwrap().to_string_lossy();
        if spike || big > 0 || i < 3 {
            println!(
                "frame {i:04} [{name}]: big(>{threshold})={big} worst={worst} first_bad={:?}{}",
                first_bad,
                if spike { "   <==== SPIKE (first divergence)" } else { "" }
            );
        }
        if spike && first_spike.is_none() {
            first_spike = Some(i);
        }
        prev_big = big;
    }
    match first_spike {
        Some(i) => println!("\n=== FIRST DIVERGENCE at frame {i:04} — inspect ops between EndFrame {} and {} in the flat stream ===", i.saturating_sub(1), i),
        None => println!("\n=== no big-diff spike found (streak may be below threshold, or ours matches golden) ==="),
    }
}
