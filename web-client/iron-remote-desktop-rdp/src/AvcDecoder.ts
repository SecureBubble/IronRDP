/**
 * Browser WebCodecs H.264 decoder for MS-RDPEGFX AVC (AVC420 / AVC444) main
 * sub-streams.
 *
 * MVP scope: decodes ONLY the main (stream1) YUV420 picture, which alone yields a
 * full-color 4:2:0 image — one `VideoDecoder`, no auxiliary stream, no YUV444
 * recombination. The Rust run loop hands us the compressed main picture via the
 * `avc_decode_callback` extension; we decode it and hand the resulting RGBA back
 * through `on_avc_decoded` (invoked on the live Session), which re-enters the run
 * loop as an ordinary output region and is blitted to the canvas.
 *
 * Wiring mirrors `RdpFileTransferProvider`: the host adds `getBuilderExtension()` to
 * the session config before connect and calls `setSession()` once connected.
 */

// Per-surface present telemetry (fps / queue / decode / present-path). Off by
// default — flip to true to diagnose AVC present/pacing; it logs ~1 line/sec per
// surface to the console.
const AVC_STATS_DEBUG = false;

import { avcDecodeCallback, onAvcDecoded, onAvcAck, avcWatermarkCallback } from './extensions';
import type { Extension } from '../../../crates/ironrdp-web/pkg/ironrdp_web';

/** The subset of the connected Session we call back into. */
interface SessionLike {
    invokeExtension(ext: Extension): unknown;
}

interface Geom {
    /** eGFX frame this picture belongs to; echoed back on present so the client can
     *  send its deferred FrameAcknowledge (flow control). */
    frameId: number;
    x: number;
    y: number;
    width: number;
    height: number;
}

interface FrameInfo {
    /** True if the frame carries an IDR slice (NAL type 5) — a keyframe. */
    hasKey: boolean;
    /** The first bytes of the SPS NAL (incl. NAL header), enough for the codec string. */
    sps: Uint8Array | null;
}

/**
 * Decode state for ONE eGFX surface. MS-RDPEGFX carries an independent H.264
 * bitstream per surface_id — its own SPS/PPS, its own IDR and reference-frame
 * chain. A multi-monitor session has one surface per monitor, so each needs its
 * own `VideoDecoder`: multiplexing two independent streams through a single
 * decoder pollutes the reference-picture buffer and renders the second monitor as
 * white / stale / ghosted frames. Present geometry stays per-frame (see `Geom`),
 * so the shared present queue can still blit each surface at its output origin.
 */
interface SurfaceDecoder {
    decoder: VideoDecoder | null;
    codec: string | null;
    sawKeyframe: boolean;
    /** Monotonic per-surface chunk timestamp; also the `geom` map key. */
    timestamp: number;
    /** decode-time geometry keyed by chunk timestamp (matched on output). */
    geom: Map<number, Geom>;

    // --- Per-surface PRESENT pipeline -------------------------------------------------
    // Each surface presents through its OWN FIFO + single-in-flight slot + rAF cadence, so
    // two monitors present CONCURRENTLY instead of serializing through one shared pipeline
    // (which capped total present throughput at the display refresh rate across BOTH
    // surfaces and was the multi-monitor stall's throughput root). Single-monitor is
    // unchanged: one surface == one pipeline == the old behaviour exactly.
    /** Decoded frames for THIS surface awaiting present, FIFO. */
    queue: Array<{ frame: VideoFrame; geom: Geom }>;
    /** A rAF is scheduled for this surface's present pump. */
    rafScheduled: boolean;
    /** A present is in flight for this surface (single-in-flight per surface). */
    displaying: boolean;
    /** GPU direct-draw enabled for THIS surface. Flipped off per-surface on a drawImage
     *  failure so one monitor's failure never forces the other onto CPU readback. */
    useGpu: boolean;
    /** Prefer VideoFrame.copyTo on the CPU fallback; per-surface so two concurrent CPU
     *  readbacks never clash on a shared scratch buffer. */
    useCopyTo: boolean;
    /** Reused RGBA readback buffer (per-surface, CPU fallback only). */
    rgbaBuffer: ArrayBuffer | null;
    /** Per-surface OffscreenCanvas scratch for the canvas readback fallback. */
    canvas: OffscreenCanvas | null;
    ctx: OffscreenCanvasRenderingContext2D | null;

