import type { PreviewInput } from './contract'

export type DeliveryState =
  | 'ready'
  | 'uploading'
  | 'uploaded'
  | 'dispatching'
  | 'sent'
  | 'failed'
  | 'unknown'
export interface ShareOperation {
  id: string
  state: DeliveryState
  uploadId?: string
  eventId?: string
  failure?: string
}
export interface ShareFile {
  id: string
  name: string
  mime: string
  bytes: number
  digest: string
  thumbnail: boolean
  operation?: ShareOperation
  photoSize?: 'original' | 'large' | 'medium' | 'small'
  photoBytes?: number
  caption?: string
}
export interface SharePreview extends PreviewInput {
  provenance: 'source' | 'generated'
  thumbnail?: string
}
export interface IncomingShare {
  version: 1 | 2 | 3 | 4
  id: string
  createdAt: number
  session: string
  accountId?: string
  roomId?: string
  text: string
  previews?: SharePreview[]
  files: ShareFile[]
  operation: ShareOperation
  importFailures: string[]
  owner?: string
  leaseUntil?: number
  revision: number
}
export interface IncomingSharePort {
  call<T>(action: string, args?: Record<string, unknown>): Promise<T>
  /** Wake hints only. list on startup/resume remains authoritative. */
  subscribe(listener: () => void): () => void
}

/** Upgrade legacy combined first-photo/message delivery once, at the boundary. */
export function canonicalShare(record: IncomingShare): IncomingShare {
  if (record.version === 4) return record
  const first = record.files.find((file) => !file.thumbnail)?.operation
  const operation = { ...record.operation }
  if (
    record.text.trim() &&
    first &&
    ['sent', 'dispatching', 'unknown'].includes(first.state)
  ) {
    operation.state = first.state === 'dispatching' ? 'unknown' : first.state
    operation.eventId = first.eventId
    operation.failure = first.failure
  }
  return {
    ...record,
    version: 4,
    operation,
    files: record.files.map((file) =>
      file.thumbnail ? file : { ...file, caption: file.caption ?? '' },
    ),
  }
}
export function shareOperations(record: IncomingShare): ShareOperation[] {
  const current = canonicalShare(record)
  const photos = current.files.flatMap((file) =>
    file.operation ? [file.operation] : [],
  )
  return current.text.trim() || photos.length === 0
    ? [current.operation, ...photos]
    : photos
}
export function shareCompleted(record: IncomingShare): boolean {
  return shareOperations(record).every(
    (operation) => operation.state === 'sent',
  )
}
export function photoCaption(_record: IncomingShare, file: ShareFile): string {
  return file.caption ?? ''
}

/** Reject unsupported/corrupt records without acknowledging their contents. */
export function isIncomingShare(value: unknown): value is IncomingShare {
  if (typeof value !== 'object' || value === null) return false
  const record = value as Partial<IncomingShare>
  const operation = (value: unknown): value is ShareOperation => {
    if (typeof value !== 'object' || value === null) return false
    const item = value as Partial<ShareOperation>
    return (
      typeof item.id === 'string' &&
      typeof item.state === 'string' &&
      [
        'ready',
        'uploading',
        'uploaded',
        'dispatching',
        'sent',
        'failed',
        'unknown',
      ].includes(item.state)
    )
  }
  return (
    [1, 2, 3, 4].includes(record.version ?? 0) &&
    typeof record.id === 'string' &&
    typeof record.session === 'string' &&
    typeof record.text === 'string' &&
    record.text.length <= 64 * 1024 &&
    typeof record.createdAt === 'number' &&
    Number.isFinite(record.createdAt) &&
    Number.isSafeInteger(record.revision) &&
    operation(record.operation) &&
    Array.isArray(record.importFailures) &&
    record.importFailures.every((failure) => typeof failure === 'string') &&
    Array.isArray(record.files) &&
    record.files.length <= 26 &&
    record.files.every(
      (file) =>
        file !== null &&
        typeof file === 'object' &&
        typeof file.id === 'string' &&
        typeof file.name === 'string' &&
        typeof file.mime === 'string' &&
        typeof file.digest === 'string' &&
        Number.isSafeInteger(file.bytes) &&
        file.bytes >= 0 &&
        file.bytes <= 100 * 1024 * 1024 &&
        typeof file.thumbnail === 'boolean' &&
        ((record.version ?? 0) >= 2 ||
          (file.photoSize === undefined && file.photoBytes === undefined)) &&
        (file.photoSize === undefined ||
          ['original', 'large', 'medium', 'small'].includes(file.photoSize)) &&
        (file.photoBytes === undefined ||
          (Number.isSafeInteger(file.photoBytes) &&
            file.photoBytes >= 0 &&
            file.photoBytes <= 100 * 1024 * 1024)) &&
        (file.photoSize === undefined) === (file.photoBytes === undefined) &&
        (file.photoSize === undefined || file.mime.startsWith('image/')) &&
        (!file.thumbnail ||
          (file.photoSize === undefined && file.photoBytes === undefined)) &&
        (file.caption === undefined ||
          ((record.version ?? 0) >= 3 &&
            !file.thumbnail &&
            typeof file.caption === 'string' &&
            file.caption.length <= 64 * 1024)) &&
        (file.thumbnail || operation(file.operation)),
    ) &&
    (record.version !== 4 ||
      record.files.every(
        (file) => file.thumbnail || typeof file.caption === 'string',
      )) &&
    new TextEncoder().encode(record.text).length +
      record.files.reduce(
        (bytes, file) =>
          bytes + new TextEncoder().encode(file.caption ?? '').length,
        0,
      ) <=
      64 * 1024 &&
    (record.previews === undefined ||
      (Array.isArray(record.previews) &&
        record.previews.length <= 8 &&
        record.previews.every(
          (preview) =>
            preview !== null &&
            typeof preview === 'object' &&
            typeof preview.matched_url === 'string' &&
            ['source', 'generated'].includes(preview.provenance),
        )))
  )
}
