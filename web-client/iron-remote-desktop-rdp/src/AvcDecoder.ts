/**
 * Browser AVC (H.264) decoder for MS-RDPEGFX AVC420/AVC444 main sub-streams.
 *
 * TWO PATHS:
 *  - DEFAULT (production): decode in a Web Worker ({@link ./avc-worker}), read back RGBA, and
 *    composite each region into the WASM surface via `on_avc_decoded` (CPU 2D `put_image_data`).
 *    The worker keeps the FrameAcknowledge loop off the CPU-composite critical path so the host
 *    never throttles.
 *  - WEBGL (`?ironwebgl=1`): mirror the Microsoft AVD web client (see memory
 *    `msft-webclient-render-architecture`) — decode on the MAIN thread with software WebCodecs,
 *    ACK-ON-DECODE (fire the FrameAcknowledge the instant the frame decodes, BEFORE present, so a
 *    present/vsync stall can never delay the ack), then `texImage2D` the VideoFrame straight onto a
 *    WebGL surface-0 canvas. No worker, no readback, no cross-thread transfer. The GPU composite is
 *    cheap enough that main-thread decode doesn't starve the ack loop (MSFT proves this at 2484×1268).
 *
 * Wiring mirrors `RdpFileTransferProvider`: the host adds `getBuilderExtensions()` to the session
 * config before connect, calls `setSession()` and `setCanvas()` once connected.
 */

import {
    avcDecodeCallback,
    onAvcDecoded,
    onAvcAck,
    avcWatermarkCallback,
    surfacePresentCallback,
    surfaceLayoutCallback,
    surfaceCopyCallback,
} from './extensions';
import type { Extension } from '../../../crates/ironrdp-web/pkg/ironrdp_web';
import { SurfaceRenderer } from './SurfaceRenderer';
import { analyzeAnnexB, codecStringFromSps, unflatten, type RegionRect } from './avc-annexb';
// Inline worker: bundled into this module as a Blob so the single vendor bundle stays self-contained.
import AvcWorker from './avc-worker?worker&inline';

/**
 * Can this browser actually run the GPU path? Both capabilities are hard requirements, and neither
 * degrades gracefully mid-session: without `VideoDecoder` no AVC frame is ever decoded, and without
 * WebGL2 there is no surface to present to — either way the GPU path would show nothing at all,
 * while the worker/2D path still renders. So the choice is made HERE, before connect, because it
 * decides which extensions get registered and therefore which present path the Rust side takes.
 */
function webGlPathSupported(): boolean {
    if (typeof VideoDecoder === 'undefined') return false;
    try {
        return document.createElement('canvas').getContext('webgl2') !== null;
    } catch {
        return false;
    }
}

/**
 * Route AVC through the main-thread GPU WebGL path (vs the worker/2D path).
 *
 * DEFAULT ON, with an automatic fallback: `?ironwebgl=0` forces the worker/2D path, `?ironwebgl=1`
 * forces the GPU path even if capability detection is unsure, and with no parameter we use the GPU
 * path whenever the browser supports it.
 */
function webGlPresentEnabled(): boolean {
    let param: string | null = null;
    try {
        param = new URLSearchParams(globalThis.location?.search ?? '').get('ironwebgl');
    } catch {
        /* no location (worker/SSR) — fall through to capability detection */
    }
    if (param === '0' || param === 'false') return false;
    if (param === '1') return true;
    const ok = webGlPathSupported();
    if (!ok) {
        console.warn('[AVC] WebGL present path unavailable (needs WebCodecs + WebGL2) — using the worker/2D path');
    }
    return ok;
}

/** The subset of the connected Session we call back into. */
interface SessionLike {
    invokeExtension(ext: Extension): unknown;
}

interface WorkerRect {
    x: number;
    y: number;
    w: number;
    h: number;
}

