import type { ScreenScale } from '../enums/ScreenScale';
import type { NewSessionInfo } from './NewSessionInfo';
import { ConfigBuilder } from '../services/ConfigBuilder';
import type { Config } from '../services/Config';
import type { Extension } from './Extension';
import type { Callback } from '../lib/Observable';
import type { FileTransferProvider } from './FileTransferProvider';
import type { AvcDecoderProvider } from './AvcDecoderProvider';
import type { ClipboardData } from './ClipboardData';

export interface UserInteraction {
    setVisibility(state: boolean): void;

    setScale(scale: ScreenScale): void;

    configBuilder(): ConfigBuilder;

    connect(config: Config): Promise<NewSessionInfo>;

    setKeyboardUnicodeMode(useUnicode: boolean): void;

    ctrlAltDel(): void;

    metaKey(): void;

    ctrlC(): void;

    ctrlV(): void;

    shutdown(): void;

    setCursorStyleOverride(style: string | null): void;

    onWarningCallback(callback: Callback<string>): void;

    onClipboardRemoteUpdateCallback(callback: Callback<void>): void;

    resize(width: number, height: number, scale?: number): void;

    setEnableClipboard(enable: boolean): void;

    setEnableAutoClipboard(enable: boolean): void;

    saveRemoteClipboardData(): Promise<void>;

    sendClipboardData(): Promise<void>;

    invokeExtension(ext: Extension): void;

    /**
     * Enable file transfer support. Must be called before connect().
     * The provider becomes active after connect() resolves.
     * Implicitly enables clipboard (required for file transfer protocol).
     *
     * @param provider - Protocol-specific file transfer provider (e.g., RdpFileTransferProvider)
     * @returns The same provider, with monitoring hooks composed in
     */
    enableFileTransfer(provider: FileTransferProvider): FileTransferProvider;

    enableAvcDecoder(provider: AvcDecoderProvider): AvcDecoderProvider;

    /**
     * Route the session's clipboard to the caller instead of this page's clipboard -- for a page
     * that runs the session on behalf of a user elsewhere (the user's clipboard is not this page's).
     * Call after `ready` and before connect(). Returns a function that pushes the user's clipboard
     * content into the session (`null` or empty data clears it).
     */
    bridgeClipboard(bridge: ClipboardBridge): (data: ClipboardData | null) => Promise<void>;
}

export interface ClipboardBridge {
    /** The remote clipboard changed; `data` is its full content. */
    remoteChanged(data: ClipboardData): void;
    /** The host asked for the current local clipboard (start of session): push it, or push empty. */
    forceUpdate(): void;
}
