// Clipboard file transfer for the web desktop viewer.
//
// Nothing moves eagerly. An offer lists names and sizes; whoever pastes pulls byte
// ranges, one request per range, and Warpgate checks each against the offer and the
// target's size cap before relaying it.
//
// Upload: files the user pastes or drops into the page are offered to the target and
// stay in the page, which serves the ranges the target asks for.
// Download: files copied on the target are listed with a download button; a download
// pulls the file range by range into a Blob and saves it.

export interface DesktopFile {
    path: string
    size: number
    directory: boolean
}

export interface RemoteFileOffer {
    offer: number
    files: DesktopFile[]
}

export type FilesServerMessage =
    | {
          type: 'capabilities'
          clipboard_files_max_bytes: number | null
          drive_uploads_max_bytes: number | null
      }
    | { type: 'drive_upload'; id: number; stored: boolean }
    | { type: 'clipboard_files'; offer: number; files: DesktopFile[] }
    | {
          type: 'file_contents_request'
          id: number
          offer: number
          index: number
          offset: number
          length: number
      }
    | { type: 'file_contents'; id: number; data: string | null }

// Ranges a download asks for. Small enough that one base64 JSON frame stays modest,
// large enough that the per-range round trip doesn't dominate.
export const DOWNLOAD_CHUNK_BYTES = 1024 * 1024

// Upload chunk size for the session drive; the upload is paced by the socket.
export const UPLOAD_CHUNK_BYTES = 512 * 1024

// Offers the page keeps serving. The newest is the clipboard; older ones stay readable
// while a paste the target started from them may still be running.
const KEPT_OFFERS = 8

export function bytesToBase64(bytes: Uint8Array): string {
    let binary = ''
    const step = 0x8000
    for (let i = 0; i < bytes.length; i += step) {
        binary += String.fromCharCode(...bytes.subarray(i, i + step))
    }
    return btoa(binary)
}

export function base64ToBytes(data: string): Uint8Array {
    const binary = atob(data)
    const bytes = new Uint8Array(binary.length)
    for (let i = 0; i < binary.length; i++) {
        bytes[i] = binary.charCodeAt(i)
    }
    return bytes
}

// The name a download is saved under: the last path component, since a browser download
// cannot create directories.
export function downloadName(path: string): string {
    return path.split('/').pop() || 'download'
}

export interface DownloadProgress {
    path: string
    received: number
    size: number
}

export class DesktopFiles {
    // The target's per-copy cap; null while clipboard files are off.
    maxBytes: number | null = null
    remoteOffer: RemoteFileOffer | null = null
    // The session drive's capacity; null while drive redirection is off.
    driveMaxBytes: number | null = null
    // Files on the session drive, as this page uploaded them.
    driveFiles: { id: number; name: string; size: number }[] = []

    private nextUpload = 0
    private uploads = new Map<number, (stored: boolean) => void>()

    private nextOffer = 0
    private localOffers: { offer: number; files: File[] }[] = []
    private nextRequest = 0
    private pending = new Map<number, (data: Uint8Array | null) => void>()

    constructor(
        private readonly send: (msg: unknown) => void,
        // Called whenever `maxBytes` or `remoteOffer` change.
        private readonly changed: () => void = () => {},
    ) {}

    get enabled(): boolean {
        return this.maxBytes !== null
    }

    // Returns true when the message was a file message and has been handled.
    onMessage(msg: { type: string }): boolean {
        const m = msg as FilesServerMessage
        switch (m.type) {
            case 'capabilities':
                this.maxBytes = m.clipboard_files_max_bytes
                this.driveMaxBytes = m.drive_uploads_max_bytes
                this.changed()
                return true
            case 'drive_upload': {
                const resolve = this.uploads.get(m.id)
                if (resolve) {
                    this.uploads.delete(m.id)
                    resolve(m.stored)
                }
                return true
            }
            case 'clipboard_files':
                this.remoteOffer = { offer: m.offer, files: m.files }
                this.changed()
                return true
            case 'file_contents_request':
                void this.serve(m)
                return true
            case 'file_contents': {
                const resolve = this.pending.get(m.id)
                if (resolve) {
                    this.pending.delete(m.id)
                    resolve(m.data === null ? null : base64ToBytes(m.data))
                }
                return true
            }
            default:
                return false
        }
    }

