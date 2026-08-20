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
const STATS_ENABLED = (() => {
    try {
        return new URLSearchParams(globalThis.location?.search ?? '').get('ironstats') === '1';
    } catch {
        return false;
    }
})();

/** Present modes from the Rust compositor (`WEBGL_LAYOUT_*` in graphics.rs — keep in sync). */
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
    private windowStart = 0;
    record(ms: number, now: number): void {
        if (this.windowStart === 0) this.windowStart = now;
        this.frames++;
        this.sumMs += ms;
        this.maxMs = Math.max(this.maxMs, ms);
        const elapsed = now - this.windowStart;
        if (elapsed >= 2000) {
            const fps = (this.frames / elapsed) * 1000;
            // Off by default: this fired every 2s for the whole session and was a top console
            // flooder. `?ironstats=1` brings it back when measuring present cost.
            if (STATS_ENABLED) {
                console.info(
                    `[SurfaceRenderer] fps=${fps.toFixed(1)} present(avg/max)=${(this.sumMs / this.frames).toFixed(2)}/${this.maxMs.toFixed(2)}ms`,
                );
            }
            this.frames = 0;
            this.sumMs = 0;
            this.maxMs = 0;
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

    private rafScheduled = false;
    private pending = false;
    private readonly stats = new PresentStats();

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
    submitAvcFrame(frame: VideoFrame, rects: Rect[]): void {
        if (!this.ensure()) {
            frame.close();
            return;
        }
        this.syncSize();
        const gl = this.gl!;

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
        // Free the decoder buffer straight away — a pinned VideoFrame stalls the decoder.
        frame.close();

        // Draw this frame's regions into the retained surface texture. Same Y mapping as the rest
        // of the accumulation path: FBO row 0 == surface row 0 (no flip); the flip happens only at
        // present. Source coords normalize by the DECODED frame size (which is macroblock-padded,
        // e.g. 1312 for a 1308 surface); destination coords normalize by the surface size.
        if (this.avcW > 0 && this.avcH > 0 && rects.length > 0) {
            gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
            gl.viewport(0, 0, this.surfW, this.surfH);
            const W = this.surfW;
            const H = this.surfH;
            for (const r of rects) {
                if (r.w <= 0 || r.h <= 0) continue;
                gl.uniform4f(this.uDst, (r.x / W) * 2 - 1, (r.y / H) * 2 - 1, (r.w / W) * 2, (r.h / H) * 2);
                gl.uniform4f(this.uSrc, r.x / this.avcW, r.y / this.avcH, r.w / this.avcW, r.h / this.avcH);
                gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
            }
            gl.bindFramebuffer(gl.FRAMEBUFFER, null);
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
        const next: Rect[] = [];
        for (let i = 0; i + 3 < rects.length; i += 4) {
            next.push({ x: rects[i]!, y: rects[i + 1]!, w: rects[i + 2]!, h: rects[i + 3]! });
        }
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
            console.warn(
                `[SurfaceRenderer] avcFrame=${this.avcW}x${this.avcH} surface=${this.surfW}x${this.surfH}`,
            );
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
            const quads =
                this.layoutMode === LAYOUT_CLIP ? this.windowRects : [{ x: 0, y: 0, w: W, h: H }];
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


        this.stats.record(performance.now() - t0, performance.now());
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
        overlay.width = Math.max(1, this.base.width);
        overlay.height = Math.max(1, this.base.height);
        if (getComputedStyle(parent).position === 'static') {
            parent.style.position = 'relative';
        }
        parent.appendChild(overlay);

        const gl = overlay.getContext('webgl2', { alpha: true, premultipliedAlpha: false, antialias: false });
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
        this.overlay?.remove();
        this.overlay = null;
        this.gl = null;
        this.chromeTex = null;
        this.avcTex = null;
        this.copyTex = null;
    }
}