type WorkerMessage =
    | { type: 'ack'; surfaceId: number; frameId: number }
    | { type: 'frame'; surfaceId: number; frameId: number; rects: WorkerRect[]; rgbas: ArrayBuffer[] }
    | { type: 'unsupported' };

/** Per-surface main-thread decode state (WebGL path only). */
interface MainSurface {
    decoder: VideoDecoder | null;
    codec: string | null;
    sawKeyframe: boolean;
    /** Monotonic per-surface chunk timestamp; also the `geom` map key. */
    timestamp: number;
    /** decode-time metadata keyed by chunk timestamp (matched on decoder output). */
    geom: Map<number, { frameId: number; rects: RegionRect[]; originX: number; originY: number }>;
}

export class AvcDecoder {
    private session: SessionLike | null = null;
    private readonly useWebGl = webGlPresentEnabled();
    /** Worker path (default). Null in the WebGL path, where decode runs on the main thread. */
    private readonly worker: Worker | null;
    private warnedUnsupported = false;
    /** Passive multi-monitor presenter notification (source-canvas pixel coords). Optional. */
    private canvasUpdated: ((x: number, y: number, width: number, height: number) => void) | null = null;
    /** surface_id -> MapSurfaceToOutput origin, for translating surface coords into output space. */
    private origins = new Map<number, { x: number; y: number }>();

    // --- WebGL (main-thread) path state ---
    private renderer: SurfaceRenderer | null = null;
    private readonly surfaces = new Map<number, MainSurface>();

    constructor() {
        if (this.useWebGl) {
            this.worker = null;
            console.info('[AVC] WebGL present path: main-thread decode + ack-on-decode (default; ?ironwebgl=0 to opt out)');
        } else {
            this.worker = new AvcWorker();
            this.worker.onmessage = (e: MessageEvent<WorkerMessage>) => this.onWorkerMessage(e.data);
        }
    }

    /** Extensions registered on the SessionBuilder so the run loop can call us. */
    getBuilderExtensions(): Extension[] {
        return [
            avcDecodeCallback((surfaceId, frameId, originX, originY, regions, data) => {
                // originX/originY are this surface's MapSurfaceToOutput origin. They are (0,0) for a
                // single monitor, which is why discarding them went unnoticed — but under
                // multi-monitor EACH monitor is its own eGFX surface, so surface 1's regions start
                // at (0,0) again and must be shifted into output space or they paint over monitor 0
                // and monitor 1 never updates.
                //
                // WHERE the shift belongs differs by path, so it is NOT applied here:
                //  - WebGL: composites straight into the output-space texture -> needs the shift.
                //  - Worker: hands regions back to the WASM compositor, which applies each surface's
                //    origin itself -> must stay in surface coords, or it would be shifted twice.
                this.origins.set(surfaceId, { x: originX, y: originY });
                // Copy out of WASM memory first — the underlying buffer is reused by the run loop.
                if (this.useWebGl) {
                    this.decodeMain(surfaceId, frameId, originX, originY, new Uint32Array(regions), new Uint8Array(data));
                } else {
                    const dataCopy = new Uint8Array(data);
                    const regionsCopy = new Uint32Array(regions);
                    this.worker!.postMessage(
                        { type: 'decode', surfaceId, frameId, regions: regionsCopy, data: dataCopy },
                        [dataCopy.buffer, regionsCopy.buffer],
                    );
                }
            }),
            avcWatermarkCallback(() => {
                // The watermark is re-blended by the WASM surface flush in the CPU composite path;
                // the old GPU-overlay watermark went away with the GPU direct-draw path. No-op.
            }),
            // WebGL path only. Two halves of one contract with the Rust compositor:
            //  - `surfacePresentCallback` delivers ONLY pixels a non-AVC codec just decoded (the
            //    exact rects, never a re-extract of a window or dirty box). It must be that narrow
            //    because the AVC video is decoded here in JS and lives only in the GPU texture —
            //    the WASM SurfaceBuf has a video-shaped hole where it should be, so any re-sent
            //    region overlapping the video would upload that hole as opaque black over it.
            //  - `surfaceLayoutCallback` delivers the app-window rects instead, and WE do the
            //    clipping on the GPU at present. That is why the compositor no longer needs to
            //    clear a dragged window's vacated area (the ex-source of drag ghosts/black blocks).
            ...(this.useWebGl
                ? [
                      surfacePresentCallback((x, y, w, h, rgba) => {
                          this.renderer?.uploadChromeRegion(x, y, w, h, rgba);
                      }),
                      surfaceLayoutCallback((mode, surfaceW, surfaceH, rects) => {
                          this.renderer?.setLayout(mode, surfaceW, surfaceH, rects);
                      }),
                      surfaceCopyCallback((srcX, srcY, w, h, points) => {
                          this.renderer?.copyRegion(srcX, srcY, w, h, points);
                      }),
                  ]
                : []),
        ];
    }

