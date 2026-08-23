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
        // Tolerant of a JSON-quoted value; see the same note on STATS_ENABLED in SurfaceRenderer.
        param = new URLSearchParams(globalThis.location?.search ?? '').get('ironwebgl')?.replace(/^"|"$/g, '') ?? null;
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

/**
 * Hardware H.264 decode on the WebGL path. DEFAULT ON; `?ironhwdec=0` forces software back.
 *
 * Worth roughly half the renderer's CPU (see the configure call for the measurements). Per-surface
 * fallback to software is automatic if the hardware decoder refuses to configure or errors at
 * runtime, so this flag is an override, not the safety net.
 *
 * Read once at module scope, like the other flags: a decoder is configured per surface at its first
 * keyframe, and re-reading per surface would let a mid-session URL change split one session in two.
 */
const HW_DECODE = (() => {
    try {
        const p = new URLSearchParams(globalThis.location?.search ?? '').get('ironhwdec')?.replace(/^"|"$/g, '');
        return p !== '0' && p !== 'false';
    } catch {
        return true;
    }
})();

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
    /**
     * Latched once this surface has fallen back to software decode, so a machine whose hardware
     * decoder fails does not retry hardware on every keyframe and thrash.
     */
    softwareFallback: boolean;
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
            console.info(
                '[AVC] WebGL present path: main-thread decode + ack-on-decode (default; ?ironwebgl=0 to opt out)',
            );
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
                    this.decodeMain(
                        surfaceId,
                        frameId,
                        originX,
                        originY,
                        new Uint32Array(regions),
                        new Uint8Array(data),
                    );
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
            sd = {
                decoder: null,
                codec: null,
                sawKeyframe: false,
                softwareFallback: false,
                timestamp: 0,
                geom: new Map(),
            };
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
                error: (e) => {
                    console.error('[AVC] VideoDecoder error (surface', surfaceId, ')', e);
                    // A decoder that errors is closed and cannot be reused. If it was the hardware
                    // one, drop this surface back to software and rebuild on the next keyframe --
                    // otherwise a machine with a broken hardware decoder gets a dead surface, which
                    // would be a far worse regression than the CPU we are trying to save.
                    this.fallBackToSoftware(surfaceId);
                },
            });
            try {
                // HARDWARE decode on this path, and it is worth ~half the renderer's CPU.
                //
                // This used to be `prefer-software`, inherited from the WORKER path -- where it is
                // correct, because CPU-backed frames make that path's per-rect `copyTo` a cheap
                // memcpy instead of a GPU->CPU readback stall. Here the requirement is the opposite:
                // we hand the frame straight to `texImage2D`, so we want it already on the GPU.
                //
                // Measured on one session, same content, only this line changed:
                //   tab CPU          164 / 129  ->  82 / 45   (Chrome task manager)
                //   texImage2D avg   1.6-2.3ms  ->  0.3-0.6ms (no CPU-side I420->RGBA convert)
                //   `up` per second  38-210ms/s ->  4-28ms/s
                // The 210ms/s stall regime, where uploads blocked on a full driver queue and rAF
                // fell to 1Hz, did not reappear at all.
                //
                // Software decode also ran H.264 on a CPU thread pool, which is why nine rounds of
                // main-thread instrumentation never found this: the cost was never on the thread we
                // were measuring. `?ironhwdec=0` forces the old behaviour back.
                const accel: HardwareAcceleration =
                    HW_DECODE && !sd.softwareFallback ? 'prefer-hardware' : 'prefer-software';
                console.info(`[AVC] decoder configure codec=${sd.codec} hardwareAcceleration=${accel}`);
                decoder.configure({
                    codec: sd.codec,
                    optimizeForLatency: true,
                    hardwareAcceleration: accel,
                });
            } catch (e) {
                console.error('[AVC] configure failed for', sd.codec, e);
                // Only hardware is optional. If SOFTWARE configure failed there is nothing left to
                // try, so do not latch and retry forever -- just drop the frame as before.
                if (!sd.softwareFallback && HW_DECODE) {
                    console.warn('[AVC] hardware decode unavailable — falling back to software');
                    sd.softwareFallback = true;
                }
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

    /**
     * Drop one surface back to software decode after its hardware decoder failed.
     *
     * The errored decoder is already closed, so the surface is torn down to the pre-keyframe state
     * and rebuilt on the next keyframe -- deltas in between are useless without their reference
     * frames. `geom` is cleared too: those entries are keyed by chunk timestamp for a decoder that
     * will never produce output, and would otherwise leak for the life of the session.
     */
    private fallBackToSoftware(surfaceId: number): void {
        const sd = this.surfaces.get(surfaceId);
        if (!sd || sd.softwareFallback) return;
        console.warn('[AVC] hardware decoder failed on surface', surfaceId, '— retrying in software');
        sd.softwareFallback = true;
        sd.decoder = null;
        sd.sawKeyframe = false;
        sd.geom.clear();
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