    // --- Per-surface instrumentation (live 2-monitor repro) ---------------------------
    decodeCount: number;
    presentCount: number;
    droppedCount: number;
    /** Wall-clock ms accumulated in presentFrame since the last stats log. */
    presentMs: number;
    lastStatsTs: number;
    lastPresentCount: number;
    warnedDrop: boolean;
}

function toHex2(n: number): string {
    return n.toString(16).padStart(2, '0');
}

/**
 * Derive the WebCodecs `codec` string from an SPS NAL. Layout: byte 0 = NAL header,
 * byte 1 = profile_idc, byte 2 = constraint-set flags, byte 3 = level_idc. The
 * WebCodecs H.264 codec string is `avc1.<profile><constraints><level>` in hex.
 */
function codecStringFromSps(sps: Uint8Array): string {
    const profile = sps[1] ?? 0x42;
    const constraints = sps[2] ?? 0x00;
    const level = sps[3] ?? 0x1f;
    return `avc1.${toHex2(profile)}${toHex2(constraints)}${toHex2(level)}`;
}

/**
 * Scan an Annex B H.264 bitstream (start-code-prefixed NAL units, as MS-RDPEGFX / AVD
 * sends them) to report whether it carries a keyframe (IDR slice, NAL type 5) and to
 * capture the start of the SPS (NAL type 7) for the WebCodecs codec string. The
 * bitstream is fed to the decoder unchanged — no conversion needed.
 */
function analyzeAnnexB(data: Uint8Array): FrameInfo {
    let hasKey = false;
    let sps: Uint8Array | null = null;
    const n = data.length;
    let i = 0;

    while (i + 3 < n) {
        // Match a 3-byte (00 00 01) or 4-byte (00 00 00 01) start code.
        if (data[i] === 0 && data[i + 1] === 0) {
            let scLen = 0;
            if (data[i + 2] === 1) {
                scLen = 3;
            } else if (data[i + 2] === 0 && data[i + 3] === 1) {
                scLen = 4;
            }
            if (scLen > 0) {
                const nalStart = i + scLen;
                if (nalStart < n) {
                    const nalType = data[nalStart] & 0x1f;
                    if (nalType === 5) {
                        hasKey = true; // IDR slice
                    }
                    if (nalType === 7 && sps === null) {
                        // Only the first 4 bytes are needed (NAL header + profile/
                        // constraints/level) to build the codec string.
                        sps = data.subarray(nalStart, Math.min(nalStart + 4, n));
                    }
                }
                i = nalStart;
                continue;
            }
        }
        i++;
    }

    return { hasKey, sps };
}

/** Memory backstop: max decoded frames held awaiting present. Small — with frame-ack
 *  pacing the queue stays near-empty; this only bounds VideoFrame memory if the server
 *  floods despite acks. */
const MAX_QUEUE = 8;

export class AvcDecoder {
    private session: SessionLike | null = null;
    /**
     * Per-surface decode state, keyed by eGFX surface_id. One `VideoDecoder` per
     * surface (see {@link SurfaceDecoder}) so a multi-monitor session's independent
     * per-monitor H.264 streams do not corrupt one another.
     */
    private readonly surfaces = new Map<number, SurfaceDecoder>();
    private warnedUnsupported = false;
    /** The shared render canvas 2D context. When set, AVC frames are drawn straight
     *  onto it (GPU→GPU, no readback) — the "Enhanced graphics" fast path. The present
     *  FIFO / in-flight slot / GPU-fallback flag are now PER-SURFACE (see
     *  {@link SurfaceDecoder}); only this shared canvas context and the watermark pattern
     *  are session-global. */
    private renderCtx: CanvasRenderingContext2D | null = null;
    /** Session watermark as a repeating pattern (built from the forwarded tile), drawn
     *  over each GPU-composited frame with a background-opposing blend. */
    private wmPattern: CanvasPattern | null = null;
    private wmOpacity = 0;
    /** Passive notification for an external multi-monitor presenter: fired after a GPU
     *  direct-draw present blits an AVC frame onto the shared render canvas (that path
     *  never re-enters the Rust run loop, so it can't use `canvas_updated_callback`). The
     *  CPU-readback fallback instead re-enters via `onAvcDecoded`, so the Rust side fires
     *  the notification for it. `null` when no presenter is registered. */
    private canvasUpdated: ((x: number, y: number, width: number, height: number) => void) | null = null;

