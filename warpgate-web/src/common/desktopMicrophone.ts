// The viewer's microphone for the target.
//
// While the viewer has the microphone on and the target records, audio is captured with an
// AudioWorklet, converted to the format the target asked for (channel count and sample
// rate; the browser captures at the device's rate) and sent as binary WebSocket frames,
// `[7]` followed by interleaved signed 16-bit little-endian samples.

export const MICROPHONE_FRAME_KIND = 7

export interface MicrophoneFormat {
    channels: number
    sampleRate: number
    framesPerPacket: number
}

/**
 * Converts captured audio to `to.sampleRate`, by linear interpolation. Keeps its position
 * between calls, so the result does not depend on how the input is split.
 */
export class LinearResampler {
    private readonly step: number
    /** Position of the next output sample, in input samples after `last`. */
    private position = 0
    private last = 0

    constructor(fromRate: number, toRate: number) {
        this.step = fromRate / toRate
    }

    process(input: Float32Array): Float32Array {
        const output: number[] = []
        let previous = this.last
        let position = this.position
        for (let i = 0; i < input.length; i++) {
            // biome-ignore lint/style/noNonNullAssertion: within bounds
            const current = input[i]!
            // Output samples between `previous` (at 0) and `current` (at 1).
            while (position < 1) {
                output.push(previous + (current - previous) * position)
                position += this.step
            }
            position -= 1
            previous = current
        }
        this.last = previous
        this.position = position
        return Float32Array.from(output)
    }
}

/** Mixes captured `planes` (one per channel) down or up to `channels` planes. */
export function mixChannels(
    planes: Float32Array[],
    channels: number,
): Float32Array[] {
    if (channels === 1 && planes.length > 1) {
        const frames = planes[0]?.length ?? 0
        const mono = new Float32Array(frames)
        for (const plane of planes) {
            plane.forEach((sample, frame) => {
                // biome-ignore lint/style/noNonNullAssertion: within bounds
                mono[frame]! += sample / planes.length
            })
        }
        return [mono]
    }
    return Array.from(
        { length: channels },
        // A mono microphone fills every channel.
        (_, channel) =>
            // biome-ignore lint/style/noNonNullAssertion: planes is not empty
            planes[Math.min(channel, planes.length - 1)]!,
    )
}

/** Interleaves equally long `planes`. */
export function interleave(planes: Float32Array[]): Float32Array {
    const frames = Math.min(...planes.map(plane => plane.length))
    const channels = planes.length
    const out = new Float32Array(frames * channels)
    planes.forEach((plane, channel) => {
        for (let frame = 0; frame < frames; frame++) {
            // biome-ignore lint/style/noNonNullAssertion: within bounds
            out[frame * channels + channel] = plane[frame]!
        }
    })
    return out
}

/** `[7][s16le…]`, clamping samples to [-1, 1]. */
export function encodeMicrophoneFrame(samples: Float32Array): ArrayBuffer {
    const buf = new ArrayBuffer(1 + samples.length * 2)
    const view = new DataView(buf)
    view.setUint8(0, MICROPHONE_FRAME_KIND)
    samples.forEach((sample, i) => {
        const clamped = Math.max(-1, Math.min(1, sample))
        view.setInt16(
            1 + i * 2,
            clamped < 0 ? clamped * 0x8000 : clamped * 0x7fff,
            true,
        )
    })
    return buf
}

// Posts each render quantum's input, one Float32Array per channel.
const CAPTURE_WORKLET = `
registerProcessor('warpgate-capture', class extends AudioWorkletProcessor {
    process(inputs) {
        const input = inputs[0]
        if (input && input.length > 0) {
            this.port.postMessage(input.map(channel => channel.slice()))
        }
        return true
    }
})
`

export class DesktopMicrophone {
    private stream: MediaStream | null = null
    private context: AudioContext | null = null
    private node: AudioWorkletNode | null = null
    private format: MicrophoneFormat | null = null
    /** One per channel the target records. */
    private resamplers: LinearResampler[] = []
    private packet: number[] = []

    constructor(private readonly send: (frame: ArrayBuffer) => void) {}

    get enabled(): boolean {
        return this.stream !== null
    }

    /** Asks for the microphone; run it from a user gesture. */
    async enable(): Promise<void> {
        if (this.stream) {
            return
        }
        const stream = await navigator.mediaDevices.getUserMedia({
            audio: {
                echoCancellation: true,
                noiseSuppression: true,
                autoGainControl: true,
            },
        })
        try {
            const context = new AudioContext({ latencyHint: 'interactive' })
            const module = URL.createObjectURL(
                new Blob([CAPTURE_WORKLET], { type: 'text/javascript' }),
            )
            try {
                await context.audioWorklet.addModule(module)
            } finally {
                URL.revokeObjectURL(module)
            }
            const node = new AudioWorkletNode(context, 'warpgate-capture')
            node.port.onmessage = (event: MessageEvent<Float32Array[]>) =>
                this.capture(event.data)
            context.createMediaStreamSource(stream).connect(node)
            await context.resume()
            this.stream = stream
            this.context = context
            this.node = node
            this.setFormat(this.format)
        } catch (e) {
            for (const track of stream.getTracks()) {
                track.stop()
            }
            throw e
        }
    }

    disable(): void {
        this.node?.disconnect()
        void this.context?.close()
        for (const track of this.stream?.getTracks() ?? []) {
            track.stop()
        }
        this.stream = null
        this.context = null
        this.node = null
        this.setFormat(this.format)
    }

    /** What the target records in, or `null` while it does not record. */
    setFormat(format: MicrophoneFormat | null): void {
        this.format = format
        this.packet = []
        const context = this.context
        this.resamplers =
            format && context
                ? Array.from(
                      { length: format.channels },
                      () =>
                          new LinearResampler(
                              context.sampleRate,
                              format.sampleRate,
                          ),
                  )
                : []
    }

    private capture(planes: Float32Array[]): void {
        const format = this.format
        if (
            !format ||
            planes.length === 0 ||
            this.resamplers.length !== format.channels
        ) {
            return
        }
        const resampled = mixChannels(planes, format.channels).map(
            (plane, channel) =>
                // biome-ignore lint/style/noNonNullAssertion: one per channel
                this.resamplers[channel]!.process(plane),
        )
        for (const sample of interleave(resampled)) {
            this.packet.push(sample)
        }
        const packetSamples =
            Math.max(1, format.framesPerPacket) * format.channels
        while (this.packet.length >= packetSamples) {
            const packet = this.packet.splice(0, packetSamples)
            this.send(encodeMicrophoneFrame(Float32Array.from(packet)))
        }
    }
}