    /** Give us the live session (its `invokeExtension` is our RGBA/ack return path). */
    setSession(session: SessionLike): void {
        this.session = session;
    }

    /** Receives the render canvas. No-op in the worker/2D path (the run loop composites AVC into the
     *  WASM surface). In the WebGL path it's where we build the surface-0 GPU sink. */
    setCanvas(canvas: HTMLCanvasElement): void {
        if (this.useWebGl && !this.renderer) {
            this.renderer = new SurfaceRenderer(canvas);
        }
    }

    /** Retained for API compatibility (multi-monitor presenter). */
    setCanvasUpdatedCallback(cb: ((x: number, y: number, width: number, height: number) => void) | null): void {
        this.canvasUpdated = cb;
    }

    /** Retained for API compatibility. Watermark is handled by the WASM flush now. */
    setWatermark(): void {}

    dispose(): void {
        if (this.worker) {
            try {
                this.worker.postMessage({ type: 'dispose' });
                this.worker.terminate();
            } catch {
                /* already gone */
            }
        }
        for (const s of this.surfaces.values()) {
            try {
                s.decoder?.close();
            } catch {
                /* already closed */
            }
        }
        this.surfaces.clear();
        this.renderer?.dispose();
        this.renderer = null;
        this.session = null;
    }

    // ============================================================================================
    // WebGL path: main-thread decode → ACK-ON-DECODE → GPU present.
    // ============================================================================================

    private getMainSurface(surfaceId: number): MainSurface {
        let sd = this.surfaces.get(surfaceId);
        if (sd === undefined) {
            sd = { decoder: null, codec: null, sawKeyframe: false, timestamp: 0, geom: new Map() };
            this.surfaces.set(surfaceId, sd);
        }
        return sd;
    }

    private decodeMain(
        surfaceId: number,
        frameId: number,
        originX: number,
        originY: number,
        regions: Uint32Array,
        data: Uint8Array,
    ): void {
        if (typeof VideoDecoder === 'undefined') {
            if (!this.warnedUnsupported) {
                this.warnedUnsupported = true;
                console.error('[AVC] WebCodecs VideoDecoder unavailable in this browser; AVC frames dropped');
            }
            return;
        }
        const sd = this.getMainSurface(surfaceId);
        const { hasKey, sps } = analyzeAnnexB(data);

        if (sd.decoder === null) {
            if (!hasKey || sps === null) return; // wait for this surface's first keyframe
            sd.codec = codecStringFromSps(sps);
            const decoder = new VideoDecoder({
                output: (frame) => this.onFrameMain(surfaceId, frame),
                error: (e) => console.error('[AVC] VideoDecoder error (surface', surfaceId, ')', e),
            });
            try {
                // Software decode (MSFT does the same) → CPU-backed frames; texImage2D uploads them
                // to the GPU. optimizeForLatency keeps the decode queue shallow for the ack window.
                decoder.configure({ codec: sd.codec, optimizeForLatency: true, hardwareAcceleration: 'prefer-software' });
            } catch (e) {
                console.error('[AVC] configure failed for', sd.codec, e);
                return;
            }
            sd.decoder = decoder;
        }

        if (!sd.sawKeyframe && !hasKey) return; // never a delta before the first keyframe
        if (hasKey) sd.sawKeyframe = true;

        const ts = sd.timestamp++;
        // Rects stay in SURFACE coords: they double as the SOURCE rect inside the decoded video
        // frame, so shifting them here would break the sampling. The origin rides along and is
        // applied only where an OUTPUT-space coordinate is actually needed (the GPU destination
        // rect, and the canvas-update notification).
        sd.geom.set(ts, { frameId, rects: unflatten(regions), originX, originY });
        try {
            sd.decoder.decode(new EncodedVideoChunk({ type: hasKey ? 'key' : 'delta', timestamp: ts, data }));
        } catch (e) {
            console.error('[AVC] decode() threw (surface', surfaceId, ')', e);
            sd.geom.delete(ts);
        }
    }