    /** Extensions registered on the SessionBuilder so the run loop can call us. */
    getBuilderExtensions(): Extension[] {
        return [
            avcDecodeCallback((surfaceId, frameId, x, y, width, height, data) => {
                this.decode(surfaceId, frameId, x, y, width, height, data);
            }),
            avcWatermarkCallback((rgba, width, height, cellW, cellH, offX, offY, opacity) => {
                this.setWatermark(rgba, width, height, cellW, cellH, offX, offY, opacity);
            }),
        ];
    }

    /** Give us the live session (its `invokeExtension` is our RGBA return path). */
    setSession(session: SessionLike): void {
        this.session = session;
    }

    /** Register (or clear with `null`) the passive render-canvas update notification used
     *  by an external multi-monitor presenter. Fired from the GPU direct-draw present path
     *  with the updated rect in source-canvas pixel coords. Passive — it never affects
     *  decode, present, or frame-ack. */
    setCanvasUpdatedCallback(cb: ((x: number, y: number, width: number, height: number) => void) | null): void {
        this.canvasUpdated = cb;
    }

    /** Give us the render canvas so AVC frames can be drawn directly on the GPU
     *  (no readback). getContext('2d') returns the same context WASM composites into,
     *  so the two paths share one surface. If absent, we fall back to CPU readback. */
    setCanvas(canvas: HTMLCanvasElement): void {
        try {
            this.renderCtx = canvas.getContext('2d') as CanvasRenderingContext2D | null;
        } catch {
            this.renderCtx = null;
        }
        if (!this.renderCtx) {
            console.warn('[AVC] no 2D context on render canvas; using CPU readback path');
        }
    }

    /** Build the watermark into a repeating canvas pattern: a cellW×cellH transparent
     *  cell with the QR tile placed at (offX,offY), tiled from the canvas origin so it
     *  aligns to the desktop grid — matching the CPU blend's placement. */
    setWatermark(
        rgba: Uint8Array,
        width: number,
        height: number,
        cellW: number,
        cellH: number,
        offX: number,
        offY: number,
        opacity: number,
    ): void {
        this.wmPattern = null;
        this.wmOpacity = 0;
        if (!this.renderCtx || cellW <= 0 || cellH <= 0 || width <= 0 || height <= 0 || opacity <= 0) {
            return;
        }
        try {
            const size = width * height * 4;
            const tile = new ImageData(new Uint8ClampedArray(rgba.subarray(0, size)), width, height);
            const cell = new OffscreenCanvas(cellW, cellH);
            const cctx = cell.getContext('2d');
            if (!cctx) {
                return;
            }
            cctx.putImageData(tile, offX, offY);
            this.wmPattern = this.renderCtx.createPattern(cell, 'repeat');
            this.wmOpacity = opacity;
        } catch (e) {
            console.warn('[AVC] watermark pattern setup failed', e);
            this.wmPattern = null;
            this.wmOpacity = 0;
        }
    }

    /** Overdraw the watermark onto a just-presented AVC region using a background-
     *  opposing blend ('difference'): the mark stays legible on light and dark content
     *  alike, done on the GPU with no readback. Scaled by the requested opacity. */
    private drawWatermark(x: number, y: number, w: number, h: number): void {
        const ctx = this.renderCtx;
        if (!ctx || !this.wmPattern || this.wmOpacity <= 0) {
            return;
        }
        ctx.save();
        ctx.globalCompositeOperation = 'difference';
        ctx.globalAlpha = this.wmOpacity / 255;
        ctx.fillStyle = this.wmPattern;
        ctx.fillRect(x, y, w, h);
        ctx.restore();
    }

    dispose(): void {
        for (const sd of this.surfaces.values()) {
            try {
                sd.decoder?.close();
            } catch {
                /* already closed */
            }
            sd.geom.clear();
            for (const q of sd.queue) {
                q.frame.close();
            }
            sd.queue.length = 0;
        }
        this.surfaces.clear();
    }

    /** Get (or lazily create) the decode + present state for an eGFX surface. */
    private getSurface(surfaceId: number): SurfaceDecoder {
        let sd = this.surfaces.get(surfaceId);
        if (sd === undefined) {
            sd = {
                decoder: null,
                codec: null,
                sawKeyframe: false,
                timestamp: 0,
                geom: new Map<number, Geom>(),
                queue: [],
                rafScheduled: false,
                displaying: false,
                useGpu: true,
                useCopyTo: true,
                rgbaBuffer: null,
                canvas: null,
                ctx: null,
                decodeCount: 0,
                presentCount: 0,
                droppedCount: 0,
                presentMs: 0,
                lastStatsTs: 0,
                lastPresentCount: 0,
                warnedDrop: false,
            };
            this.surfaces.set(surfaceId, sd);
        }
        return sd;
    }

