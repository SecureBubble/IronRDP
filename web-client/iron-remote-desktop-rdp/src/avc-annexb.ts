/**
 * Shared H.264 Annex-B helpers for the AVC decode paths (the default Web Worker path in
 * `avc-worker.ts` and the main-thread WebGL path in `AvcDecoder.ts`).
 */

export interface RegionRect {
    x: number;
    y: number;
    w: number;
    h: number;
}

function toHex2(n: number): string {
    return n.toString(16).padStart(2, '0');
}

/** WebCodecs `codec` string from an SPS NAL: byte1=profile, byte2=constraints, byte3=level. */
export function codecStringFromSps(sps: Uint8Array): string {
    const profile = sps[1] ?? 0x42;
    const constraints = sps[2] ?? 0x00;
    const level = sps[3] ?? 0x1f;
    return `avc1.${toHex2(profile)}${toHex2(constraints)}${toHex2(level)}`;
}

/** Scan an Annex-B bitstream for a keyframe (IDR, NAL 5) and the SPS start (NAL 7). */
export function analyzeAnnexB(data: Uint8Array): { hasKey: boolean; sps: Uint8Array | null } {
    let hasKey = false;
    let sps: Uint8Array | null = null;
    const n = data.length;
    let i = 0;
    while (i + 3 < n) {
        if (data[i] === 0 && data[i + 1] === 0) {
            let scLen = 0;
            if (data[i + 2] === 1) scLen = 3;
            else if (data[i + 2] === 0 && data[i + 3] === 1) scLen = 4;
            if (scLen > 0) {
                const nalStart = i + scLen;
                if (nalStart < n) {
                    const nalType = data[nalStart] & 0x1f;
                    if (nalType === 5) hasKey = true;
                    if (nalType === 7 && sps === null) sps = data.subarray(nalStart, Math.min(nalStart + 4, n));
                }
                i = nalStart;
                continue;
            }
        }
        i++;
    }
    return { hasKey, sps };
}

/** Unflatten a `[x,y,w,h, x,y,w,h, ...]` Uint32Array into region rects, dropping empty ones. */
export function unflatten(regions: Uint32Array): RegionRect[] {
    const out: RegionRect[] = [];
    for (let i = 0; i + 3 < regions.length; i += 4) {
        const w = regions[i + 2];
        const h = regions[i + 3];
        if (w > 0 && h > 0) out.push({ x: regions[i], y: regions[i + 1], w, h });
    }
    return out;
}
