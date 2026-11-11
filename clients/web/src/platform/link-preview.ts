import { invoke } from '@tauri-apps/api/core'
import {
  type DeviceLinkPreview,
  LinkPreviewError,
  type LinkPreviewFailure,
} from './index'
import { PREVIEW_FIELD_LIMITS } from '../preview-links'

/**
 * The page's side of the shell's link-preview fetcher (ADR 0132 § 3), the
 * in-repo plugin at `src-tauri/axon-link-preview/`.
 *
 * The plugin fetches a public HTTPS page and its preview image through a
 * restricted transport of its own: the origin's A records only, a socket to
 * the validated address, TLS verified against the link's host, at most two
 * lookups at a time and 25 seconds each. This binding only asks, cancels, and
 * checks the shape of what comes back.
 *
 * The plugin resolves with one frame: a big-endian `u32` length, that many
 * bytes of JSON ({@link FrameHead}), then the thumbnail's bytes. A failure
 * rejects with `{ kind }` and no free text, which becomes a
 * {@link LinkPreviewError}. An abort rejects with the signal's own reason at
 * once, without waiting for the plugin, which is told to stop.
 */

const FAILURES: ReadonlySet<string> = new Set<LinkPreviewFailure>([
  'unsupported_url',
  'public_ipv4_required',
  'timeout',
  'no_metadata',
  'unavailable',
  'busy',
  'cancelled',
])

/** The JSON half of a frame (`Head` in `axon-link-preview/src/lib.rs`). */
interface FrameHead {
  url: string
  title?: string
  description?: string
  siteName?: string
  image?: {
    mimeType: string
    width: number
    height: number
    byteLength: number
  }
  imageUnavailable: boolean
}

function invalid(): LinkPreviewError {
  return new LinkPreviewError('unavailable')
}

function optionalText(value: unknown, limit: number): string | undefined {
  if (value === undefined) {
    return undefined
  }
  if (typeof value !== 'string' || value.length === 0 || value.length > limit) {
    throw invalid()
  }
  return value
}

function dimension(value: unknown): number {
  if (
    !Number.isInteger(value) ||
    (value as number) < 1 ||
    (value as number) > 1200
  ) {
    throw invalid()
  }
  return value as number
}

/** Decode and check one frame. Exported for tests. */
export function decodeFrame(frame: ArrayBuffer): DeviceLinkPreview {
  const bytes = new Uint8Array(frame)
  if (bytes.length < 4) {
    throw invalid()
  }
  const length = new DataView(frame).getUint32(0)
  if (length > bytes.length - 4) {
    throw invalid()
  }
  let head: FrameHead
  try {
    head = JSON.parse(
      new TextDecoder('utf-8', { fatal: true }).decode(
        bytes.subarray(4, 4 + length),
      ),
    ) as FrameHead
  } catch {
    throw invalid()
  }
  if (
    typeof head !== 'object' ||
    head === null ||
    typeof head.url !== 'string' ||
    typeof head.imageUnavailable !== 'boolean'
  ) {
    throw invalid()
  }
  const rest = bytes.subarray(4 + length)
  const preview: DeviceLinkPreview = {
    url: head.url,
    imageUnavailable: head.imageUnavailable,
  }
  const title = optionalText(head.title, PREVIEW_FIELD_LIMITS.title)
  const description = optionalText(
    head.description,
    PREVIEW_FIELD_LIMITS.description,
  )
  const siteName = optionalText(head.siteName, PREVIEW_FIELD_LIMITS.siteName)
  if (title !== undefined) preview.title = title
  if (description !== undefined) preview.description = description
  if (siteName !== undefined) preview.siteName = siteName
  if (head.image === undefined) {
    if (rest.length !== 0) {
      throw invalid()
    }
  } else {
    const { mimeType, byteLength } = head.image
    if (
      (mimeType !== 'image/jpeg' && mimeType !== 'image/png') ||
      byteLength !== rest.length ||
      byteLength === 0
    ) {
      throw invalid()
    }
    preview.image = {
      // A copy, so the frame's buffer is not kept alive by the card.
      bytes: rest.slice(),
      mimeType,
      width: dimension(head.image.width),
      height: dimension(head.image.height),
    }
  }
  return preview
}

function failure(error: unknown): LinkPreviewError {
  const kind =
    typeof error === 'object' && error !== null
      ? (error as { kind?: unknown }).kind
      : undefined
  return new LinkPreviewError(
    typeof kind === 'string' && FAILURES.has(kind)
      ? (kind as LinkPreviewFailure)
      : 'unavailable',
  )
}

/**
 * Lookup ids, unique for the life of this page. The plugin keys lookups by
 * webview and id and forgets a webview's lookups when a page starts loading,
 * so a fresh page may start again from 1.
 */
let nextId = 1

/** Build a preview of `url` on this device. */
export function shellLinkPreview(
  url: string,
  signal?: AbortSignal,
): Promise<DeviceLinkPreview> {
  if (signal?.aborted) {
    return Promise.reject(signal.reason as Error)
  }
  const id = nextId++
  return new Promise<DeviceLinkPreview>((resolve, reject) => {
    let settled = false
    const onAbort = () => {
      if (settled) return
      settled = true
      // The plugin stops at its next check; nothing waits for that.
      invoke('plugin:axon-link-preview|cancel', { id }).catch(() => {})
      reject(signal!.reason as Error)
    }
    signal?.addEventListener('abort', onAbort, { once: true })
    invoke<ArrayBuffer>('plugin:axon-link-preview|lookup', { id, url }).then(
      (frame) => {
        if (settled) return
        settled = true
        signal?.removeEventListener('abort', onAbort)
        try {
          resolve(decodeFrame(frame))
        } catch (error) {
          reject(error as Error)
        }
      },
      (error: unknown) => {
        if (settled) return
        settled = true
        signal?.removeEventListener('abort', onAbort)
        reject(failure(error))
      },
    )
  })
}
