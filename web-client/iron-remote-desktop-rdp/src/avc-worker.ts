/**
 * AVC (H.264) decode worker — MS-RDPEGFX AVC420/AVC444 main sub-streams (DEFAULT / CPU path).
 *
 * WHY A WORKER: on the main thread the WebCodecs decode + the VideoFrame→RGBA readback compete
 * with rendering, input, and the WASM run loop. Under a sustained video stream that contention
 * makes the pipeline fall behind, the eGFX FrameAcknowledge backpressure kicks in, the host
 * throttles the AVC frame-rate and eventually stalls waiting for acks, and the session drops.
 * Moving decode + readback OFF the main thread (this worker) keeps the ack loop fed so the host
 * never throttles.
 *
 * WHY SOFTWARE DECODE: `hardwareAcceleration: 'prefer-software'` decodes to CPU-backed
 * VideoFrames, so `copyTo` is a cheap CPU copy instead of a GPU→CPU readback stall.
 *
 * NOTE: the WebGL present path (`?ironwebgl=1`) does NOT use this worker — it decodes on the main
 * thread and uploads the VideoFrame straight to a WebGL surface (see AvcDecoder.ts). This worker is
 * the default/production path that composites RGBA into the WASM surface.
 *
 * Protocol with the main thread (see AvcDecoder.ts):
 *   main → worker: { type:'decode', surfaceId, frameId, regions:Uint32Array, data:Uint8Array }
 *   worker → main: { type:'ack',   surfaceId, frameId }                         (at DECODE, paces the host)
 *                  { type:'frame', surfaceId, frameId, rects:[{x,y,w,h}], rgbas:ArrayBuffer[] } (after readback)
 *   worker → main: { type:'unsupported' } once, if WebCodecs is absent.
 */

import { analyzeAnnexB, codecStringFromSps, unflatten, type RegionRect } from './avc-annexb';

interface SurfaceState {
    decoder: VideoDecoder | null;
    codec: string | null;
    sawKeyframe: boolean;
    /** Monotonic per-surface chunk timestamp; also the `geom` map key. */
    timestamp: number;
    /** decode-time metadata keyed by chunk timestamp (matched on decoder output). */
    geom: Map<number, { frameId: number; rects: RegionRect[] }>;
}

const surfaces = new Map<number, SurfaceState>();
let warnedUnsupported = false;

function getSurface(surfaceId: number): SurfaceState {
    let sd = surfaces.get(surfaceId);
    if (sd === undefined) {
        sd = { decoder: null, codec: null, sawKeyframe: false, timestamp: 0, geom: new Map() };
        surfaces.set(surfaceId, sd);
    }
    return sd;
}

async function onFrame(surfaceId: number, frame: VideoFrame): Promise<void> {
    const sd = surfaces.get(surfaceId);
    const meta = sd?.geom.get(frame.timestamp);
    sd?.geom.delete(frame.timestamp);
    if (!sd || !meta) {
        frame.close();
        return;
    }

    // Ack at DECODE (before readback) so the host is paced to real decode throughput — the fix
    // for the FrameAcknowledge backpressure that was stalling the stream.
    (self as unknown as Worker).postMessage({ type: 'ack', surfaceId, frameId: meta.frameId });

    // Read back each region rect to RGBA (CPU copy on a software frame) and hand the buffers to
    // the main thread to composite into the WASM surface. The region's surface coords are also its
    // source coords in the surface-aligned coded picture.
    const rects: RegionRect[] = [];
    const rgbas: ArrayBuffer[] = [];
    try {
        for (const r of meta.rects) {
            const size = r.w * r.h * 4;
            const buf = new ArrayBuffer(size);
            const view = new Uint8Array(buf);
            try {
                await frame.copyTo(view, { format: 'RGBA', rect: { x: r.x, y: r.y, width: r.w, height: r.h } });
            } catch {
                continue; // region out of the coded picture / bad align — skip it
            }
            rects.push(r);
            rgbas.push(buf);
        }
    } finally {
        frame.close();
    }
    if (rgbas.length > 0) {
        (self as unknown as Worker).postMessage({ type: 'frame', surfaceId, frameId: meta.frameId, rects, rgbas }, rgbas);
    }
}

function decode(surfaceId: number, frameId: number, regions: Uint32Array, data: Uint8Array): void {
    if (typeof VideoDecoder === 'undefined') {
        if (!warnedUnsupported) {
            warnedUnsupported = true;
            (self as unknown as Worker).postMessage({ type: 'unsupported' });
        }
        return;
    }
    const sd = getSurface(surfaceId);
    const { hasKey, sps } = analyzeAnnexB(data);

    if (sd.decoder === null) {
        if (!hasKey || sps === null) return; // wait for this surface's first keyframe
        sd.codec = codecStringFromSps(sps);
        const decoder = new VideoDecoder({
            output: (frame) => void onFrame(surfaceId, frame),
            error: (e) => console.error('[avc-worker] VideoDecoder error (surface', surfaceId, ')', e),
        });
        try {
            // prefer-software => CPU-backed frames => cheap copyTo (no GPU→CPU readback);
            // optimizeForLatency => small decode queue, matching the shallow frame-ack window.
            decoder.configure({ codec: sd.codec, optimizeForLatency: true, hardwareAcceleration: 'prefer-software' });
        } catch (e) {
            console.error('[avc-worker] configure failed for', sd.codec, e);
            return;
        }
        sd.decoder = decoder;
    }

    if (!sd.sawKeyframe && !hasKey) return; // never a delta before the first keyframe
    if (hasKey) sd.sawKeyframe = true;

    const ts = sd.timestamp++;
    sd.geom.set(ts, { frameId, rects: unflatten(regions) });
    try {
        sd.decoder.decode(new EncodedVideoChunk({ type: hasKey ? 'key' : 'delta', timestamp: ts, data }));
    } catch (e) {
        console.error('[avc-worker] decode() threw (surface', surfaceId, ')', e);
        sd.geom.delete(ts);
    }
}

self.onmessage = (e: MessageEvent) => {
    const msg = e.data;
    if (msg && msg.type === 'decode') {
        decode(msg.surfaceId, msg.frameId, msg.regions as Uint32Array, msg.data as Uint8Array);
    } else if (msg && msg.type === 'dispose') {
        for (const sd of surfaces.values()) {
            try {
                sd.decoder?.close();
            } catch {
                /* already closed */
            }
        }
        surfaces.clear();
    }
};
