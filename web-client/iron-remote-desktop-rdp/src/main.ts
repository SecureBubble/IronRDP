import wasm_init, {
    setup,
    DesktopSize,
    DeviceEvent,
    InputTransaction,
    SessionBuilder,
    ClipboardData,
    Extension,
    RdpFile,
} from '../../../crates/ironrdp-web/pkg/ironrdp_web';

export async function init(log_level: string) {
    await wasm_init();
    setup(log_level);
}

export { RdpFile };

export const Backend = {
    DesktopSize: DesktopSize,
    InputTransaction: InputTransaction,
    SessionBuilder: SessionBuilder,
    ClipboardData: ClipboardData,
    DeviceEvent: DeviceEvent,
};

// --- Pre-connection configuration extensions ---

export function preConnectionBlob(pcb: string): Extension {
    return new Extension('pcb', pcb);
}

// RDP load-balance info / routing token. The value is sent as the X.224
// Connection Request routing token (`Cookie: msts=<value>\r\n`); a leading
// `Cookie: msts=` in the passed string is stripped on the Rust side so callers
// may pass either the bare value or the full cookie form.
export function loadBalanceInfo(info: string): Extension {
    return new Extension('load_balance_info', info);
}

export type VmConnectMode = 'enhanced' | 'basic';

export function vmConnect(vmId: string, mode: VmConnectMode = 'enhanced'): Extension {
    if (vmId.trim() === '') {
        throw new Error('vmconnect requires a VM ID');
    }

    const payload = mode === 'enhanced' ? `${vmId};EnhancedMode=1` : vmId;
    return new Extension('vmconnect', payload);
}

export function displayControl(enable: boolean): Extension {
    return new Extension('display_control', enable);
}

// Advertise AVC420/AVC444 eGFX caps (H.264 "enhanced graphics"). When true the
// server may send AVC, decoded by the browser WebCodecs path; when false the
// server falls back to ClearCodec / RFX-Progressive. Maps to the web UI's
// "Enhanced graphics" toggle. Defaults to true on the Rust side if unset.
export function advertiseAvc(enable: boolean): Extension {
    return new Extension('advertise_avc', enable);
}

export function kdcProxyUrl(url: string): Extension {
    return new Extension('kdc_proxy_url', url);
}

// RemoteApp-style published app: run this program as the session shell (RDP
// "alternate shell") instead of the full desktop. Forwarded by the Bubble proxy to
// the target on its back leg. Empty string => normal desktop.
export function alternateShell(shell: string): Extension {
    return new Extension('alternate_shell', shell);
}

// RAIL (Remote Programs) launch over the `rail` static channel: negotiates RAIL,
// advertises Window List support, and launches `program` with `args` (the command
// line travels natively in the RAIL Client Execute PDU). This is the classic,
// non-eGFX RemoteApp path — use it ONLY for non-AVD targets. AVD does RAIL over a
// DVC + eGFX and must keep using its own path; do not call this for AVD sessions.
export function remoteApp(program: string, args?: string, workingDir?: string): Extension {
    return new Extension('remote_app', {
        program,
        args: args ?? '',
        workingDir: workingDir ?? '',
    });
}

export function outboundMessageSizeLimit(limit: number): Extension {
    return new Extension('outbound_message_size_limit', limit);
}

// Multi-monitor layout (Phase 0 — PROTOCOL ENABLEMENT ONLY).
//
// Advertises more than one monitor at connect time (GCC Client Monitor Data) so
// the remote host produces a single spanning virtual desktop equal to the
// bounding box of all monitors. In Phase 0 that spanning desktop is rendered into
// the existing single canvas (scroll/fit); the per-monitor browser window UI is
// Phase 1 and out of scope here.
//
// Each entry is in virtual-desktop pixel coordinates. The primary monitor MUST be
// at (0,0); set `primary: true` on exactly one entry (if none/several do, the
// first is used). Widths must be even (odd widths are adjusted down on the host).
//
// Example — two 1280x720 monitors side by side (2560x720 virtual desktop):
//   monitors([
//     { left: 0,    top: 0, width: 1280, height: 720, primary: true },
//     { left: 1280, top: 0, width: 1280, height: 720 },
//   ])
export interface MonitorLayoutEntry {
    left: number;
    top: number;
    width: number;
    height: number;
    primary?: boolean;
}

export function monitors(layout: MonitorLayoutEntry[]): Extension {
    return new Extension('monitors', layout);
}

export function enableCredssp(enable: boolean): Extension {
    return new Extension('enable_credssp', enable);
}

// --- AVC (H.264) WebCodecs decode (RDP-specific) ---

export { AvcDecoder } from './AvcDecoder';

// --- File transfer (RDP-specific) ---

export { RdpFileTransferProvider } from './RdpFileTransferProvider';
export type {
    RdpFileTransferProviderOptions,
    TransferProgress,
    FileTransferError,
    DownloadHandle,
    UploadHandle,
    DroppedFile,
} from './RdpFileTransferProvider';
export type { FileInfo, FileContentsRequest, FileContentsResponse } from './FileTransfer';
export { FileContentsFlags } from './FileContentsFlags';

// --- Storage backends ---
// Re-export for consumers who want to configure the storageBackend
// option on RdpFileTransferProviderOptions, implement a custom backend,
// or construct a specific backend instance directly.
export type { FileStorageBackend, FileWriteHandle, StorageBackendPreference } from './storage';
export { BlobStorageBackend } from './storage';
export { OpfsStorageBackend } from './storage';
export { detectStorageBackend } from './storage';

// Re-export extension factories for advanced consumers who want to
// register callbacks or invoke file transfer operations directly.
export {
    filesAvailableCallback,
    fileContentsRequestCallback,
    fileContentsResponseCallback,
    lockCallback,
    unlockCallback,
    locksExpiredCallback,
    requestFileContents,
    submitFileContents,
    initiateFileCopy,
    printJobStreamCallbacks,
    PrinterDriverName,
    printerName,
    printerDeviceId,
    printerDriverName,
    soundCallbacks,
    avcDecodeCallback,
    onAvcDecoded,
    onAvcAck,
    avcWatermarkCallback,
    canvasUpdatedCallback,
    railActivate,
    railSysCommand,
    SC_MINIMIZE,
    SC_MAXIMIZE,
    SC_CLOSE,
    SC_RESTORE,
    railWindowsCallback,
    railWindowCallback,
} from './extensions';
export type { PrintJobStreamCallbacks, RailIcon, RailWindow, SoundCallbacks } from './extensions';
