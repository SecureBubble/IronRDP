/**
 * RDP-specific extension factories for file transfer.
 *
 * These create Extension objects that are dispatched through the WASM
 * invoke_extension() / extension() mechanism in ironrdp-web.
 */

import { Extension } from '../../../crates/ironrdp-web/pkg/ironrdp_web';
import type { FileInfo } from './FileTransfer';

// Builder-time callback extensions (registered via SessionBuilder.extension())

export function filesAvailableCallback(cb: (files: FileInfo[], clipDataId?: number) => void): Extension {
    return new Extension('files_available_callback', cb as unknown);
}

export function fileContentsRequestCallback(
    cb: (request: {
        streamId: number;
        index: number;
        flags: number;
        position: number;
        size: number;
        dataId?: number;
    }) => void,
): Extension {
    return new Extension('file_contents_request_callback', cb as unknown);
}

export function fileContentsResponseCallback(
    cb: (response: { streamId: number; isError: boolean; data: Uint8Array }) => void,
): Extension {
    return new Extension('file_contents_response_callback', cb as unknown);
}

export function lockCallback(cb: (dataId: number) => void): Extension {
    return new Extension('lock_callback', cb as unknown);
}

export function unlockCallback(cb: (dataId: number) => void): Extension {
    return new Extension('unlock_callback', cb as unknown);
}

export function locksExpiredCallback(cb: (clipDataIds: Uint32Array) => void): Extension {
    return new Extension('locks_expired_callback', cb as unknown);
}

export function formatListResponseCallback(cb: (ok: boolean) => void): Extension {
    return new Extension('format_list_response_callback', cb as unknown);
}

// Virtual printer (RDPDR) extensions
//
// Registering `printJobStreamCallbacks` activates the browser-side virtual
// printer device. The printer backend streams write chunks as they arrive
// instead of buffering a completed job in WASM memory. By default, the web
// connector follows FreeRDP's macOS heuristic where possible:
// browser-reported macOS 14+ uses Microsoft Print to PDF, and other clients use
// MS Publisher Imagesetter for PostScript bytes. Use `printerDriverName` when
// the target host needs a different installed printer driver. Jobs larger than
// 128 MiB are rejected, and queued write chunks are bounded to protect browser
// memory. `printerName`, `printerDeviceId`, and `printerDriverName` are
// optional; sensible defaults are applied when omitted.

export const PrinterDriverName = {
    PostScript: 'MS Publisher Imagesetter',
    MicrosoftPrintToPdf: 'Microsoft Print to PDF',
} as const;

export interface PrintJobStreamCallbacks {
    onJobStart?: (fileId: number) => void;
    onJobData: (fileId: number, chunk: Uint8Array) => void;
    onJobComplete: (fileId: number) => void;
    onJobError?: (fileId: number) => void;
}

export function printJobStreamCallbacks(callbacks: PrintJobStreamCallbacks): Extension {
    return new Extension('print_job_stream_callbacks', callbacks as unknown);
}

export function printerName(name: string): Extension {
    return new Extension('printer_name', name);
}

export function printerDeviceId(id: number): Extension {
    return new Extension('printer_device_id', id);
}

export function printerDriverName(driverName: string): Extension {
    return new Extension('printer_driver_name', driverName);
}

// Audio output (RDPSND) extension
//
// Registering `soundCallbacks` activates server→client audio playback. The client
// advertises a single PCM format (44.1 kHz, stereo, 16-bit signed) and the server
// transcodes to it, so no audio codec runs in the browser. Each received chunk is
// delivered to `onWave(sampleRate, channels, bitsPerSample, pcm)` as raw
// little-endian PCM; feed it to the Web Audio API for playback. `onClose` fires
// when the server tears the audio stream down.

export interface SoundCallbacks {
    onWave: (sampleRate: number, channels: number, bitsPerSample: number, pcm: Uint8Array) => void;
    onClose?: () => void;
}

export function soundCallbacks(callbacks: SoundCallbacks): Extension {
    return new Extension('sound_callbacks', callbacks as unknown);
}

// Runtime operation extensions (invoked via Session.invokeExtension())

export function requestFileContents(params: {
    stream_id: number;
    file_index: number;
    flags: number;
    position: number;
    size: number;
    clip_data_id?: number;
}): Extension {
    return new Extension('request_file_contents', params as unknown);
}

