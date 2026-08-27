/**
 * Unified WebGL2 surface-0 renderer — the GPU-composite path for `?ironwebgl=1`.
 *
 * MODEL (mirrors the Microsoft AVD web client, see memory `msft-webclient-render-architecture`):
 * ONE full-desktop surface-0 WebGL canvas that BOTH the non-AVC chrome and the AVC video composite
 * into, presented as one frame. No second content canvas → no seam/ghost artifacts.
 *
 *  - CHROME (ClearCodec / Progressive / NSCodec, composited into the WASM SurfaceBuf): the run loop
 *    forwards each decoded region `(x,y,w,h,rgba)` here (via `surface_present_callback`); we
 *    `texSubImage2D` it into the persistent surface-0 texture — dirty-region upload, near-zero cost
 *    while video plays and the chrome is static. The AVC region is a HOLE the chrome never fills
 *    (the WebGL AVC draw fills it), so no readback is reintroduced.
 *  - AVC (H.264, main-thread WebCodecs-decoded in AvcDecoder): `submitAvcFrame` hands us the latest
 *    VideoFrame; we upload it to a second texture and draw it on top at its region rects.
 *  - PRESENT: a coalescing, self-suspending rAF loop. An arriving chrome region or AVC frame (or a
 *    layout change) schedules an rAF; on the rAF we apply the pending texture uploads, draw
 *    chrome + AVC, and present the surface CLIPPED to the Path A app-window rects (`setLayout`) —
 *    one quad per window, on a canvas cleared to transparent, so the RAIL surround and a dragged
 *    window's trail are never drawn. If nothing is pending we do NOT reschedule — no idle 60 Hz
 *    spin. VideoFrames are drop-to-latest (a newer frame closes the older undrawn one) and closed
 *    right after the upload, so the decoder is never back-pressured.
 *
 * The visible canvas is a sibling overlaid on the (now-blank) 2D `#renderer`; the watermark, if any,
 * stays a separate DOM overlay and is not our concern.
 */

interface Rect {
    x: number;
    y: number;
    w: number;
    h: number;
}

/** `?ironstats=1` re-enables the rolling present-stats line (off by default: console flood). */
/**
 * STICKY, and it has to be: connecting navigates to `/webclient` and the query string does not
 * survive it, which silently disarmed an entire A/B capture -- the session ran, AVC frames flowed,
 * and not one stat line was written. The webapp latches the flag into sessionStorage on first
 * sighting; this reads the same latch so the vendor and the app arm together.
 */
/**
 * Per-restore tile-cache tracing (`?irontc=1`). SEPARATE from `?ironstats=1` on purpose: a RAIL
 * session produced 53,093 restores, and one console line each would drown the perf meter that
 * `ironstats` exists for -- the instrument would distort the thing it measures. Aggregate cache
 * counters stay under `ironstats`; only the per-restore `dst <- src` line needs this.
 */
const TILECACHE_TRACE = (() => {
    try {
        const p = new URLSearchParams(globalThis.location?.search ?? '').get('irontc')?.replace(/^"|"$/g, '');
        return p === '1';
    } catch {
        return false;
    }
})();

const STATS_ENABLED = (() => {
    try {
        // Tolerant of both `ironstats=1` and `ironstats="1"` -- the app router JSON-serializes
        // string search values, and the quoted form silently fails a strict `=== '1'` test.
        const param = new URLSearchParams(globalThis.location?.search ?? '').get('ironstats')?.replace(/^"|"$/g, '');
        if (param === '0') return false;
        return param === '1' || sessionStorage.getItem('ironstats') === '1';
    } catch {
        return false;
    }
})();

/** Present modes from the Rust compositor (`WEBGL_LAYOUT_*` in graphics.rs — keep in sync). */
/** Which edge of which RAIL window the pointer is over, and the cursor for it. */
export interface RailEdgeHit {
    cursor: string;
    /** 'l' | 'r' | 't' | 'b' | 'tl' | 'tr' | 'bl' | 'br' */
    edge: string;
    /** RAIL window id — what a resulting `WindowMove` is addressed to. */
    id: number;
    /** The window's current rect in SURFACE coords, the basis for the new rect. */
    rect: { x: number; y: number; w: number; h: number };
}

/** Grab margin around a RAIL window edge, in surface pixels. Matches the feel of a native frame. */
const RESIZE_EDGE_PX = 6;

const LAYOUT_BLANK = 0;
const LAYOUT_FULLSCREEN = 1;
const LAYOUT_CLIP = 2;

const VERT = `#version 300 es
in vec2 aPos;              // unit quad, 0..1
uniform vec4 uDst;         // dest rect in NDC: (x, y, w, h), y already flipped
uniform vec4 uSrc;         // source rect in texture space 0..1 (top-left origin)
out vec2 vTex;
void main() {
    vec2 p = uDst.xy + aPos * uDst.zw;
    gl_Position = vec4(p, 0.0, 1.0);
    vTex = uSrc.xy + aPos * uSrc.zw;
}`;

const FRAG = `#version 300 es
precision mediump float;
in vec2 vTex;
uniform sampler2D uTex;
out vec4 outColor;
void main() {
    outColor = vec4(texture(uTex, vTex).rgb, 1.0);
}`;

/** Rolling per-second present stats (delivered fps / present cost) for the acid test. */
class PresentStats {
    private frames = 0;
    private sumMs = 0;
    private maxMs = 0;
    private wmSum = 0;
    private wmMax = 0;
    private windowStart = 0;
    record(ms: number, wmMs: number, now: number): void {
        if (this.windowStart === 0) this.windowStart = now;
        this.frames++;
        this.sumMs += ms;
        this.wmSum += wmMs;
        this.wmMax = Math.max(this.wmMax, wmMs);
        this.maxMs = Math.max(this.maxMs, ms);
        const elapsed = now - this.windowStart;
        if (elapsed >= 2000) {
            const fps = (this.frames / elapsed) * 1000;
            // Off by default: this fired every 2s for the whole session and was a top console
            // flooder. `?ironstats=1` brings it back when measuring present cost.
            if (STATS_ENABLED) {
                console.info(
                    `[SurfaceRenderer] fps=${fps.toFixed(1)} present(avg/max)=${(this.sumMs / this.frames).toFixed(2)}/${this.maxMs.toFixed(2)}ms ` +
                        `wm=${((this.wmSum / elapsed) * 1000).toFixed(1)}ms/s (avg=${(this.wmSum / this.frames).toFixed(2)} max=${this.wmMax.toFixed(2)})`,
                );
            }
            this.frames = 0;
            this.sumMs = 0;
            this.maxMs = 0;
            this.wmSum = 0;
            this.wmMax = 0;
            this.windowStart = now;
        }
    }
}