    private decode(
        surfaceId: number,
        frameId: number,
        x: number,
        y: number,
        width: number,
        height: number,
        data: Uint8Array,
    ): void {
        if (typeof VideoDecoder === 'undefined') {
            if (!this.warnedUnsupported) {
                console.error('[AVC] WebCodecs VideoDecoder unavailable in this browser; AVC frames dropped');
                this.warnedUnsupported = true;
            }
            return;
        }

        // Each surface_id is an INDEPENDENT H.264 stream (its own SPS/PPS + reference
        // chain), so it gets its own decoder and keyframe/timestamp state.
        const sd = this.getSurface(surfaceId);

        // The AVD/MS-RDPEGFX main sub-stream is already Annex B (start-code prefixed);
        // feed it to WebCodecs unchanged, just scanning for the keyframe + SPS.
        const { hasKey, sps } = analyzeAnnexB(data);

        // Configure this surface's decoder lazily from its first keyframe — we need its
        // SPS for the codec string, and WebCodecs requires the first decoded chunk to be
        // a keyframe.
        if (sd.decoder === null) {
            if (!hasKey || sps === null) {
                return; // wait for this surface's first keyframe
            }
            sd.codec = codecStringFromSps(sps);
            const decoder = new VideoDecoder({
                output: (frame) => this.onFrame(surfaceId, frame),
                error: (e) => console.error('[AVC] VideoDecoder error (surface', surfaceId, ')', e),
            });
            try {
                decoder.configure({ codec: sd.codec, optimizeForLatency: true });
            } catch (e) {
                console.error('[AVC] configure failed for', sd.codec, '(surface', surfaceId, ')', e);
                return;
            }
            sd.decoder = decoder;
            console.info('[AVC] VideoDecoder configured:', sd.codec, '(surface', surfaceId, ')');
        }

        if (!sd.sawKeyframe && !hasKey) {
            return; // never feed a delta frame before this surface's first keyframe
        }
        if (hasKey) {
            sd.sawKeyframe = true;
        }

        const ts = sd.timestamp++;
        sd.geom.set(ts, { frameId, x, y, width, height });
        try {
            sd.decoder.decode(
                new EncodedVideoChunk({
                    type: hasKey ? 'key' : 'delta',
                    timestamp: ts,
                    data,
                }),
            );
            sd.decodeCount++;
        } catch (e) {
            console.error('[AVC] decode() threw (surface', surfaceId, ')', e);
            sd.geom.delete(ts);
        }
    }

    private onFrame(surfaceId: number, frame: VideoFrame): void {
        const sd = this.surfaces.get(surfaceId);
        const g = sd?.geom.get(frame.timestamp);
        sd?.geom.delete(frame.timestamp);
        if (!sd || !g) {
            frame.close();
            return;
        }

        // FLOW CONTROL — ack at DECODE, not at present.
        //
        // WebCodecs decode is the client's real throughput bottleneck (sub-ms, on the GPU);
        // presentation is rAF-gated (at most one present per display refresh, per surface).
        // Firing the deferred eGFX FrameAcknowledge HERE — the instant the decoder produces a
        // frame — paces the server to our true decode rate, exactly like the synchronous
        // non-AVC codecs (ClearCodec/RFX/Planar) ack at EndFrame right after their Rust decode.
        //
        // Previously the ack fired from `presentFrame` (see below), i.e. from a
        // requestAnimationFrame callback. That gated server delivery on a present round-trip
        // (server → decode → next rAF → ack → server), capping delivery at roughly
        // window / (rAF_quantum + RTT). One monitor tolerated it (a single surface's rAF hides
        // the round-trip); two monitors sharing ONE eGFX flow-control window did not — the
        // window split across two independently-phased rAF pipelines starved both to a bursty
        // crawl. Acking at decode removes the rAF quantum and the present latency from the
        // flow-control loop entirely.
        //
        // This does NOT let the server outrun us: a new ack only issues when the decoder
        // actually PRODUCES a frame, so the server is still paced to real decode work (it can
        // never flood an async decoder — the failure the deferred model was created to
        // prevent). Present latency stays bounded because the per-surface FIFO below caps the
        // queue and drops stale frames if decode ever outruns present (e.g. a hidden/throttled
        // tab): memory and display lag are bounded to MAX_QUEUE frames regardless of delivery
        // rate. In normal operation the server's encode rate is the cap and is <= the display
        // refresh, so the queue stays shallow and nothing is dropped — single-monitor AVC is
        // unchanged except that its ack (and therefore the next frame) arrives up to one rAF
        // sooner, i.e. latency goes down, never up.
        this.session?.invokeExtension(onAvcAck({ frameId: g.frameId }));

        // Present every frame in order (FreeRDP model) through THIS surface's own queue. With
        // decode-time acking the queue is now the sole present-latency bound: the cap coalesces
        // by dropping the oldest when present can't keep up (the Rust high-water ack has already
        // advanced past those frame_ids, so dropping here never deadlocks flow control). A
        // sustained overflow is the smoking gun that present can't keep up with decode for this
        // surface.
        while (sd.queue.length >= MAX_QUEUE) {
            sd.queue.shift()?.frame.close();
            sd.droppedCount++;
            if (!sd.warnedDrop) {
                console.warn(
                    '[AVC] present FIFO overflow (surface',
                    surfaceId,
                    '); dropping oldest — present cannot keep up with decode',
                );
                sd.warnedDrop = true;
            }
        }
        sd.queue.push({ frame, geom: g });
        this.scheduleDisplay(sd, surfaceId);
    }