export function submitFileContents(params: { stream_id: number; is_error: boolean; data: Uint8Array }): Extension {
    return new Extension('submit_file_contents', params as unknown);
}

export function initiateFileCopy(files: FileInfo[]): Extension {
    return new Extension('initiate_file_copy', files as unknown);
}

// AVC (H.264) WebCodecs decode extensions
//
// Registering `avcDecodeCallback` lets the run loop hand a compressed AVC main
// sub-stream to the browser WebCodecs decoder. The decoder returns RGBA
// asynchronously via `onAvcDecoded` (invoked through Session.invokeExtension), which
// re-enters the run loop as a normal output region.

export function avcDecodeCallback(
    cb: (
        surfaceId: number,
        frameId: number,
        originX: number,
        originY: number,
        // Flat [x, y, w, h, x, y, w, h, ...] valid region rects in SURFACE coords. The H.264
        // picture is coded at full surface size; only these sub-rects are valid video (the rest
        // is YUV(0,0,0) = green padding), so the decoder blits ONLY these.
        regions: Uint32Array,
        data: Uint8Array,
    ) => void,
): Extension {
    return new Extension('avc_decode_callback', cb as unknown);
}

export function onAvcDecoded(params: {
    surfaceId: number;
    frameId: number;
    // Region-rect origin in SURFACE coords; the run loop composites into this surface's buffer.
    x: number;
    y: number;
    width: number;
    height: number;
    data: Uint8Array;
}): Extension {
    return new Extension('on_avc_decoded', params as unknown);
}

/** Decode-complete ack signal: the WebCodecs decoder produced a frame, so send its
 *  deferred eGFX FrameAcknowledge (frame_id only, no pixels). Fired at DECODE — not at
 *  present — so the server is paced to real decode throughput instead of a present
 *  round-trip (see AvcDecoder.onFrame). Presentation happens independently on the next
 *  rAF; the bounded present FIFO caps display latency. */
/** One taskbar-listed RemoteApp window, as reported by {@link railWindowsCallback}. */
export interface RailWindow {
    /** RAIL window id — pass this to {@link railActivate}. */
    id: number;
    title: string;
    /** True for the window the HOST reports as active (not a client-side guess). */
    active: boolean;
    /** Key into the icon cache (`"cacheId:cacheEntry"`), or null while the window has no icon. */
    iconKey: string | null;
}

/** A window icon, delivered ONCE per cache slot. Convert and cache it by `key`. */
export interface RailIcon {
    key: string;
    width: number;
    height: number;
    /** Top-down RGBA8, ready for `ImageData`. */
    rgba: Uint8Array;
}

/**
 * The set of RemoteApp windows that belong in a taskbar, top-most first.
 *
 * Fired only when the set, a title/rect, or the active window actually CHANGES — a RemoteApp
 * emits window orders continuously, so this is deliberately not a per-frame feed. Windows are
 * filtered by the host's own `TaskbarButton` field rather than a heuristic: a live session
 * carries ~16 windows of which only a couple are real apps, and several impostors have titles
 * and non-zero sizes.
 */
export function railWindowsCallback(cb: (windows: RailWindow[], newIcons: RailIcon[]) => void): Extension {
    return new Extension('rail_windows_callback', cb as unknown);
}

/**
 * Bring a RAIL (RemoteApp) window to the foreground -- `TS_RAIL_ORDER_ACTIVATE`.
 *
 * This is the click-to-switch action behind an application taskbar. `window_id` is the RAIL
 * window id the client reports for a window (the same value logged as `window_id=0x...`).
 *
 * A MINIMISED window may not come back from Activate alone: restoring one needs
 * `TS_RAIL_ORDER_SYSCOMMAND` with SC_RESTORE, which is not implemented yet.
 */
export function railActivate(params: { window_id: number }): Extension {
    return new Extension('rail_activate', params as unknown);
}

export function onAvcAck(params: { frameId: number }): Extension {
    return new Extension('on_avc_ack', params as unknown);
}

/** Passive render-canvas update notification for an external multi-monitor presenter.
 *  The run loop calls it once per drawn NON-AVC region (eGFX blit + CPU AVC readback)
 *  with the updated rect in source-canvas pixel coords, so the presenter can redraw only
 *  the affected area on a real pixel change instead of sampling on a blind timer. The AVC
 *  GPU direct-draw path never re-enters the run loop, so it notifies separately via
 *  `AvcDecoder.setCanvasUpdatedCallback`. It never touches frame-ack / present flow. */