/**
 * Rolling per-second AVC upload stats — the measurement for the "full-frame texImage2D" question.
 *
 * The AVC frame path never enters Rust, so the Rust-side counters (`upload_n` and friends) are
 * structurally blind to it: they watch the Rust compositor, and AVC bypasses it. Chrome can't
 * unwind it either (it lands in `(program)`). So it gets timed here, in JS, directly.
 *
 * The number that matters is `up=ms/s` — cost per WALL second, comparable against the ~965 ms/s
 * total main-thread budget. Per-call averages are not: they say nothing about how often it runs.
 *
 * `cov` is the fraction of the decoded frame the region rects actually touch. The host sends
 * full-frame destRects with sparse real content, so this is the headroom a sub-rect upload would
 * reclaim — cov=0.10 means we upload 10x what changed.
 *
 * Cost of the instrument itself: 3 `performance.now()` calls per AVC frame, ~90/s at 30fps. The
 * last round's instrumentation added ~1,000 WASM<->JS crossings/s and BECAME the cost being
 * measured; this cannot, but re-check that assumption before trusting a surprising result.
 */
class AvcUploadStats {
    private frames = 0;
    private upSum = 0;
    private upMax = 0;
    private drawSum = 0;
    private covSum = 0;
    private bboxSum = 0;
    private rectSum = 0;
    private emptyFrames = 0;
    private windowStart = 0;
    private dims = '';

    record(
        upMs: number,
        drawMs: number,
        coverage: number,
        bboxCoverage: number,
        rectCount: number,
        w: number,
        h: number,
        now: number,
    ): void {
        if (this.windowStart === 0) this.windowStart = now;
        this.frames++;
        this.upSum += upMs;
        this.upMax = Math.max(this.upMax, upMs);
        this.drawSum += drawMs;
        this.covSum += coverage;
        this.bboxSum += bboxCoverage;
        this.rectSum += rectCount;
        if (rectCount === 0) this.emptyFrames++;
        this.dims = `${w}x${h}`;
        const elapsed = now - this.windowStart;
        if (elapsed >= 2000) {
            if (STATS_ENABLED) {
                const perSec = (ms: number) => (ms / elapsed) * 1000;
                console.info(
                    `[AVC upload] ${this.dims} fps=${((this.frames / elapsed) * 1000).toFixed(1)} ` +
                        `up=${perSec(this.upSum).toFixed(1)}ms/s (avg=${(this.upSum / this.frames).toFixed(2)} ` +
                        `max=${this.upMax.toFixed(2)}) draw=${perSec(this.drawSum).toFixed(1)}ms/s ` +
                        `cov=${(this.covSum / this.frames).toFixed(3)} ` +
                        `bbox=${(this.bboxSum / this.frames).toFixed(3)} ` +
                        `rects=${(this.rectSum / this.frames).toFixed(1)} ` +
                        `empty=${this.emptyFrames}/${this.frames}`,
                );
            }
            this.frames = 0;
            this.upSum = 0;
            this.upMax = 0;
            this.drawSum = 0;
            this.covSum = 0;
            this.bboxSum = 0;
            this.rectSum = 0;
            this.emptyFrames = 0;
            this.windowStart = now;
        }
    }
}

export class SurfaceRenderer {
    private readonly base: HTMLCanvasElement;
    private overlay: HTMLCanvasElement | null = null;
    private gl: WebGL2RenderingContext | null = null;
    private chromeTex: WebGLTexture | null = null; // the single surface-0 texture (chrome + AVC)
    private copyTex: WebGLTexture | null = null; // scratch for SurfaceToSurface (src and dst are the same texture)
    private avcTex: WebGLTexture | null = null; // scratch: the latest decoded frame, rendered INTO chromeTex
    private fbo: WebGLFramebuffer | null = null; // render target = chromeTex (AVC render-to-texture)
    private uDst: WebGLUniformLocation | null = null;
    private uSrc: WebGLUniformLocation | null = null;
    private failed = false;

    /** Surface (desktop) size the textures are allocated to. */
    private surfW = 0;
    private surfH = 0;
    /** One-shot geometry sanity log. */
    private geomLogged = false;
    /** Authoritative surface size from the Rust compositor (eGFX CreateSurface). 0 = not yet known. */
    private wantW = 0;
    private wantH = 0;

    // AVC state: latest undrawn frame (drop-to-latest), and the last-uploaded frame's geometry.
    private avcRects: Rect[] = [];
    private avcW = 0;
    private avcH = 0;
    private hasAvc = false;

    // Path A presentation layout from the Rust compositor (see `setLayout`). Until the first
    // layout arrives we present the full surface, which is right for a plain desktop session
    // (no Window List orders → the callback never fires).
    private layoutMode: number = LAYOUT_FULLSCREEN;
    private windowRects: Rect[] = [];
    /** Parallel to `windowRects`: the RAIL window id for each rect. */
    private windowIds: number[] = [];

    private rafScheduled = false;
    private pending = false;
    private readonly stats = new PresentStats();
    /** eGFX tile cache, GPU-side. See `cacheRegion` for why the CPU cache cannot serve this path. */
    private readonly tileCache = new Map<number, { tex: WebGLTexture; w: number; h: number }>();
    // Tile-cache diagnostics. Stale tiles look the same whether the slot was never stored (a miss)
    // or was stored with the wrong pixels; these separate the two.
    private tcStores = 0;
    private tcRestores = 0;
    private tcMisses = 0;
    private readonly tcMissedSlots = new Set<number>();
    private tcReportAt = 0;
    /** slot -> "sx,sy WxH" it was captured from. Diagnostics only (?ironstats=1). */
    private readonly tcOrigin = new Map<number, string>();

    // ---- session watermark (RDPGFX_CMDID_WATERMARK), drawn on its own canvas above the present
    // canvas. See `setWatermark` for why it is a sibling layer and not blended into the pixels.
    private wmCanvas: HTMLCanvasElement | null = null;
    private wm: {
        rgba: Uint8Array;
        width: number;
        height: number;
        cellW: number;
        cellH: number;
        offX: number;
        offY: number;
        opacity: number;
    } | null = null;
    private wmPattern: CanvasPattern | null = null;
    private wmDrawnW = 0;
    private wmDrawnH = 0;
    private wmDrawnClip = '';
    /** A watermark is mandated but unavailable — present nothing. See `setWatermark`. */
    private wmBlocked = false;
    private wmBlockedPainted = false;
    private readonly upStats = new AvcUploadStats();