    /** Schedule this surface's present pump on the next rAF (one in-flight per surface). */
    private scheduleDisplay(sd: SurfaceDecoder, surfaceId: number): void {
        if (sd.rafScheduled || sd.displaying) {
            return;
        }
        sd.rafScheduled = true;
        requestAnimationFrame(() => {
            sd.rafScheduled = false;
            this.displayNext(sd, surfaceId);
        });
    }

    private displayNext(sd: SurfaceDecoder, surfaceId: number): void {
        if (sd.displaying) {
            return;
        }
        const p = sd.queue.shift();
        if (!p) {
            return;
        }
        sd.displaying = true;
        void this.presentFrame(sd, surfaceId, p.frame, p.geom).finally(() => {
            sd.displaying = false;
            if (sd.queue.length > 0) {
                this.scheduleDisplay(sd, surfaceId);
            }
        });
    }

    /**
     * Present a decoded frame for one surface. Fast path: draw the VideoFrame straight onto
     * the shared render canvas on the GPU (no GPU→CPU readback). Fallback: read back to RGBA
     * (async `copyTo`, else canvas) and hand the pixels to the run loop to blit. GPU is used
     * when a render context is available; a failed `drawImage` disables it FOR THIS SURFACE
     * ONLY (so one monitor's failure never drags the other onto CPU readback) and falls
     * through to CPU readback for this same frame — the frame is still presented, never dropped.
     *
     * NOTE: the eGFX FrameAcknowledge is NOT sent from here anymore — it is fired at DECODE
     * (see `onFrame`) so server delivery is paced by decode throughput, not by this rAF
     * present. The GPU path therefore only draws; the CPU fallback still emits `onAvcDecoded`
     * purely to deliver PIXELS for the run loop to blit (its redundant ack is a Rust no-op —
     * the frame_id was already acked and retired at decode).
     */
    private async presentFrame(sd: SurfaceDecoder, surfaceId: number, frame: VideoFrame, g: Geom): Promise<void> {
        const w = g.width || frame.displayWidth;
        const h = g.height || frame.displayHeight;
        const t0 = performance.now();

        try {
            // GPU direct-draw fast path.
            if (sd.useGpu && this.renderCtx) {
                try {
                    // Crop the coded top-left w×h (macroblock padding) to the dest rect.
                    // Draw only — the frame-ack already fired at decode (see `onFrame`).
                    this.renderCtx.drawImage(frame, 0, 0, w, h, g.x, g.y, w, h);
                    this.drawWatermark(g.x, g.y, w, h);
                    // Passive: tell an external presenter which area of the shared canvas
                    // just changed (the CPU fallback notifies via onAvcDecoded → Rust).
                    this.canvasUpdated?.(g.x, g.y, w, h);
                    sd.presentCount++;
                    return;
                } catch (e) {
                    console.warn(
                        '[AVC] GPU drawImage failed (surface',
                        surfaceId,
                        '); switching THIS surface to CPU readback',
                        e,
                    );
                    sd.useGpu = false;
                    // Fall through to CPU readback for this same frame (no drop).
                }
            }

            // CPU readback fallback (per-surface scratch → no cross-surface races).
            let rgba: Uint8Array | null = null;
            if (sd.useCopyTo) {
                try {
                    rgba = await this.readbackCopyTo(sd, frame, w, h);
                } catch (e) {
                    console.warn('[AVC] VideoFrame.copyTo failed (surface', surfaceId, '); canvas readback', e);
                    sd.useCopyTo = false;
                }
            }
            if (!rgba) {
                rgba = this.readbackCanvas(sd, frame, w, h);
            }
            if (rgba && this.session) {
                this.session.invokeExtension(
                    onAvcDecoded({ frameId: g.frameId, x: g.x, y: g.y, width: w, height: h, data: rgba }),
                );
                sd.presentCount++;
            }
        } finally {
            frame.close();
            sd.presentMs += performance.now() - t0;
            this.maybeLogStats(sd, surfaceId);
        }
    }

