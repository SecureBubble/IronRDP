//! Throwaway diagnostic: frame-walk a decompressed RDPGFX PDU capture by its 8-byte header
//! (cmdId, flags, pduLength) and decode only the AVC `WireToSurface1` bodies, dumping the
//! dest_rect + regionRects. This reveals whether the green-border regionRects cover the whole
//! surface (keyframe) or a sub-rect. Run:
//!   FIXTURE=/c/Users/.../REPRO-green-border-dcc7af41.bin cargo test -p ironrdp-egfx --test dump_avc_regions -- --nocapture

use ironrdp_core::ReadCursor;
use ironrdp_egfx::pdu::{Avc420BitmapStream, Avc444BitmapStream, Codec1Type, WireToSurface1Pdu};
use ironrdp_pdu::Decode as _;

const CMD_WIRE_TO_SURFACE_1: u16 = 0x0001;
const CMD_CREATE_SURFACE: u16 = 0x0009;

#[test]
fn dump_avc_regions() {
    let Ok(path) = std::env::var("FIXTURE") else {
        eprintln!("set FIXTURE=<path to decompressed RDPGFX PDU capture>");
        return;
    };
    let bytes = std::fs::read(&path).expect("read fixture");
    eprintln!("fixture = {} bytes", bytes.len());

    let mut off = 0usize;
    let mut frame = 0usize;
    while off + 8 <= bytes.len() {
        let cmd = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        let pdu_len = u32::from_le_bytes([bytes[off + 4], bytes[off + 5], bytes[off + 6], bytes[off + 7]]) as usize;
        if pdu_len < 8 || off + pdu_len > bytes.len() {
            eprintln!("bad pduLength {pdu_len} at off {off} (cmd={cmd:#06x}); stop");
            break;
        }
        let body = &bytes[off + 8..off + pdu_len];

        if cmd == CMD_CREATE_SURFACE {
            // body: surfaceId(2) width(2) height(2) pixelFormat(1)
            if body.len() >= 6 {
                let sid = u16::from_le_bytes([body[0], body[1]]);
                let w = u16::from_le_bytes([body[2], body[3]]);
                let h = u16::from_le_bytes([body[4], body[5]]);
                eprintln!("CreateSurface id={sid} {w}x{h}");
            }
        } else if cmd == CMD_WIRE_TO_SURFACE_1 {
            let mut c = ReadCursor::new(body);
            if let Ok(w) = WireToSurface1Pdu::decode(&mut c) {
                let r = &w.destination_rectangle;
                match w.codec_id {
                    Codec1Type::Avc420 => {
                        let mut bc = ReadCursor::new(&w.bitmap_data);
                        if let Ok(s) = Avc420BitmapStream::decode(&mut bc) {
                            eprintln!(
                                "F{frame} AVC420 surf={} dest=({},{})-({},{}) nRegions={} data={}",
                                w.surface_id, r.left, r.top, r.right, r.bottom, s.rectangles.len(), s.data.len()
                            );
                            for rr in &s.rectangles {
                                eprintln!("    region ({},{})-({},{})", rr.left, rr.top, rr.right, rr.bottom);
                            }
                            frame += 1;
                        }
                    }
                    Codec1Type::Avc444 | Codec1Type::Avc444v2 => {
                        let mut bc = ReadCursor::new(&w.bitmap_data);
                        if let Ok(s) = Avc444BitmapStream::decode(&mut bc) {
                            eprintln!(
                                "F{frame} AVC444 surf={} dest=({},{})-({},{}) enc={:?} s1_regions={}",
                                w.surface_id, r.left, r.top, r.right, r.bottom, s.encoding, s.stream1.rectangles.len()
                            );
                            for rr in &s.stream1.rectangles {
                                eprintln!("    s1 region ({},{})-({},{})", rr.left, rr.top, rr.right, rr.bottom);
                            }
                            frame += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        off += pdu_len;
    }
    eprintln!("total AVC frames: {frame}");
}