    private onFrameMain(surfaceId: number, frame: VideoFrame): void {
        const sd = this.surfaces.get(surfaceId);
        const meta = sd?.geom.get(frame.timestamp);
        sd?.geom.delete(frame.timestamp);
        if (!sd || !meta) {
            frame.close();
            return;
        }

        // ACK-ON-DECODE: fire the FrameAcknowledge the instant the frame decodes, BEFORE present, so
        // a present/vsync stall can never delay the ack (MSFT: "frame ack sent without surface").
        this.session?.invokeExtension(onAvcAck({ frameId: meta.frameId }));

        // GPU present: hand the VideoFrame to the unified surface renderer (drop-to-latest; it
        // uploads + closes on the next rAF). No WASM surface write — AVC is a hole in the chrome.
        if (this.renderer) {
            this.renderer.submitAvcFrame(frame, meta.rects, meta.originX, meta.originY);
            for (const r of meta.rects) {
                this.canvasUpdated?.(r.x + meta.originX, r.y + meta.originY, r.w, r.h);
            }
        } else {
            frame.close();
        }
    }

    // ============================================================================================
    // Worker path (default / production): decode + RGBA readback in the worker, composite in WASM.
    // ============================================================================================

    private onWorkerMessage(msg: WorkerMessage): void {
        if (!msg) return;
        switch (msg.type) {
            case 'ack':
                // Deferred FrameAcknowledge, fired at DECODE-completion (in the worker) — paces the
                // host to real decode throughput so the AVC backpressure never throttles/stalls.
                this.session?.invokeExtension(onAvcAck({ frameId: msg.frameId }));
                break;
            case 'frame':
                // Composite each decoded region into the WASM surface at its surface coords.
                for (let i = 0; i < msg.rects.length; i++) {
                    const r = msg.rects[i];
                    const data = new Uint8Array(msg.rgbas[i]);
                    this.session?.invokeExtension(
                        onAvcDecoded({
                            surfaceId: msg.surfaceId,
                            frameId: msg.frameId,
                            x: r.x,
                            y: r.y,
                            width: r.w,
                            height: r.h,
                            data,
                        }),
                    );
                    // The region itself stays in surface coords (the WASM compositor positions it),
                    // but an external presenter is told about OUTPUT-space rects — otherwise a
                    // second monitor's update marks the wrong monitor dirty.
                    const o = this.origins.get(msg.surfaceId);
                    this.canvasUpdated?.(r.x + (o?.x ?? 0), r.y + (o?.y ?? 0), r.w, r.h);
                }
                break;
            case 'unsupported':
                if (!this.warnedUnsupported) {
                    this.warnedUnsupported = true;
                    console.error('[AVC] WebCodecs VideoDecoder unavailable in this browser; AVC frames dropped');
                }
                break;
        }
    }
}