export function canvasUpdatedCallback(
    cb: (x: number, y: number, width: number, height: number) => void,
): Extension {
    return new Extension('canvas_updated_callback', cb as unknown);
}

/** Unified WebGL present (`?ironwebgl=1`). When registered, the run loop forwards each decoded
 *  NON-AVC region `(x, y, width, height, rgba)` here instead of painting the 2D canvas — JS uploads
 *  it into the single surface-0 WebGL texture via `texSubImage2D`. `rgba` is width×height×4 bytes,
 *  a view into WASM memory valid only for the synchronous call (copy/upload before returning). */
export function surfacePresentCallback(
    cb: (x: number, y: number, width: number, height: number, rgba: Uint8Array) => void,
): Extension {
    return new Extension('surface_present_callback', cb as unknown);
}

/** Unified WebGL present (`?ironwebgl=1`) LAYOUT. The run loop calls it whenever the RemoteApp
 *  presentation layout changes, with `mode` and the app-window rects flattened as
 *  `[x, y, w, h, ...]` in desktop (== surface) coords. `surfaceW`/`surfaceH` are the eGFX
 *  surface's real dimensions — the AUTHORITATIVE size for the GPU texture (the DOM canvas is a
 *  different quantity and using it caused permanent black regions; see `SurfaceRenderer.syncSize`). `mode` mirrors the Rust compositor's three
 *  present modes: 0 = blank (pre-first-app logon / no app window), 1 = full-screen unclipped (the
 *  secure desktop — Ctrl+Alt+Del / lock / UAC, which the host paints with no RAIL window of its
 *  own), 2 = clip to `rects`. Clipping on the GPU is what stops a dragged window leaving a ghost:
 *  the vacated area simply stops being presented, so nothing has to clear it. Never fires for a
 *  full desktop session (no Window List orders), so registering it is inert there. */
export function surfaceLayoutCallback(
    cb: (mode: number, surfaceW: number, surfaceH: number, rects: Int32Array) => void,
): Extension {
    return new Extension('surface_layout_callback', cb as unknown);
}

/** WebGL GPU copy (`?ironwebgl=1`). An eGFX `SURFACE_TO_SURFACE` screen-to-screen copy: move the
 *  `width`x`height` block at (`srcX`,`srcY`) to each destination point in `points` (flattened
 *  `[x, y, ...]`), all in surface coords. This MUST run on the GPU: the host uses this primitive to
 *  RELOCATE a window rather than re-encode it, and on this path the pixels being moved exist only
 *  in the GPU surface texture. Doing it from the WASM surface buffer instead copies a hole (black)
 *  and leaves the originals in place (duplicated content). */
export function surfaceCopyCallback(
    cb: (srcX: number, srcY: number, width: number, height: number, points: Int32Array) => void,
): Extension {
    return new Extension('surface_copy_callback', cb as unknown);
}

/** RAIL (RemoteApp) active-window rect notification. The run loop calls it when the
 *  active top-level window's presentation rect changes, with `(x, y, width, height)` in
 *  virtual-desktop coordinates (x/y may be negative). The webapp uses it to crop/scale
 *  the render surface so only the app window fills the viewport — RAIL paints the whole
 *  desktop surface, leaving the surround stale/unpainted. Never fires for a full desktop
 *  session (no Window List orders), so registering it is inert there. */
export function railWindowCallback(cb: (x: number, y: number, width: number, height: number) => void): Extension {
    return new Extension('rail_window_callback', cb as unknown);
}

/** Session watermark forwarded to the GPU AVC draw path so it can overdraw the tile
 *  onto each frame. `rgba` is the width×height ARGB→RGBA tile; it repeats on a
 *  cellW×cellH grid with the tile at (offX,offY); opacity is 0..=255. */
export function avcWatermarkCallback(
    cb: (
        rgba: Uint8Array,
        width: number,
        height: number,
        cellW: number,
        cellH: number,
        offX: number,
        offY: number,
        opacity: number,
    ) => void,
): Extension {
    return new Extension('avc_watermark_callback', cb as unknown);
}
