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

import { avcDecodeCallback, onAvcDecoded } from './extensions';
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
    private decoder: VideoDecoder | null = null;
    private session: SessionLike | null = null;
    private codec: string | null = null;
    private sawKeyframe = false;
    private timestamp = 0;
    /** decode-time geometry keyed by chunk timestamp (matched on output). */
    private readonly geom = new Map<number, Geom>();
    private canvas: OffscreenCanvas | null = null;
    private ctx: OffscreenCanvasRenderingContext2D | null = null;
    private warnedUnsupported = false;
    /**
     * Decoded frames awaiting present, in FIFO order. We present EVERY frame in order
     * (no dropping) and send a FrameAcknowledge after each present, so the server paces
     * itself to our real display rate (the FreeRDP flow-control model — see the client
     * `build_frame_ack`). With that pacing the queue stays shallow. `MAX_QUEUE` is only
     * a memory backstop for the case where the server floods anyway (e.g. acks not
     * honored): the oldest frame is dropped+closed, and the client's deferred-ack cap
     * eventually acks it so flow control can't deadlock.
     */
    private readonly queue: Array<{ frame: VideoFrame; geom: Geom }> = [];
    private rafScheduled = false;
    /** True while a frame's async readback+present is in flight (single-in-flight). */
    private displaying = false;
    /** Reused RGBA readback buffer to avoid a per-frame allocation. */
    private rgbaBuffer: ArrayBuffer | null = null;
    /** Prefer VideoFrame.copyTo (async, non-blocking); fall back to canvas on error. */
    private useCopyTo = true;

    /** Extensions registered on the SessionBuilder so the run loop can call us. */
    getBuilderExtensions(): Extension[] {
        return [
            avcDecodeCallback((_surfaceId, frameId, x, y, width, height, data) => {
                this.decode(frameId, x, y, width, height, data);
            }),
        ];
    }

    /** Give us the live session (its `invokeExtension` is our RGBA return path). */
    setSession(session: SessionLike): void {
        this.session = session;
    }

    dispose(): void {
        try {
            this.decoder?.close();
        } catch {
            /* already closed */
        }
        this.decoder = null;
        this.geom.clear();
        for (const q of this.queue) {
            q.frame.close();
        }
        this.queue.length = 0;
        this.sawKeyframe = false;
    }

    private decode(frameId: number, x: number, y: number, width: number, height: number, data: Uint8Array): void {
        if (typeof VideoDecoder === 'undefined') {
            if (!this.warnedUnsupported) {
                console.error('[AVC] WebCodecs VideoDecoder unavailable in this browser; AVC frames dropped');
                this.warnedUnsupported = true;
            }
            return;
        }

        // The AVD/MS-RDPEGFX main sub-stream is already Annex B (start-code prefixed);
        // feed it to WebCodecs unchanged, just scanning for the keyframe + SPS.
        const { hasKey, sps } = analyzeAnnexB(data);

        // Configure lazily from the first keyframe — we need its SPS for the codec
        // string, and WebCodecs requires the first decoded chunk to be a keyframe.
        if (this.decoder === null) {
            if (!hasKey || sps === null) {
                return; // wait for the first keyframe
            }
            this.codec = codecStringFromSps(sps);
            const decoder = new VideoDecoder({
                output: (frame) => this.onFrame(frame),
                error: (e) => console.error('[AVC] VideoDecoder error', e),
            });
            try {
                decoder.configure({ codec: this.codec, optimizeForLatency: true });
            } catch (e) {
                console.error('[AVC] configure failed for', this.codec, e);
                return;
            }
            this.decoder = decoder;
            console.info('[AVC] VideoDecoder configured:', this.codec);
        }

        if (!this.sawKeyframe && !hasKey) {
            return; // never feed a delta frame before the first keyframe
        }
        if (hasKey) {
            this.sawKeyframe = true;
        }

        const ts = this.timestamp++;
        this.geom.set(ts, { frameId, x, y, width, height });
        try {
            this.decoder.decode(
                new EncodedVideoChunk({
                    type: hasKey ? 'key' : 'delta',
                    timestamp: ts,
                    data,
                }),
            );
        } catch (e) {
            console.error('[AVC] decode() threw', e);
            this.geom.delete(ts);
        }
    }

    private onFrame(frame: VideoFrame): void {
        const g = this.geom.get(frame.timestamp);
        this.geom.delete(frame.timestamp);
        if (!g) {
            frame.close();
            return;
        }
        // Present every frame in order (FreeRDP model — no dropping). Flow control via
        // frame-ack keeps the queue shallow; the cap is only a memory backstop for a
        // server that floods despite acks (drop the oldest; the client's deferred-ack
        // cap acks it so the flow-control window can't deadlock).
        while (this.queue.length >= MAX_QUEUE) {
            this.queue.shift()?.frame.close();
        }
        this.queue.push({ frame, geom: g });
        this.scheduleDisplay();
    }

    private scheduleDisplay(): void {
        // One present in flight at a time; the newest pending frame is picked up when
        // it finishes. rAF paces us to the compositor and yields to input/paint.
        if (this.rafScheduled || this.displaying) {
            return;
        }
        this.rafScheduled = true;
        requestAnimationFrame(() => {
            this.rafScheduled = false;
            this.displayNext();
        });
    }

    private displayNext(): void {
        if (this.displaying) {
            return;
        }
        const p = this.queue.shift();
        if (!p) {
            return;
        }
        this.displaying = true;
        void this.presentFrame(p.frame, p.geom).finally(() => {
            this.displaying = false;
            if (this.queue.length > 0) {
                this.scheduleDisplay();
            }
        });
    }

    /**
     * Read the frame back to RGBA and hand it to the run loop. The readback is the
     * heavy step (GPU→CPU); we use the async `VideoFrame.copyTo` so it does NOT block
     * the main thread the way `drawImage`+`getImageData` did (that stalled the rAF
     * handler ~50ms/frame). Falls back to the canvas path if `copyTo` is unavailable.
     */
    private async presentFrame(frame: VideoFrame, g: Geom): Promise<void> {
        const w = g.width || frame.displayWidth;
        const h = g.height || frame.displayHeight;
        try {
            let rgba: Uint8Array | null = null;
            if (this.useCopyTo) {
                try {
                    rgba = await this.readbackCopyTo(frame, w, h);
                } catch (e) {
                    console.warn('[AVC] VideoFrame.copyTo failed; falling back to canvas readback', e);
                    this.useCopyTo = false;
                }
            }
            if (!rgba) {
                rgba = this.readbackCanvas(frame, w, h);
            }
            if (rgba && this.session) {
                this.session.invokeExtension(
                    onAvcDecoded({ frameId: g.frameId, x: g.x, y: g.y, width: w, height: h, data: rgba }),
                );
            }
        } finally {
            frame.close();
        }
    }

    /** Async readback via VideoFrame.copyTo into a reused RGBA buffer. */
    private async readbackCopyTo(frame: VideoFrame, w: number, h: number): Promise<Uint8Array> {
        const size = w * h * 4;
        if (this.rgbaBuffer === null || this.rgbaBuffer.byteLength < size) {
            this.rgbaBuffer = new ArrayBuffer(size);
        }
        const buf = new Uint8Array(this.rgbaBuffer, 0, size);
        await frame.copyTo(buf, {
            format: 'RGBA',
            rect: { x: 0, y: 0, width: w, height: h },
        });
        // invokeExtension copies synchronously, so reusing `buf` next frame is safe.
        return buf;
    }

    /** Synchronous fallback readback via OffscreenCanvas (crops the top-left w×h). */
    private readbackCanvas(frame: VideoFrame, w: number, h: number): Uint8Array | null {
        if (this.canvas === null || this.canvas.width < w || this.canvas.height < h) {
            const cw = Math.max(w, this.canvas?.width ?? 0);
            const ch = Math.max(h, this.canvas?.height ?? 0);
            this.canvas = new OffscreenCanvas(cw, ch);
            this.ctx = this.canvas.getContext('2d', {
                willReadFrequently: true,
            }) as OffscreenCanvasRenderingContext2D | null;
        }
        const ctx = this.ctx;
        if (!ctx) {
            return null;
        }
        ctx.drawImage(frame, 0, 0);
        const img = ctx.getImageData(0, 0, w, h);
        return new Uint8Array(img.data.buffer, img.data.byteOffset, img.data.byteLength);
    }
}