    constructor(baseCanvas: HTMLCanvasElement) {
        this.base = baseCanvas;
    }

    // ------------------------------------------------------------------ inputs

    /** Upload a decoded NON-AVC region into the persistent surface-0 texture (dirty-region). */
    uploadChromeRegion(x: number, y: number, w: number, h: number, rgba: Uint8Array): void {
        if (w <= 0 || h <= 0 || !this.ensure()) return;
        this.syncSize();
        const gl = this.gl!;
        if (x + w > this.surfW || y + h > this.surfH) return; // out of surface — skip defensively
        gl.bindTexture(gl.TEXTURE_2D, this.chromeTex);
        gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
        gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
        // texSubImage2D copies immediately, so the WASM-memory view need not outlive this call.
        gl.texSubImage2D(gl.TEXTURE_2D, 0, x, y, w, h, gl.RGBA, gl.UNSIGNED_BYTE, rgba);
        this.pending = true;
        this.scheduleFrame();
    }

    /**
     * Apply one decoded AVC frame's regions to the surface texture — IMMEDIATELY, never deferred.
     *
     * This used to hold the frame and let the next rAF draw it, closing any previous undrawn frame
     * ("drop-to-latest"). That is right for a video presenter, where each frame is a COMPLETE
     * picture and only the newest matters. It is wrong here: this is an ACCUMULATING dirty-region
     * surface, and each frame carries UNIQUE INCREMENTAL regions. Dropping a frame discarded its
     * region rects permanently — and the host never re-sends them, because it assumes an
     * accumulating client already has them. The dropped update's pixels stayed stale forever: old
     * text still visible beneath new (the duplication), and black wherever the lost region covered
     * area nothing had painted. Drops spike exactly when frames outpace rAF — i.e. while dragging
     * or scrolling, which is when the artifact appeared. The 2D/worker path composites every
     * frame's regions into the WASM SurfaceBuf and drops nothing, which is why it was never
     * affected.
     *
     * So: upload + draw here, synchronously. Only the final present to the visible canvas stays
     * rAF-coalesced (that one IS safe to coalesce — it re-presents the whole retained texture).
     */
    submitAvcFrame(frame: VideoFrame, rects: Rect[], originX = 0, originY = 0): void {
        if (!this.ensure()) {
            frame.close();
            return;
        }
        this.syncSize();
        const gl = this.gl!;

        // FULL-FRAME UPLOAD, measured. The decoder is configured `prefer-software` (see AvcDecoder),
        // so `frame` is CPU-backed and this call color-converts I420->RGBA and pushes the WHOLE
        // frame across, on the main thread, every frame -- no matter that the region rects below
        // often touch <10% of it. `up=ms/s` in the [AVC upload] line is the cost of that.
        const t0 = STATS_ENABLED ? performance.now() : 0;
        try {
            gl.bindTexture(gl.TEXTURE_2D, this.avcTex);
            gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
            gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
            gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame);
            this.avcW = frame.displayWidth || frame.codedWidth;
            this.avcH = frame.displayHeight || frame.codedHeight;
        } catch (e) {
            console.error('[SurfaceRenderer] texImage2D(VideoFrame) failed:', e);
            frame.close();
            return;
        }
        // GPU work is deferred, so this timer captures the CPU-side convert+stage, which is the
        // part that competes with everything else on this thread. It does NOT capture GPU time.
        const t1 = STATS_ENABLED ? performance.now() : 0;

        // Free the decoder buffer straight away — a pinned VideoFrame stalls the decoder.
        frame.close();

        // Draw this frame's regions into the retained surface texture. Same Y mapping as the rest
        // of the accumulation path: FBO row 0 == surface row 0 (no flip); the flip happens only at
        // present. Source coords normalize by the DECODED frame size (which is macroblock-padded,
        // e.g. 1312 for a 1308 surface); destination coords normalize by the surface size.
        //
        // TWO COORDINATE SPACES, and the rect means something different in each:
        //  - SOURCE: where the pixels sit inside THIS surface's decoded frame -> raw rect.
        //  - DESTINATION: where they belong in the shared output texture -> rect + this surface's
        //    MapSurfaceToOutput origin.
        // They coincide only when the origin is (0,0), i.e. single monitor. Under multi-monitor
        // each screen is its own eGFX surface whose regions restart at (0,0), so without the
        // origin every surface draws over monitor 0 and the others never update.
        if (this.avcW > 0 && this.avcH > 0 && rects.length > 0) {
            gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
            gl.viewport(0, 0, this.surfW, this.surfH);
            const W = this.surfW;
            const H = this.surfH;
            for (const r of rects) {
                if (r.w <= 0 || r.h <= 0) continue;
                const dx = r.x + originX;
                const dy = r.y + originY;
                gl.uniform4f(this.uDst, (dx / W) * 2 - 1, (dy / H) * 2 - 1, (r.w / W) * 2, (r.h / H) * 2);
                gl.uniform4f(this.uSrc, r.x / this.avcW, r.y / this.avcH, r.w / this.avcW, r.h / this.avcH);
                gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
            }
            gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        }

        if (STATS_ENABLED) {
            const t2 = performance.now();
            // `cov` sizes a PER-RECT upload; `bbox` sizes a SINGLE bounding-box upload. They diverge
            // when the rects are scattered, and that gap is the whole decision: one texSubImage2D of
            // the bbox is safe, whereas N per-rect calls risk Chrome re-converting the CPU-backed
            // frame N times -- which would be slower than the full upload we have now.
            let covered = 0;
            let n = 0;
            let x0 = Infinity;
            let y0 = Infinity;
            let x1 = 0;
            let y1 = 0;
            for (const r of rects) {
                if (r.w <= 0 || r.h <= 0) continue;
                covered += r.w * r.h;
                n++;
                x0 = Math.min(x0, r.x);
                y0 = Math.min(y0, r.y);
                x1 = Math.max(x1, r.x + r.w);
                y1 = Math.max(y1, r.y + r.h);
            }
            const area = this.avcW * this.avcH;
            const bbox = n > 0 ? (x1 - x0) * (y1 - y0) : 0;
            this.upStats.record(
                t1 - t0,
                t2 - t1,
                area > 0 ? covered / area : 0,
                area > 0 ? bbox / area : 0,
                n,
                this.avcW,
                this.avcH,
                t2,
            );
        }

