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
}
export interface SharePreview extends PreviewInput {
  provenance: 'source' | 'generated'
  thumbnail?: string
}
export interface IncomingShare {
  version: 1
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
    record.version === 1 &&
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
        (file.thumbnail || operation(file.operation)),
    ) &&
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
