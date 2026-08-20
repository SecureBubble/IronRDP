//! MS-RDPEGFX ALPHA codec (`RDPGFX_CODECID_ALPHA` = 0x000C) bitmap-stream decoder.
//!
//! The ALPHA codec rewrites ONLY the alpha channel of pixels already present in a
//! surface (drawn by a prior color codec in the same frame). RGB is preserved; only
//! the per-pixel alpha byte over the destination rectangle is replaced.
//!
//! This module is the pure, surface-independent stream decoder: it turns an ALPHA
//! `bitmap_data` blob into a row-major buffer of one alpha byte per pixel across the
//! destination rectangle (`width * height` bytes). Applying those bytes to a surface
//! buffer (leaving RGB untouched) is the caller's job.
//!
//! Mirrors FreeRDP `libfreerdp/gdi/gfx.c` `gdi_SurfaceCommand_Alpha` (wire parsing)
//! and `gdi_apply_alpha` (run application). See the per-item comments below for the
//! exact FreeRDP lines each rule is taken from.

use ironrdp_core::{DecodeResult, ReadCursor, invalid_field_err};

/// ALPHA stream signature. FreeRDP: `if (alphaSig != 0x414C) return ERROR_INVALID_DATA;`
/// (gfx.c ~line 940). The two ASCII bytes "AL" read as a little-endian `u16`.
const ALPHA_SIGNATURE: u16 = 0x414C;