        this.avcRects = rects;
        this.hasAvc = true;
        this.pending = true;
        this.scheduleFrame();
    }

    /**
     * eGFX `SURFACE_TO_SURFACE`: move the `w`x`h` block at (`sx`,`sy`) to each destination point,
     * entirely on the GPU.
     *
     * The host uses this to RELOCATE a window instead of re-encoding it — which is why a dragged
     * window is the case that breaks. It cannot be done from the WASM surface buffer on this path:
     * that buffer has a video-shaped hole where AVC painted, so copying from it moves a HOLE (which
     * presents as opaque black) and leaves the original pixels untouched (so the content shows at
     * BOTH the old and new positions, and stays that way once the drag stops).
     *
     * Source and destination are the same texture, so the block goes via a scratch texture:
     * `copyTexImage2D` pulls it out of the FBO (whose colour attachment IS the surface texture),
     * then one quad per destination point draws it back in. Y mapping matches the AVC
     * render-to-texture path (FBO row 0 == surface row 0), not the flipped present mapping.
     */
    copyRegion(sx: number, sy: number, w: number, h: number, points: Int32Array): void {
        if (w <= 0 || h <= 0 || !this.ensure()) return;
        this.syncSize();
        const gl = this.gl!;
        if (this.copyTex === null) this.copyTex = this.makeTexture(gl);

        gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
        gl.bindTexture(gl.TEXTURE_2D, this.copyTex);
        gl.copyTexImage2D(gl.TEXTURE_2D, 0, gl.RGBA, sx, sy, w, h, 0);

        gl.viewport(0, 0, this.surfW, this.surfH);
        const W = this.surfW;
        const H = this.surfH;
        for (let i = 0; i + 1 < points.length; i += 2) {
            const dx = points[i]!;
            const dy = points[i + 1]!;
            gl.uniform4f(this.uDst, (dx / W) * 2 - 1, (dy / H) * 2 - 1, (w / W) * 2, (h / H) * 2);
            gl.uniform4f(this.uSrc, 0, 0, 1, 1);
            gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
        }
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        this.pending = true;
        this.scheduleFrame();
    }

    /**
     * eGFX tile cache on the GPU: `SURFACE_TO_CACHE` (store) and `CACHE_TO_SURFACE` (restore).
     *
     * WHY THIS EXISTS. The Rust compositor also keeps a CPU tile cache, but on this path that cache
     * is worthless and actively harmful: it snapshots the WASM `SurfaceBuf`, which has a
     * video-shaped HOLE wherever AVC painted, because AVC frames are decoded in JS and live only in
     * the GPU texture. Caching a block that straddles video stores transparency, and restoring it
     * later punches an opaque black rectangle through the live video.
     *
     * This is the same failure `copyRegion` was written to fix for `SurfaceToSurface`. The cache
     * commands were left on the CPU at the time because they were wire-confirmed unused -- true
     * while the host had the AVC444 GPO on, since it then encodes the whole surface as AVC444 and
     * has nothing to cache. With that GPO off the host switches to AVC420 plus heavy tile caching,
     * and the black rectangles came back everywhere.
     *
     * `points` empty means STORE (snapshot `w`x`h` at `sx`,`sy` into `slot`); non-empty means
     * RESTORE (blit `slot` to each point). One entry point so the two can never disagree.
     *
     * Slots are whole textures rather than an atlas: the host reuses a bounded set of slots and
     * re-stores them constantly, so per-slot textures keep each store a single `copyTexImage2D`
     * with no packing, and a re-store of a different size just reallocates that one texture.
     */
    cacheRegion(slot: number, sx: number, sy: number, w: number, h: number, points: Int32Array): void {
        if (!this.ensure()) return;
        this.syncSize();
        const gl = this.gl!;

        if (points.length === 0) {
            // STORE
            if (w <= 0 || h <= 0) return;
            this.tcStores++;
            let entry = this.tileCache.get(slot);
            if (!entry) {
                const tex = this.makeTexture(gl);
                if (!tex) return;
                entry = { tex, w: 0, h: 0 };
                this.tileCache.set(slot, entry);
            }
            gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
            gl.bindTexture(gl.TEXTURE_2D, entry.tex);
            // Reads from the FBO's colour attachment (the surface texture), so it captures whatever
            // is really on screen there -- chrome AND decoded video alike.
            gl.copyTexImage2D(gl.TEXTURE_2D, 0, gl.RGBA, sx, sy, w, h, 0);
            entry.w = w;
            entry.h = h;
            // Record WHERE this slot was captured from. A wrong tile on screen is otherwise
            // untraceable: the restore looks correct and the bad pixels came from a store that
            // happened earlier, somewhere else. With this, a visible artifact at (x,y) can be
            // looked up rather than guessed at.
            if (TILECACHE_TRACE) this.tcOrigin.set(slot, `${sx},${sy} ${w}x${h}`);
            gl.bindFramebuffer(gl.FRAMEBUFFER, null);
            return;
        }

        // RESTORE
        this.tcRestores++;
        const entry = this.tileCache.get(slot);
        if (!entry || entry.w <= 0 || entry.h <= 0) {
            // Cache miss. Leave the destination alone rather than painting garbage -- same choice
            // the Rust compositor makes for an unfilled slot.
            //
            // A miss is NOT benign: it leaves whatever was on screen, which is how a stale tile
            // survives. Counted, and each missing slot warns once, because "is the GPU cache
            // missing entries?" and "is it storing the wrong pixels?" produce identical artifacts
            // on screen and need opposite fixes.
            this.tcMisses++;
            if (!this.tcMissedSlots.has(slot)) {
                this.tcMissedSlots.add(slot);
                console.warn(
                    `[tilecache] MISS slot=${slot} never stored on the GPU; ` +
                        `${points.length / 2} destination(s) left stale`,
                );
            }
            return;
        }
        gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
        gl.bindTexture(gl.TEXTURE_2D, entry.tex);
        gl.viewport(0, 0, this.surfW, this.surfH);
        const W = this.surfW;
        const H = this.surfH;
        for (let i = 0; i + 1 < points.length; i += 2) {
            const dx = points[i]!;
            const dy = points[i + 1]!;
            // `dst <- src` for every restore, so an artifact's screen position maps straight back to
            // the source rect it was captured from. Behind ?ironstats=1 -- it is one line per
            // restore and there are thousands.
            if (TILECACHE_TRACE) {
                console.info(
                    `[tilecache] restore slot=${slot} dst=${dx},${dy} ${entry.w}x${entry.h} ` +
                        `src=${this.tcOrigin.get(slot) ?? '?'}`,
                );
            }
            gl.uniform4f(this.uDst, (dx / W) * 2 - 1, (dy / H) * 2 - 1, (entry.w / W) * 2, (entry.h / H) * 2);
            gl.uniform4f(this.uSrc, 0, 0, 1, 1);
            gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
        }
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
        this.pending = true;
        this.scheduleFrame();
    }

    /**
     * Hit-test a point against the RAIL window edges and return the CSS cursor for it, or null.
     *
     * WHY THIS EXISTS AT ALL. We advertise `ALLOWLOCALMOVESIZE` in the RAIL Client Status PDU, so
     * the host hands window move/resize to the client and DELIBERATELY STOPS SENDING RESIZE
     * CURSORS -- it expects the client's own window manager to draw them. Measured: a whole RAIL
     * session produced ZERO pointer updates of any kind while the host emitted
     * RAIL_ORDER_LOCALMOVESIZE. So the missing resize cursor was never a broken pipeline; there was
     * simply nothing upstream to render, and the affordance is ours to provide. The Microsoft AVD
     * web client draws its own for the same reason.
     *
     * QUERIED, NOT PUSHED. The window rects change on every drag frame. The taskbar payload
     * deliberately omits them for exactly that reason (carrying them re-rendered the React list on
     * every position update), so this is a pull: the caller asks on mousemove and nothing is
     * broadcast.
     *
     * `sx`,`sy` are SURFACE coordinates. Returns one of the eight resize cursors, or null when the
     * point is not near an edge of a presented window.
     */
    hitTestWindowEdge(sx: number, sy: number): RailEdgeHit | null {
        if (this.layoutMode !== LAYOUT_CLIP) return null;
        // Topmost first: `windowRects` is bottom-to-top by z, and the window a user means is the
        // one drawn last.
        for (let i = this.windowRects.length - 1; i >= 0; i--) {
            const r = this.windowRects[i]!;
            if (r.w <= 0 || r.h <= 0) continue;
            const m = RESIZE_EDGE_PX;
            // Outside the window plus its grab margin -> not this window. Checked before the inner
            // test so a window stacked on top cannot claim a neighbour's edge.
            if (sx < r.x - m || sx > r.x + r.w + m || sy < r.y - m || sy > r.y + r.h + m) continue;
            const left = sx <= r.x + m;
            const right = sx >= r.x + r.w - m;
            const top = sy <= r.y + m;
            const bottom = sy >= r.y + r.h - m;
            const hit = (cursor: string, edge: string) => ({ cursor, edge, id: this.windowIds[i] ?? 0, rect: r });
            if (top && left) return hit('nwse-resize', 'tl');
            if (top && right) return hit('nesw-resize', 'tr');
            if (bottom && left) return hit('nesw-resize', 'bl');
            if (bottom && right) return hit('nwse-resize', 'br');
            if (left) return hit('ew-resize', 'l');
            if (right) return hit('ew-resize', 'r');
            if (top) return hit('ns-resize', 't');
            if (bottom) return hit('ns-resize', 'b');
            // Inside this window and away from its edges: stop, do not fall through to a window
            // underneath whose edge happens to pass beneath this one.
            return null;
        }
        return null;
    }

    /**
     * Path A presentation layout, pushed by the Rust compositor whenever it changes.
     *
     * `rects` is flattened `[x, y, w, h, ...]` in surface (== desktop) coords. In CLIP mode we
     * present ONLY those rects — that is what makes the RemoteApp look right (no desktop
     * background, no shell) AND what kills the drag ghost: when a window moves, the area it
     * vacated stops being presented, so no one has to clear it. The old CPU path had to push a
     * transparent clear region for the vacated rect and re-extract the whole window from the WASM
     * `SurfaceBuf`; on this path that buffer has a video-shaped hole in it (the AVC frames are
     * decoded in JS and live only in the GPU texture), so those uploads punched black rectangles
     * through the live video. Clipping here instead means Rust never has to re-send pixels it
     * doesn't own.
     */
    setLayout(mode: number, surfaceW: number, surfaceH: number, rects: Int32Array): void {
        // FIVE ints per window: id, x, y, w, h. The id is carried so a client-side resize can be
        // addressed back to the right window -- `WindowMove` is keyed by window id.
        const next: Rect[] = [];
        const ids: number[] = [];
        for (let i = 0; i + 4 < rects.length; i += 5) {
            ids.push(rects[i]!);
            next.push({ x: rects[i + 1]!, y: rects[i + 2]!, w: rects[i + 3]!, h: rects[i + 4]! });
        }
        this.windowIds = ids;
        this.layoutMode = mode;
        this.windowRects = next;
        // THE authoritative surface size (eGFX CreateSurface), not the DOM canvas. See `syncSize`.
        if (surfaceW > 0 && surfaceH > 0) {
            this.wantW = surfaceW;
            this.wantH = surfaceH;
        }
        this.pending = true;
        this.scheduleFrame();
    }

    // ------------------------------------------------------------------ present loop

    private scheduleFrame(): void {
        if (this.rafScheduled) return;
        this.rafScheduled = true;
        requestAnimationFrame(() => this.present());
    }

    private present(): void {
        this.rafScheduled = false;
        const gl = this.gl;
        if (!gl || !this.pending) return;
        this.pending = false;
        const t0 = performance.now();

        // Re-sync the surface geometry EVERY present. This used to be called only from
        // `uploadChromeRegion` (plus once in `ensure`), which silently froze the overlay, the
        // surface texture and the viewport at whatever the canvas measured on the FIRST AVC frame.
        // Wire evidence from the proxy's controlled pair shows this session is 100% AVC —
        // fills=0, refs=0, one full-desktop region per frame — so `uploadChromeRegion` NEVER runs
        // and the geometry was never corrected after the canvas resized to the real desktop.
        // The 2D path cannot hit this: its size comes from the Rust-owned SurfaceBuf.
        this.syncSize();

        // One-shot sanity check: the decoded picture is macroblock-padded (e.g. 1312 for a 1308
        // surface), which is expected — source coords normalize by the DECODED size and
        // destination coords by the SURFACE size. Logged once so a real mismatch is still visible.
        if (this.hasAvc && !this.geomLogged) {
            this.geomLogged = true;
            console.warn(`[SurfaceRenderer] avcFrame=${this.avcW}x${this.avcH} surface=${this.surfW}x${this.surfH}`);
        }

        // 3) Present the surface-0 texture to the visible canvas — one quad, chrome + AVC together,
        //    clipped to the Path A layout. The canvas is cleared to TRANSPARENT first, every frame:
        //    that is the clip. Anything outside the app windows (the desktop the host never paints
        //    in RAIL, and the trail a dragged window leaves in the texture) is simply not drawn, so
        //    no clear/repaint plumbing is needed to erase it.
        gl.viewport(0, 0, this.surfW, this.surfH);
        gl.clearColor(0, 0, 0, 0);
        gl.clear(gl.COLOR_BUFFER_BIT);
        if (this.layoutMode !== LAYOUT_BLANK) {
            gl.bindTexture(gl.TEXTURE_2D, this.chromeTex);
            const W = this.surfW;
            const H = this.surfH;
            // FULLSCREEN (plain desktop, or the secure desktop the host paints with no RAIL window
            // of its own) presents the whole surface; CLIP presents one quad per app window. The
            // windows share one composited texture, so per-window quads need no z-ordering.
            const quads = this.layoutMode === LAYOUT_CLIP ? this.windowRects : [{ x: 0, y: 0, w: W, h: H }];
            for (const r of quads) {
                // Window rects are desktop-absolute and may hang off the edges (RAIL allows
                // negative origins); clamp to the surface so the source texcoords stay in range.
                const x0 = Math.max(0, Math.min(W, r.x));
                const y0 = Math.max(0, Math.min(H, r.y));
                const x1 = Math.max(0, Math.min(W, r.x + r.w));
                const y1 = Math.max(0, Math.min(H, r.y + r.h));
                const w = x1 - x0;
                const h = y1 - y0;
                if (w <= 0 || h <= 0) continue;
                // Dest in NDC (y-down surface → y-up NDC, so the quad grows downward);
                // source in texture space, top-left origin (FLIP_Y=false on every upload).
                gl.uniform4f(this.uDst, (x0 / W) * 2 - 1, 1 - (y0 / H) * 2, (w / W) * 2, -((h / H) * 2));
                gl.uniform4f(this.uSrc, x0 / W, y0 / H, w / W, h / H);
                gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
            }
        }

        // Watermark last, above everything just composited. Idempotent -- it only repaints when
        // the canvas size or the RAIL clip changed, so calling it every frame costs a comparison.
        //
        // TIMED SEPARATELY, and inside the present measurement rather than after it: dragging a
        // RAIL window changes the clip EVERY frame, so this is the one input that can turn an
        // idempotent call into a per-frame full repaint. Leaving it outside the timer would have
        // hidden exactly the cost worth watching.
        const wmT0 = STATS_ENABLED ? performance.now() : 0;
        this.drawWatermark();
        // drawWatermark can DISCOVER a block (no layer, no 2D context), so test after it runs.
        if (this.wmBlocked) this.paintBlocked();
        const now = performance.now();
        this.stats.record(now - t0, STATS_ENABLED ? now - wmT0 : 0, now);
        if (STATS_ENABLED && now - this.tcReportAt > 2000) {
            this.tcReportAt = now;
            console.info(
                `[tilecache] stores=${this.tcStores} restores=${this.tcRestores} ` +
                    `misses=${this.tcMisses} live_slots=${this.tileCache.size}`,
            );
        }
    }

    // ------------------------------------------------------------------ setup

    private ensure(): boolean {
        if (this.gl) return true;
        if (this.failed) return false;

        const parent = this.base.parentElement;
        if (!parent) {
            this.failed = true;
            return false;
        }
        const overlay = document.createElement('canvas');
        overlay.style.position = 'absolute';
        overlay.style.top = '0';
        overlay.style.left = '0';
        overlay.style.width = '100%';
        overlay.style.height = '100%';
        overlay.style.pointerEvents = 'none';
        // Marks this as the canvas that actually HOLDS the composited framebuffer, so an
        // external presenter (the webapp's multi-monitor controller) can find the pixels.
        // `#renderer` stays blank on this path, and sampling it yielded all-black monitors.
        overlay.setAttribute('data-iron-present', '1');
        overlay.width = Math.max(1, this.base.width);
        overlay.height = Math.max(1, this.base.height);
        if (getComputedStyle(parent).position === 'static') {
            parent.style.position = 'relative';
        }
        parent.appendChild(overlay);

        // `preserveDrawingBuffer` is REQUIRED, not a nicety: an external presenter reads this
        // canvas with drawImage() from its own rAF, i.e. outside our draw tick. Without it the
        // drawing buffer is cleared after compositing and every such read returns empty.
        const gl = overlay.getContext('webgl2', {
            alpha: true,
            premultipliedAlpha: false,
            antialias: false,
            preserveDrawingBuffer: true,
        });
        if (!gl) {
            overlay.remove();
            this.failed = true;
            console.error('[SurfaceRenderer] WebGL2 unavailable; staying on the CPU present path');
            return false;
        }
        const prog = this.buildProgram(gl);
        if (!prog) {
            overlay.remove();
            this.failed = true;
            return false;
        }
        gl.useProgram(prog);

        const vbo = gl.createBuffer();
        gl.bindBuffer(gl.ARRAY_BUFFER, vbo);
        gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 0, 1, 0, 0, 1, 1, 1]), gl.STATIC_DRAW);
        const aPos = gl.getAttribLocation(prog, 'aPos');
        gl.enableVertexAttribArray(aPos);
        gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, 0, 0);

        this.chromeTex = this.makeTexture(gl);
        this.avcTex = this.makeTexture(gl);
        this.fbo = gl.createFramebuffer();
        this.uDst = gl.getUniformLocation(prog, 'uDst');
        this.uSrc = gl.getUniformLocation(prog, 'uSrc');

        this.overlay = overlay;
        this.gl = gl;
        this.surfW = 0;
        this.surfH = 0;
        this.syncSize();
        console.warn(`[SurfaceRenderer] WebGL surface renderer up (${overlay.width}x${overlay.height})`);
        return true;
    }

    private makeTexture(gl: WebGL2RenderingContext): WebGLTexture | null {
        const tex = gl.createTexture();
        gl.bindTexture(gl.TEXTURE_2D, tex);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
        gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
        return tex;
    }

    /**
     * Size the overlay + surface texture to the AUTHORITATIVE eGFX surface size.
     *
     * This used to read `base.width/height` — the DOM canvas — which is a different quantity that
     * merely usually agrees with the eGFX surface. When they disagreed the damage was PERMANENT:
     * the host paints the full desktop only in its opening frames and sends small delta regions
     * forever after, so any area that landed at the wrong size was never repainted and stayed at
     * the transparent init, which the shader forces to opaque BLACK. Taking the size from Rust
     * (`setLayout`) also means the texture is only ever reallocated on a real ResetGraphics —
     * exactly when the host repaints the new generation anyway — so a reallocation can no longer
     * silently discard an accumulated desktop that nothing will redraw.
     *
     * Falls back to the canvas only until the first layout arrives.
     */
    /**
     * Install the session watermark (`RDPGFX_CMDID_WATERMARK`, a proxy extension).
     *
     * WHY A SEPARATE CANVAS, and not a blend into the pixels. The watermark used to be a CPU
     * blend in Rust, applied per extracted region. That structurally cannot reach two things:
     *   - AVC-painted pixels, which are decoded in JS and live only in the GPU texture. They
     *     never traverse WASM, so on AVD -- where the app content IS AVC -- most of the screen
     *     went unwatermarked even in full-desktop.
     *   - RAIL Path A, whose `send_output_rect` never called the blend at all.
     * A sibling layer above the composited canvas is immune to both: it does not care which codec
     * produced the pixels underneath, or which present path drew them. It is also how the
     * Microsoft AVD web client does it -- a `#watermarkingCanvas` sized to the session, and its
     * "Watermark presented" trace fires ~80ms BEFORE WebCodecs initializes, i.e. entirely outside
     * the codec pipeline.
     *
     * The tile repeats on a `cellW`x`cellH` grid (382x205 from the PDU) with the QR at
     * (`offX`,`offY`) in each cell, matching the Rust `blend_watermark_into` placement.
     *
     * `difference` blending reproduces the Rust intent: that blend applied a luminance delta whose
     * SIGN opposed the background, so the QR stays legible on light and dark content alike. A flat
     * translucent draw cannot do that -- it vanishes against one end of the range. Drawing the tile
     * at intensity `opacity` under `difference` darkens light backgrounds and lightens dark ones by
     * that amount, which is the same behaviour on the GPU and for free.
     */
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
        // CONTRACT with the Rust side (`notify_watermark_blocked`): a zero-sized tile means a
        // watermark was MANDATED and could not be prepared -- a truncated or malformed PDU. It does
        // not mean "clear the watermark". Fail closed: block the session rather than present clean
        // pixels, which is the single outcome a watermark exists to prevent.
        if (width === 0 || height === 0 || cellW === 0 || cellH === 0 || opacity === 0) {
            this.wm = null;
            this.wmPattern = null;
            this.clearWatermark();
            this.wmBlocked = true;
            this.pending = true;
            this.scheduleFrame();
            return;
        }
        this.wmBlocked = false;
        this.wm = { rgba, width, height, cellW, cellH, offX, offY, opacity };
        this.wmPattern = null; // rebuilt lazily against the live 2D context
        this.wmDrawnW = 0; // force a redraw
        this.pending = true;
        this.scheduleFrame();
    }

    /**
     * Cover the session with an opaque panel explaining WHY it is blank.
     *
     * A silent black screen is the wrong failure here -- it is indistinguishable from the render
     * bugs this client has actually had, and would send whoever hits it debugging graphics instead
     * of reading the one line that explains it. Painted on the watermark canvas with blending
     * turned off, so it is opaque rather than differenced against the content underneath.
     */
    private paintBlocked(): void {
        const c = this.ensureWatermarkCanvas();
        if (!c) return;
        if (c.width !== this.surfW || c.height !== this.surfH) {
            c.width = Math.max(1, this.surfW);
            c.height = Math.max(1, this.surfH);
            this.wmBlockedPainted = false;
        }
        if (this.wmBlockedPainted) return;
        const ctx = c.getContext('2d');
        if (!ctx) return;
        c.style.mixBlendMode = 'normal';
        // Tells the multi-monitor blit to copy this opaquely instead of differencing it, so the
        // explanation reaches the secondary displays intact rather than as inverted noise.
        c.setAttribute('data-iron-watermark-blocked', '1');
        ctx.clearRect(0, 0, c.width, c.height);
        ctx.fillStyle = '#0b0b0c';
        ctx.fillRect(0, 0, c.width, c.height);
        ctx.fillStyle = '#e5e5e6';
        ctx.textAlign = 'center';
        ctx.textBaseline = 'middle';
        ctx.font = '600 20px system-ui, sans-serif';
        ctx.fillText('Session hidden: the security watermark could not be displayed.', c.width / 2, c.height / 2 - 14);
        ctx.font = '400 15px system-ui, sans-serif';
        ctx.fillStyle = '#a1a1a6';
        ctx.fillText(
            'This session is required to be watermarked. Reconnect, or contact your administrator.',
            c.width / 2,
            c.height / 2 + 16,
        );
        this.wmBlockedPainted = true;
    }

    /** Drop the watermark layer (session reset). */
    private clearWatermark(): void {
        this.wmBlockedPainted = false;
        this.wmCanvas?.remove();
        this.wmCanvas = null;
        this.wmDrawnW = 0;
        this.wmDrawnH = 0;
        this.wmDrawnClip = '';
    }

    /**
     * Create the watermark canvas as a sibling ABOVE the present canvas.
     *
     * `isolation: isolate` on the parent is required, not cosmetic: without it `mix-blend-mode`
     * blends against whatever is behind the parent in the page rather than against our own
     * presented pixels.
     */
    private ensureWatermarkCanvas(): HTMLCanvasElement | null {
        if (this.wmCanvas) return this.wmCanvas;
        const parent = this.overlay?.parentElement;
        if (!parent) return null;
        const c = document.createElement('canvas');
        c.style.position = 'absolute';
        c.style.top = '0';
        c.style.left = '0';
        c.style.width = '100%';
        c.style.height = '100%';
        c.style.pointerEvents = 'none';
        c.style.mixBlendMode = 'difference';
        // Lets an external presenter (the multi-monitor controller) find and re-apply this layer.
        c.setAttribute('data-iron-watermark', '1');
        parent.style.isolation = 'isolate';
        parent.appendChild(c);
        this.wmCanvas = c;
        return c;
    }

    /**
     * Repaint the watermark layer. Cheap and idempotent: it redraws only when the canvas size or
     * the RAIL clip actually changed, so the per-frame present path can call it unconditionally.
     */
    private drawWatermark(): void {
        const wm = this.wm;
        if (!wm) return;
        const c = this.ensureWatermarkCanvas();
        if (!c) {
            // The layer is the ONLY thing watermarking AVC pixels. If it cannot be created we are
            // presenting unmarked content, so stop presenting instead.
            console.error('[SurfaceRenderer] watermark layer unavailable — blocking the session');
            this.wmBlocked = true;
            return;
        }

        // In RAIL the canvas is desktop-sized and everything outside an app window is transparent,
        // so a full-canvas watermark would float QR tiles over the user's OWN desktop. Clip to the
        // app windows -- which is where the remote content, and the thing worth marking, actually
        // is. Full-desktop has no clip and covers everything.
        const clip = this.layoutMode === LAYOUT_CLIP ? this.windowRects : null;
        const clipKey = clip ? clip.map((r) => `${r.x},${r.y},${r.w},${r.h}`).join(';') : '';
        if (this.surfW === this.wmDrawnW && this.surfH === this.wmDrawnH && clipKey === this.wmDrawnClip) {
            return;
        }
        if (c.width !== this.surfW || c.height !== this.surfH) {
            c.width = Math.max(1, this.surfW);
            c.height = Math.max(1, this.surfH);
            this.wmPattern = null; // a resize drops the context state the pattern belongs to
        }
        const ctx = c.getContext('2d');
        if (!ctx) {
            console.error('[SurfaceRenderer] watermark 2D context unavailable — blocking the session');
            this.wmBlocked = true;
            return;
        }
        // Restore the blend mode: `paintBlocked` turns it off to draw an opaque panel, and a
        // recovered session would otherwise keep rendering its tiles with no blending at all.
        c.style.mixBlendMode = 'difference';
        c.removeAttribute('data-iron-watermark-blocked');
        this.wmBlockedPainted = false;
        ctx.clearRect(0, 0, c.width, c.height);

        if (!this.wmPattern) {
            // One cell of the repeating grid: the QR at its offset, transparent elsewhere. The
            // tile is drawn at intensity `opacity` (a neutral grey) because `difference` turns that
            // into a +/- delta against the background rather than a fixed colour.
            const cell = document.createElement('canvas');
            cell.width = wm.cellW;
            cell.height = wm.cellH;
            const cctx = cell.getContext('2d');
            if (!cctx) {
                // FAIL CLOSED. Returning here would leave the layer canvas cleared and transparent,
                // so the session would present NORMALLY and UNWATERMARKED with no error -- the proxy
                // mandated a watermark, we could not build it, and nobody would ever know.
                console.error('[SurfaceRenderer] watermark tile context unavailable — blocking the session');
                this.wmBlocked = true;
                return;
            }
            const img = cctx.createImageData(wm.width, wm.height);
            const level = Math.min(255, Math.max(0, wm.opacity));
            for (let i = 0; i < wm.width * wm.height; i++) {
                const a = wm.rgba[i * 4 + 3] ?? 0;
                if (a === 0) continue;
                // Scale the delta by the tile's own alpha so antialiased QR edges stay soft.
                const v = Math.round((level * a) / 255);
                img.data[i * 4] = v;
                img.data[i * 4 + 1] = v;
                img.data[i * 4 + 2] = v;
                img.data[i * 4 + 3] = 255;
            }
            cctx.putImageData(img, wm.offX, wm.offY);
            this.wmPattern = ctx.createPattern(cell, 'repeat');
        }
        if (!this.wmPattern) {
            // Same fail-open trap as the tile context above: no pattern means no tiles get drawn,
            // and a transparent layer over a healthy session is indistinguishable from no watermark
            // at all. Block instead.
            console.error('[SurfaceRenderer] watermark pattern unavailable — blocking the session');
            this.wmBlocked = true;
            return;
        }

        ctx.fillStyle = this.wmPattern;
        if (clip) {
            for (const r of clip) {
                if (r.w <= 0 || r.h <= 0) continue;
                ctx.fillRect(r.x, r.y, r.w, r.h);
            }
        } else {
            ctx.fillRect(0, 0, c.width, c.height);
        }

        this.wmDrawnW = this.surfW;
        this.wmDrawnH = this.surfH;
        this.wmDrawnClip = clipKey;
    }

    private syncSize(): void {
        const gl = this.gl!;
        const w = Math.max(1, this.wantW || this.base.width);
        const h = Math.max(1, this.wantH || this.base.height);
        if (w === this.surfW && h === this.surfH) return;
        console.warn(
            `[SurfaceRenderer] surface texture ${this.surfW}x${this.surfH} -> ${w}x${h} ` +
                `(eGFX=${this.wantW}x${this.wantH} canvas=${this.base.width}x${this.base.height}) — reallocating (clears it)`,
        );
        this.surfW = w;
        this.surfH = h;
        if (this.overlay) {
            this.overlay.width = w;
            this.overlay.height = h;
        }
        gl.viewport(0, 0, w, h);
        // Re-allocate the surface texture storage (a resize clears it; the host repaints on reset).
        gl.bindTexture(gl.TEXTURE_2D, this.chromeTex);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
        // (Re)attach the surface texture as the FBO color target (for the AVC render-to-texture).
        gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
        gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, this.chromeTex, 0);
        gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    }

    private buildProgram(gl: WebGL2RenderingContext): WebGLProgram | null {
        const compile = (type: number, src: string): WebGLShader | null => {
            const sh = gl.createShader(type);
            if (!sh) return null;
            gl.shaderSource(sh, src);
            gl.compileShader(sh);
            if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
                console.error('[SurfaceRenderer] shader compile failed:', gl.getShaderInfoLog(sh));
                return null;
            }
            return sh;
        };
        const vs = compile(gl.VERTEX_SHADER, VERT);
        const fs = compile(gl.FRAGMENT_SHADER, FRAG);
        if (!vs || !fs) return null;
        const prog = gl.createProgram();
        if (!prog) return null;
        gl.attachShader(prog, vs);
        gl.attachShader(prog, fs);
        gl.linkProgram(prog);
        if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
            console.error('[SurfaceRenderer] program link failed:', gl.getProgramInfoLog(prog));
            return null;
        }
        return prog;
    }

    dispose(): void {
        const gl = this.gl;
        if (gl) {
            for (const entry of this.tileCache.values()) gl.deleteTexture(entry.tex);
        }
        this.tileCache.clear();
        this.clearWatermark();
        this.wm = null;
        this.wmPattern = null;
        this.overlay?.remove();
        this.overlay = null;
        this.gl = null;
        this.chromeTex = null;
        this.avcTex = null;
        this.copyTex = null;
    }
}
