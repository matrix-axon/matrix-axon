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
  version: 1 | 2 | 3
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

export function shareOperations(record: IncomingShare): ShareOperation[] {
  const files = record.files.flatMap((file) =>
    file.operation === undefined ? [] : [file.operation],
  )
  return files.length === 0 ? [record.operation] : files
}

export function shareCompleted(record: IncomingShare): boolean {
  return shareOperations(record).every(
    (operation) => operation.state === 'sent',
  )
}

export function photoCaption(record: IncomingShare, file: ShareFile): string {
  return record.files.find((candidate) => !candidate.thumbnail)?.id === file.id
    ? record.text
    : (file.caption ?? '')
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
    [1, 2, 3].includes(record.version ?? 0) &&
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
          (record.version === 3 &&
            !file.thumbnail &&
            typeof file.caption === 'string' &&
            file.caption.length <= 64 * 1024)) &&
        (file.thumbnail || operation(file.operation)),
    ) &&
    record.files.find((file) => !file.thumbnail)?.caption === undefined &&
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
