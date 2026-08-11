import type { Extension } from './Extension';
import type { Session } from './Session';

/**
 * Protocol-agnostic interface for an out-of-band AVC (H.264) decoder.
 *
 * The concrete implementation lives in a protocol-specific package
 * (`AvcDecoder` in `iron-remote-desktop-rdp`, backed by the browser WebCodecs
 * `VideoDecoder`) and is injected into the web component via `enableAvcDecoder()`.
 * It registers a builder-time callback the run loop calls with compressed frames,
 * and returns decoded RGBA through `session.invokeExtension(...)`.
 */
export interface AvcDecoderProvider {
    /** Extensions to register on the SessionBuilder before connect(). */
    getBuilderExtensions(): Extension[];

    /** Called after connect() with the live session (the RGBA return path). */
    setSession(session: Session): void;

    /** Clean up the decoder. */
    dispose(): void;
}