/// Decode an ALPHA (`RDPGFX_CODECID_ALPHA`) bitmap stream into a row-major buffer of
/// one alpha byte per pixel across a `width` x `height` destination rectangle.
///
/// The returned `Vec<u8>` has length `width * height`; element `y * width + x` is the
/// alpha byte for the pixel at rect-local `(x, y)`. The caller composites these onto
/// an existing surface, writing ONLY the alpha channel of each pixel.
///
/// # Wire format (FreeRDP `gdi_SurfaceCommand_Alpha`)
///
/// - `alphaSig` (`u16` LE) — must equal [`ALPHA_SIGNATURE`] (0x414C), else a decode error.
/// - `compressed` (`u16` LE).
/// - If `compressed == 0`: a raw stream of one alpha byte per pixel, row-major, exactly
///   `width * height` bytes.
/// - If `compressed != 0`: an RLE stream of `(alphaValue: u8, count)` runs. `count` is a
///   `u8`; if that byte is `0xFF`, a `u16` count follows; if THAT is `0xFFFF`, a `u32`
///   count follows (escalation). Each escalated read REPLACES the count — the `0xFF` /
///   `0xFFFF` markers are escapes, not addends. A run means "the next `count` pixels
///   (row-major, wrapping across rows) all have this alpha".
///
/// There is intentionally NO special-casing for an alpha value of 0x00 or 0xFF: the
/// FreeRDP reference (this revision) does not special-case the alpha *value* — only the
/// *count* escalates. The only per-value effect is that FreeRDP's `gdi_apply_alpha`
/// clamps a run that overruns the rectangle to the rectangle's remaining pixels; we do
/// the same (`end = min(pos + count, pixel_count)`).
///
/// # Errors
///
/// Returns a decode error if the signature is wrong or the stream is shorter than the
/// declared format requires (mirrors FreeRDP's `ERROR_INVALID_DATA` length checks). A
/// zero `count` run is also rejected: FreeRDP would spin forever on it (it never
/// advances the write cursor), so we reject it rather than hang.
pub fn decode_alpha_stream(data: &[u8], width: u16, height: u16) -> DecodeResult<Vec<u8>> {
    let width = usize::from(width);
    let height = usize::from(height);
    let pixel_count = width * height;

    let mut cursor = ReadCursor::new(data);

    // FreeRDP: Stream_CheckAndLogRequiredLength(TAG, s, 4) then reads alphaSig + compressed.
    let alpha_sig = cursor
        .try_read_u16()
        .map_err(|_| invalid_field_err!("alphaSig", "ALPHA stream truncated before signature"))?;
    let compressed = cursor
        .try_read_u16()
        .map_err(|_| invalid_field_err!("compressed", "ALPHA stream truncated before compressed flag"))?;

    if alpha_sig != ALPHA_SIGNATURE {
        return Err(invalid_field_err!("alphaSig", "ALPHA signature mismatch (expected 0x414C)"));
    }

    let mut out = vec![0u8; pixel_count];

    if compressed == 0 {
        // Uncompressed: one alpha byte per pixel, row-major.
        // FreeRDP: Stream_CheckAndLogRequiredLengthOfSize(TAG, s, cmd->height, cmd->width).
        if cursor.len() < pixel_count {
            return Err(invalid_field_err!(
                "alphaData",
                "ALPHA uncompressed stream shorter than width*height"
            ));
        }
        let raw = cursor.read_slice(pixel_count);
        out.copy_from_slice(raw);
        return Ok(out);
    }

    // Compressed (RLE). FreeRDP walks the rect row-major, advancing a flat write cursor
    // by each run's count; a run's `count` pixels are set to its alpha value. `pos` here
    // is exactly FreeRDP's `(rect.top - top) * width + startOffsetX` (the running sum of
    // counts), and the outer loop ends when the rect is filled (`rect.top >= rect.bottom`).
    let mut pos = 0usize;
    while pos < pixel_count {
        // FreeRDP: Stream_CheckAndLogRequiredLength(TAG, s, 2); Read a; Read count(u8).
        let a = cursor
            .try_read_u8()
            .map_err(|_| invalid_field_err!("alphaRun", "ALPHA RLE stream truncated before run value"))?;
        let mut count = u32::from(
            cursor
                .try_read_u8()
                .map_err(|_| invalid_field_err!("alphaRun", "ALPHA RLE stream truncated before run count"))?,
        );

        // Count escalation. FreeRDP: `if (count >= 0xFF) { Read u16; if (count >= 0xFFFF) { Read u32; } }`.
        // For a byte-sized count `>= 0xFF` means `== 0xFF`; the follow-up read REPLACES count.
        if count >= 0xFF {
            count = u32::from(
                cursor
                    .try_read_u16()
                    .map_err(|_| invalid_field_err!("alphaRun", "ALPHA RLE stream truncated before u16 count"))?,
            );
            if count >= 0xFFFF {
                count = cursor
                    .try_read_u32()
                    .map_err(|_| invalid_field_err!("alphaRun", "ALPHA RLE stream truncated before u32 count"))?;
            }
        }

        // Guard against a zero-length run: FreeRDP never advances its write cursor for
        // count==0 and would loop forever. Reject instead of hanging.
        if count == 0 {
            return Err(invalid_field_err!("alphaRun", "ALPHA RLE run count is zero"));
        }

        // Apply the run, clamping to the rectangle's remaining pixels (FreeRDP's
        // gdi_apply_alpha stops at rect.bottom). `pos` still advances by the full count
        // so the row/rect bookkeeping matches FreeRDP even when the last run overruns.
        let end = pos.saturating_add(count as usize).min(pixel_count);
        for slot in &mut out[pos..end] {
            *slot = a;
        }
        pos = pos.saturating_add(count as usize);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Little-endian bytes for the 0x414C signature ("AL" as a LE u16 -> [0x4C, 0x41]).
    const SIG_LE: [u8; 2] = [0x4C, 0x41];

    #[test]
    fn rejects_bad_signature() {
        // Signature 0x0000, uncompressed, would-be data.
        let stream = [0x00, 0x00, 0x00, 0x00, 0x11, 0x22];
        let err = decode_alpha_stream(&stream, 2, 1).unwrap_err();
        assert!(format!("{err}").contains("alphaSig") || format!("{err:?}").contains("alphaSig"));
    }

    #[test]
    fn rejects_truncated_header() {
        let stream = [0x4C]; // only one byte, can't even read the signature
        assert!(decode_alpha_stream(&stream, 1, 1).is_err());
    }

    #[test]
    fn uncompressed_row_major() {
        // sig, compressed=0, then 6 alpha bytes for a 3x2 rect (row-major).
        let mut stream = Vec::new();
        stream.extend_from_slice(&SIG_LE);
        stream.extend_from_slice(&0u16.to_le_bytes()); // compressed = 0
        let alphas = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60];
        stream.extend_from_slice(&alphas);

        let out = decode_alpha_stream(&stream, 3, 2).unwrap();
        assert_eq!(out, alphas);
    }

    #[test]
    fn uncompressed_rejects_short_data() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&SIG_LE);
        stream.extend_from_slice(&0u16.to_le_bytes());
        stream.extend_from_slice(&[0x10, 0x20]); // only 2 bytes, need 4 for 2x2
        assert!(decode_alpha_stream(&stream, 2, 2).is_err());
    }

    #[test]
    fn rle_basic_runs() {
        // sig, compressed=1, run(0xAA, 3), run(0xBB, 1) -> 4 pixels for a 4x1 rect.
        let mut stream = Vec::new();
        stream.extend_from_slice(&SIG_LE);
        stream.extend_from_slice(&1u16.to_le_bytes()); // compressed = 1
        stream.extend_from_slice(&[0xAA, 0x03]); // alpha 0xAA, count 3
        stream.extend_from_slice(&[0xBB, 0x01]); // alpha 0xBB, count 1

        let out = decode_alpha_stream(&stream, 4, 1).unwrap();
        assert_eq!(out, [0xAA, 0xAA, 0xAA, 0xBB]);
    }

    #[test]
    fn rle_run_clamped_to_rect() {
        // A run whose count overruns the rect is clamped to the remaining pixels.
        let mut stream = Vec::new();
        stream.extend_from_slice(&SIG_LE);
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&[0x7F, 0x0A]); // count 10, but rect only has 3 pixels

        let out = decode_alpha_stream(&stream, 3, 1).unwrap();
        assert_eq!(out, [0x7F, 0x7F, 0x7F]);
    }

    #[test]
    fn rle_u16_count_escalation() {
        // count byte == 0xFF triggers a u16 count that REPLACES it. Use 300 pixels.
        let width = 300u16;
        let mut stream = Vec::new();
        stream.extend_from_slice(&SIG_LE);
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.push(0xCC); // alpha value
        stream.push(0xFF); // escalate to u16
        stream.extend_from_slice(&300u16.to_le_bytes()); // count = 300

        let out = decode_alpha_stream(&stream, width, 1).unwrap();
        assert_eq!(out.len(), 300);
        assert!(out.iter().all(|&b| b == 0xCC));
    }

    #[test]
    fn rle_u32_count_escalation() {
        // count byte == 0xFF, then u16 == 0xFFFF, escalates to a u32 count.
        // Use a 70000-pixel rect (> 0xFFFF) so the u32 path is exercised.
        let total: u32 = 70_000;
        let mut stream = Vec::new();
        stream.extend_from_slice(&SIG_LE);
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.push(0x42); // alpha value
        stream.push(0xFF); // escalate to u16
        stream.extend_from_slice(&0xFFFFu16.to_le_bytes()); // escalate to u32
        stream.extend_from_slice(&total.to_le_bytes()); // count = 70000

        // 70000 = 700 * 100, a rect whose pixel count exceeds 0xFFFF.
        let out = decode_alpha_stream(&stream, 700, 100).unwrap();
        assert_eq!(out.len(), 70_000);
        assert!(out.iter().all(|&b| b == 0x42));
    }

    #[test]
    fn rle_rejects_zero_count() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&SIG_LE);
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.extend_from_slice(&[0x55, 0x00]); // count 0 -> rejected (would hang FreeRDP)
        assert!(decode_alpha_stream(&stream, 4, 1).is_err());
    }

    #[test]
    fn rle_rejects_truncated_run() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&SIG_LE);
        stream.extend_from_slice(&1u16.to_le_bytes());
        stream.push(0x55); // alpha value but no count byte
        assert!(decode_alpha_stream(&stream, 4, 1).is_err());
    }
}