    /** Per-surface live telemetry, throttled to ~1 Hz. On a 2-monitor drag repro these
     *  lines PROVE which mechanism binds: compare each surface's present fps against its
     *  decoded fps and `decodeQueueSize` (decoder throughput / keyframe bursts = candidate
     *  d), watch `queue`/`dropped` climb (shared-FIFO throughput = candidate b, now
     *  per-surface), and watch `avg` present ms balloon when `path` flips to `cpu`
     *  (CPU-readback cost = candidate c). The Rust `pending_depth` / straggler logs show
     *  the reorder-buffer head-of-line effect (candidate a). */
    private maybeLogStats(sd: SurfaceDecoder, surfaceId: number): void {
        if (!AVC_STATS_DEBUG) return;
        const now = performance.now();
        if (sd.lastStatsTs === 0) {
            sd.lastStatsTs = now;
            sd.lastPresentCount = sd.presentCount;
            return;
        }
        const dt = now - sd.lastStatsTs;
        if (dt < 1000) {
            return;
        }
        const presented = sd.presentCount - sd.lastPresentCount;
        const fps = (presented * 1000) / dt;
        const avgMs = presented > 0 ? sd.presentMs / presented : 0;
        console.info(
            `[AVC] surface ${surfaceId}: present ${fps.toFixed(1)}fps avg ${avgMs.toFixed(2)}ms ` +
                `queue=${sd.queue.length} decodeQ=${sd.decoder?.decodeQueueSize ?? -1} ` +
                `decoded=${sd.decodeCount} presented=${sd.presentCount} dropped=${sd.droppedCount} ` +
                `path=${sd.useGpu ? 'gpu' : 'cpu'}`,
        );
        sd.lastStatsTs = now;
        sd.lastPresentCount = sd.presentCount;
        sd.presentMs = 0;
    }

    /** Async readback via VideoFrame.copyTo into this surface's reused RGBA buffer. */
    private async readbackCopyTo(sd: SurfaceDecoder, frame: VideoFrame, w: number, h: number): Promise<Uint8Array> {
        const size = w * h * 4;
        if (sd.rgbaBuffer === null || sd.rgbaBuffer.byteLength < size) {
            sd.rgbaBuffer = new ArrayBuffer(size);
        }
        const buf = new Uint8Array(sd.rgbaBuffer, 0, size);
        await frame.copyTo(buf, {
            format: 'RGBA',
            rect: { x: 0, y: 0, width: w, height: h },
        });
        // invokeExtension copies synchronously, so reusing `buf` next frame is safe.
        return buf;
    }

    /** Synchronous fallback readback via this surface's OffscreenCanvas (crops top-left w×h). */
    private readbackCanvas(sd: SurfaceDecoder, frame: VideoFrame, w: number, h: number): Uint8Array | null {
        if (sd.canvas === null || sd.canvas.width < w || sd.canvas.height < h) {
            const cw = Math.max(w, sd.canvas?.width ?? 0);
            const ch = Math.max(h, sd.canvas?.height ?? 0);
            sd.canvas = new OffscreenCanvas(cw, ch);
            sd.ctx = sd.canvas.getContext('2d', {
                willReadFrequently: true,
            }) as OffscreenCanvasRenderingContext2D | null;
        }
        const ctx = sd.ctx;
        if (!ctx) {
            return null;
        }
        ctx.drawImage(frame, 0, 0);
        const img = ctx.getImageData(0, 0, w, h);
        return new Uint8Array(img.data.buffer, img.data.byteOffset, img.data.byteLength);
    }
}