    // Offer local files to the target. Returns the files actually offered: those that fit
    // the cap, in order (the server applies the same cut).
    offer(files: File[]): File[] {
        if (this.maxBytes === null || files.length === 0) {
            return []
        }
        let used = 0
        const kept: File[] = []
        for (const file of files) {
            if (used + file.size > this.maxBytes) {
                continue
            }
            used += file.size
            kept.push(file)
        }
        if (kept.length === 0) {
            return []
        }
        this.nextOffer = (this.nextOffer + 1) >>> 0
        const offer = this.nextOffer
        this.localOffers.push({ offer, files: kept })
        if (this.localOffers.length > KEPT_OFFERS) {
            this.localOffers.shift()
        }
        this.send({
            type: 'clipboard_files',
            offer,
            files: kept.map(file => ({
                path: file.name,
                size: file.size,
                directory: false,
            })),
        })
        return kept
    }

    // The target is reading a range of a file we offered.
    private async serve(request: {
        id: number
        offer: number
        index: number
        offset: number
        length: number
    }): Promise<void> {
        const file = this.localOffers.find(o => o.offer === request.offer)
            ?.files[request.index]
        if (!file || request.offset > file.size) {
            this.send({ type: 'file_contents', id: request.id })
            return
        }
        try {
            const end = Math.min(file.size, request.offset + request.length)
            const buffer = await file.slice(request.offset, end).arrayBuffer()
            this.send({
                type: 'file_contents',
                id: request.id,
                data: bytesToBase64(new Uint8Array(buffer)),
            })
        } catch {
            // The file went away (deleted, permission revoked) since it was offered.
            this.send({ type: 'file_contents', id: request.id })
        }
    }

    private request(
        offer: number,
        index: number,
        offset: number,
        length: number,
    ): Promise<Uint8Array | null> {
        this.nextRequest = (this.nextRequest + 1) >>> 0
        const id = this.nextRequest
        return new Promise(resolve => {
            this.pending.set(id, resolve)
            this.send({
                type: 'file_contents_request',
                id,
                offer,
                index,
                offset,
                length,
            })
        })
    }

    // Pull one file the target offered, whole. Throws if the target or Warpgate refuses a
    // range, which is also what a file the target no longer holds looks like.
    async fetch(
        offer: RemoteFileOffer,
        index: number,
        progress: (p: DownloadProgress) => void = () => {},
    ): Promise<Blob> {
        const file = offer.files[index]
        if (!file || file.directory) {
            throw new Error('Not a file')
        }
        const parts: Uint8Array[] = []
        let received = 0
        while (received < file.size) {
            const length = Math.min(DOWNLOAD_CHUNK_BYTES, file.size - received)
            const data = await this.request(
                offer.offer,
                index,
                received,
                length,
            )
            if (data === null) {
                throw new Error(`The target refused ${file.path}`)
            }
            if (data.length === 0) {
                throw new Error(`${file.path} ended early`)
            }
            parts.push(data)
            received += data.length
            progress({ path: file.path, received, size: file.size })
        }
        return new Blob(parts as BlobPart[])
    }

    // Upload one file to the session drive, which the target sees as a read-only
    // redirected drive named "Uploads". Resolves to whether Warpgate stored it.
    async uploadToDrive(
        file: File,
        progress: (sent: number) => void = () => {},
    ): Promise<boolean> {
        if (this.driveMaxBytes === null) {
            return false
        }
        this.nextUpload = (this.nextUpload + 1) >>> 0
        const id = this.nextUpload
        const done = new Promise<boolean>(resolve =>
            this.uploads.set(id, resolve),
        )
        this.send({
            type: 'drive_upload_start',
            id,
            name: file.name,
            size: file.size,
        })
        for (let offset = 0; offset < file.size; ) {
            const end = Math.min(file.size, offset + UPLOAD_CHUNK_BYTES)
            const chunk = new Uint8Array(
                await file.slice(offset, end).arrayBuffer(),
            )
            if (!this.uploads.has(id)) {
                // Refused part-way (over capacity, bad name): stop sending.
                break
            }
            this.send({
                type: 'drive_upload_chunk',
                id,
                data: bytesToBase64(chunk),
            })
            offset = end
            progress(offset)
        }
        const stored = await done
        if (stored) {
            this.driveFiles = [
                ...this.driveFiles.filter(f => f.name !== file.name),
                { id, name: file.name, size: file.size },
            ]
            this.changed()
        }
        return stored
    }

    removeFromDrive(id: number): void {
        this.send({ type: 'drive_upload_delete', id })
        this.driveFiles = this.driveFiles.filter(f => f.id !== id)
        this.changed()
    }

    // Fail every download and upload still waiting, e.g. when the socket goes away.
    abort(): void {
        for (const resolve of this.pending.values()) {
            resolve(null)
        }
        this.pending.clear()
        for (const resolve of this.uploads.values()) {
            resolve(false)
        }
        this.uploads.clear()
    }
}

export function saveBlob(blob: Blob, name: string): void {
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = name
    document.body.appendChild(a)
    a.click()
    a.remove()
    setTimeout(() => URL.revokeObjectURL(url), 60_000)
}
