/**
 * The user's camera as H.264, at the size and rate the remote host asked for (`cameraCallbacks`
 * `onStart`). Captures with `getUserMedia`, scales (centre-cropped, never stretched) to the host's
 * size and encodes with WebCodecs -- on the user's GPU where the browser can -- in Annex B, one
 * access unit per picture, which `send` passes on (normally
 * `session.invokeExtension(cameraFrame(data, key))`).
 *
 * Needs a secure context and the user's permission; in an iframe, the embedding page must allow
 * `camera`.
 */
export class CameraCapture {
    private stream: MediaStream | null = null;
    private video: HTMLVideoElement | null = null;
    private encoder: VideoEncoder | null = null;
    private timer = 0;
    private generation = 0;
    private forceKey = false;

    constructor(
        private readonly send: (data: Uint8Array, key: boolean) => void,
        private readonly onError: (error: unknown) => void = (error) => console.warn('[camera]', error),
    ) {}

    get active(): boolean {
        return this.encoder !== null;
    }

    /** The next picture will be a key frame (the host lost its place). */
    keyframe(): void {
        this.forceKey = true;
    }

    async start(width: number, height: number, fps: number): Promise<void> {
        this.stop();
        const generation = ++this.generation;
        try {
            if (typeof VideoEncoder === 'undefined') throw new Error('this browser cannot encode video (no WebCodecs)');
            const stream = await navigator.mediaDevices.getUserMedia({
                video: { width: { ideal: width }, height: { ideal: height }, frameRate: { ideal: fps } },
            });
            if (generation !== this.generation) {
                stream.getTracks().forEach((t) => t.stop());
                return;
            }
            const video = document.createElement('video');
            video.muted = true;
            video.playsInline = true;
            video.srcObject = stream;
            await video.play();

            const config: VideoEncoderConfig = {
                // Constrained Baseline, level 3.1 (up to 1280x720 at 30 fps): what every Windows
                // H.264 decoder takes.
                codec: 'avc1.42E01F',
                width,
                height,
                framerate: fps,
                // About 0.1 bit per pixel: ~0.9 Mbit/s at 640x480, ~2.8 Mbit/s at 720p.
                bitrate: Math.round(width * height * fps * 0.1),
                latencyMode: 'realtime',
                avc: { format: 'annexb' },
            };
            const support = await VideoEncoder.isConfigSupported(config);
            if (!support.supported) throw new Error(`H.264 encoding at ${width}x${height} is not supported`);
            if (generation !== this.generation) {
                stream.getTracks().forEach((t) => t.stop());
                return;
            }

            const encoder = new VideoEncoder({
                output: (chunk) => {
                    const data = new Uint8Array(chunk.byteLength);
                    chunk.copyTo(data);
                    this.send(data, chunk.type === 'key');
                },
                error: (error) => this.onError(error),
            });
            encoder.configure(config);

            const canvas = new OffscreenCanvas(width, height);
            const ctx = canvas.getContext('2d', { alpha: false });
            if (!ctx) throw new Error('no 2D canvas');
            let count = 0;
            const keyEvery = fps * 2;
            this.timer = window.setInterval(() => {
                if (encoder.state !== 'configured' || encoder.encodeQueueSize > 2 || video.videoWidth === 0) return;
                // Centre-crop the camera picture to the host's aspect ratio.
                const scale = Math.max(width / video.videoWidth, height / video.videoHeight);
                const sw = width / scale;
                const sh = height / scale;
                ctx.drawImage(video, (video.videoWidth - sw) / 2, (video.videoHeight - sh) / 2, sw, sh, 0, 0, width, height);
                const frame = new VideoFrame(canvas, { timestamp: Math.round(performance.now() * 1000) });
                const keyFrame = this.forceKey || count % keyEvery === 0;
                this.forceKey = false;
                count++;
                encoder.encode(frame, { keyFrame });
                frame.close();
            }, 1000 / fps);

            this.stream = stream;
            this.video = video;
            this.encoder = encoder;
        } catch (error) {
            if (generation === this.generation) this.stop();
            this.onError(error);
        }
    }

    stop(): void {
        this.generation++;
        window.clearInterval(this.timer);
        this.timer = 0;
        if (this.encoder && this.encoder.state !== 'closed') this.encoder.close();
        this.encoder = null;
        this.stream?.getTracks().forEach((t) => t.stop());
        this.stream = null;
        if (this.video) this.video.srcObject = null;
        this.video = null;
    }
}
