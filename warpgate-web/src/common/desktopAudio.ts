// The target's sound, played with the Web Audio API.
//
// Blocks arrive as binary WebSocket frames, `[6][channels: u8][sample rate: u32 LE]`
// followed by interleaved signed 16-bit little-endian samples. Each block is scheduled
// right after the previous one, so playback stays gapless while blocks keep up; a small
// lead (the jitter buffer) absorbs uneven arrival.

export const AUDIO_FRAME_KIND = 6
const AUDIO_HEADER_BYTES = 6

export interface AudioBlock {
    sampleRate: number
    channels: number
    /** One array of samples in [-1, 1) per channel. */
    planes: Float32Array<ArrayBuffer>[]
}

export function decodeAudioFrame(buf: ArrayBuffer): AudioBlock | null {
    if (buf.byteLength < AUDIO_HEADER_BYTES) {
        return null
    }
    const view = new DataView(buf)
    if (view.getUint8(0) !== AUDIO_FRAME_KIND) {
        return null
    }
    const channels = view.getUint8(1)
    const sampleRate = view.getUint32(2, true)
    // The range AudioBuffer accepts in every browser.
    if (channels < 1 || sampleRate < 8000 || sampleRate > 96000) {
        return null
    }
    const frames = Math.floor(
        (buf.byteLength - AUDIO_HEADER_BYTES) / (2 * channels),
    )
    const planes = Array.from(
        { length: channels },
        () => new Float32Array(frames),
    )
    let offset = AUDIO_HEADER_BYTES
    for (let frame = 0; frame < frames; frame++) {
        for (let channel = 0; channel < channels; channel++) {
            // biome-ignore lint/style/noNonNullAssertion: one plane per channel
            planes[channel]![frame] = view.getInt16(offset, true) / 32768
            offset += 2
        }
    }
    return { sampleRate, channels, planes }
}

/**
 * When to start the next block: right after the previous one, or `lead` seconds from now
 * when playback ran dry (or has not started). `null` drops the block, since the queue
 * already holds more than `maxLead` seconds and playing it would only add delay.
 */
export function scheduleBlock(
    now: number,
    queuedUntil: number,
    lead: number,
    maxLead: number,
): number | null {
    if (queuedUntil <= now) {
        return now + lead
    }
    if (queuedUntil - now > maxLead) {
        return null
    }
    return queuedUntil
}

export class DesktopAudioPlayer {
    private context: AudioContext | null = null
    private queuedUntil = 0

    constructor(
        private readonly lead = 0.08,
        private readonly maxLead = 0.4,
    ) {}

    get playing(): boolean {
        return this.context !== null
    }

    /** Must run from a user gesture, or the browser keeps the context suspended. */
    async start(): Promise<void> {
        this.context ??= new AudioContext({ latencyHint: 'interactive' })
        this.queuedUntil = 0
        await this.context.resume()
    }

    stop(): void {
        void this.context?.close()
        this.context = null
    }

    play(block: AudioBlock): void {
        const context = this.context
        const frames = block.planes[0]?.length ?? 0
        if (context?.state !== 'running' || frames === 0) {
            return
        }
        const at = scheduleBlock(
            context.currentTime,
            this.queuedUntil,
            this.lead,
            this.maxLead,
        )
        if (at === null) {
            return
        }
        // At the target's own rate: the context resamples to the device's.
        const buffer = context.createBuffer(
            block.channels,
            frames,
            block.sampleRate,
        )
        block.planes.forEach((plane, channel) => {
            buffer.copyToChannel(plane, channel)
        })
        const source = context.createBufferSource()
        source.buffer = buffer
        source.connect(context.destination)
        source.start(at)
        this.queuedUntil = at + buffer.duration
    }
}
