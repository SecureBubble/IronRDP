/**
 * The user's microphone as 16-bit PCM, in the rate and channel count the remote host asked for
 * (`micCallbacks` `onOpen`). Captures with `getUserMedia`, resamples in an AudioWorklet (off the
 * main thread) and hands ~20 ms chunks to `send` -- normally `session.invokeExtension(micData(pcm))`.
 *
 * Needs a secure context and the user's permission; in an iframe, the embedding page must allow
 * `microphone`. The browser's echo cancellation and noise suppression stay on: the remote session's
 * sound plays through the same speakers.
 */

// Linear resampler + float -> Int16 conversion. `sampleRate` is the AudioContext's (the device's).
const WORKLET = `
class BubbleMic extends AudioWorkletProcessor {
    constructor(options) {
        super();
        const { outRate, channels } = options.processorOptions;
        this.channels = channels;
        this.step = sampleRate / outRate;
        this.pos = 0;
        this.last = null;
        this.chunk = Math.round(outRate * 0.02) * channels;
        this.out = new Int16Array(this.chunk);
        this.fill = 0;
    }
    process(inputs) {
        const input = inputs[0];
        if (!input || input.length === 0) return true;
        const n = input[0].length;
        const src = [];
        for (let c = 0; c < this.channels; c++) src.push(input[Math.min(c, input.length - 1)]);
        if (!this.last) this.last = src.map((x) => x[0]);
        // Positions are relative to this block; -1 is the previous block's last sample.
        while (this.pos < n - 1) {
            const i = Math.floor(this.pos);
            const f = this.pos - i;
            for (let c = 0; c < this.channels; c++) {
                const a = i < 0 ? this.last[c] : src[c][i];
                const b = src[c][i + 1];
                const v = Math.max(-1, Math.min(1, a + (b - a) * f));
                this.out[this.fill++] = v < 0 ? v * 0x8000 : v * 0x7fff;
            }
            if (this.fill === this.chunk) {
                this.port.postMessage(this.out.buffer, [this.out.buffer]);
                this.out = new Int16Array(this.chunk);
                this.fill = 0;
            }
            this.pos += this.step;
        }
        this.pos -= n;
        this.last = src.map((x) => x[n - 1]);
        return true;
    }
}
registerProcessor('bubble-mic', BubbleMic);
`;

export class MicCapture {
    private ctx: AudioContext | null = null;
    private stream: MediaStream | null = null;
    private generation = 0;

    constructor(
        private readonly send: (pcm: Uint8Array) => void,
        private readonly onError: (error: unknown) => void = (error) => console.warn('[mic]', error),
    ) {}

    get active(): boolean {
        return this.ctx !== null;
    }

    /** Start (or restart) capturing for `sampleRate` / `channels`. */
    async start(sampleRate: number, channels: number): Promise<void> {
        this.stop();
        const generation = ++this.generation;
        try {
            const stream = await navigator.mediaDevices.getUserMedia({
                audio: { channelCount: channels, echoCancellation: true, noiseSuppression: true, autoGainControl: true },
            });
            if (generation !== this.generation) {
                stream.getTracks().forEach((t) => t.stop());
                return;
            }
            const ctx = new AudioContext();
            const url = URL.createObjectURL(new Blob([WORKLET], { type: 'text/javascript' }));
            try {
                await ctx.audioWorklet.addModule(url);
            } finally {
                URL.revokeObjectURL(url);
            }
            if (generation !== this.generation) {
                stream.getTracks().forEach((t) => t.stop());
                void ctx.close();
                return;
            }
            const node = new AudioWorkletNode(ctx, 'bubble-mic', { processorOptions: { outRate: sampleRate, channels } });
            node.port.onmessage = (e: MessageEvent<ArrayBuffer>) => this.send(new Uint8Array(e.data));
            // A silent route to the output keeps the graph running in every browser.
            const mute = ctx.createGain();
            mute.gain.value = 0;
            ctx.createMediaStreamSource(stream).connect(node).connect(mute).connect(ctx.destination);
            if (ctx.state === 'suspended') await ctx.resume().catch(() => {});
            this.ctx = ctx;
            this.stream = stream;
        } catch (error) {
            this.onError(error);
        }
    }

    stop(): void {
        this.generation++;
        this.stream?.getTracks().forEach((t) => t.stop());
        this.stream = null;
        void this.ctx?.close();
        this.ctx = null;
    }
}
