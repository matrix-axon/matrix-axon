import { apiErrorMessage } from '../api/client'
import type { AuthProvider } from '../auth/provider'
import { browserPlatform, type Platform } from '../platform'
import { mxcToPath } from './parse-media'

/**
 * The media layer (ADR 0064, M-W8).
 *
 * A browser cannot put an `Authorization` header on `<img src>`, and every
 * `/v1` route — the media proxy included — is bearer-guarded. So media cannot
 * be loaded by pointing an `<img>` at the server: each object must be
 * `fetch()`ed with the token, turned into a `Blob`, and handed to the DOM as
 * an `URL.createObjectURL()` object URL. This service owns that lifecycle.
 *
 * The hazard is revocation. The timeline is *not* windowed — every loaded
 * event stays mounted — so the set of live `<img>` elements grows without
 * bound as the user scrolls back. An object URL revoked while an `<img>` still
 * points at it paints a broken image silently. So the cache **refcounts**:
 * `acquire()` hands back a handle whose `release()` the caller must call on
 * unmount, an entry with `refs > 0` is never revoked, and an LRU governs only
 * the zero-ref remainder as a memory ceiling.
 */

export type MediaResult =
  { ok: true; url: string } | { ok: false; error: MediaFailure }

/** The outcome of fetching an object's raw bytes (ADR 0072). */
export type BlobResult =
  { ok: true; blob: Blob } | { ok: false; error: MediaFailure }

/** The outcome of decoding an object as text (ADR 0072). */
export type TextResult =
  { ok: true; text: string } | { ok: false; error: MediaFailure }

export interface MediaFailure {
  kind: 'auth' | 'not_found' | 'too_large' | 'network' | 'rejected'
  status?: number
  /** The server's envelope message, when it sent one (`rejected` carries it). */
  message?: string
}

/** The outcome of staging bytes for a later send (ADR 0065). */
export type UploadResult =
  { ok: true; uploadId: string } | { ok: false; error: MediaFailure }

/** The Matrix media kind a `File` will be staged as (ADR 0059's `kind` enum). */
export type UploadKind = 'image' | 'file'

export interface MediaHandle {
  result: MediaResult
  /** Drop this holder's reference. Idempotent; safe on a failed result. */
  release(): void
}

export type ThumbnailMethod = 'scale' | 'crop'

export interface ThumbnailRequest {
  width: number
  height: number
  method?: ThumbnailMethod
}

export interface MediaRequestOptions {
  thumbnail?: ThumbnailRequest
  /**
   * Re-type the fetched bytes before the object URL is minted (ADR 0072). The
   * proxy downgrades everything that is not image/audio/video to
   * `application/octet-stream` (`is_inline_safe`, `routes/media.rs`), which no
   * browser will render in an `<iframe>` — so an inline PDF/text preview
   * restores the type from the event's own `info.mimetype`.
   *
   * This overrides the server's declared type, so it is only ever safe with a
   * value the caller derived from `previewPlan()`'s allowlist; an active type
   * (`text/html`, `image/svg+xml`) must never reach here.
   */
  contentType?: string
}

export interface MediaService {
  /**
   * Resolve an `mxc://` URI to an object URL, refcount-acquired. Concurrent
   * acquires of one URI share a single fetch and a single object URL. The
   * caller **must** call `release()` on the returned handle when it unmounts.
   */
  acquire(
    accountId: string,
    mxcUrl: string,
    options?: MediaRequestOptions,
  ): Promise<MediaHandle>
  /**
   * A one-shot download — the attachment card's Download button. Returns a
   * fresh object URL that the caller owns and must revoke; it is not cached or
   * refcounted, since a download is transient.
   */
  fetchBlobUrl(accountId: string, mxcUrl: string): Promise<MediaResult>
  /**
   * Resolve an `mxc://` URI to the raw `Blob`. Needed by the share sheet,
   * which takes a `File` and cannot be handed an object URL — and reading one
   * back through `fetch()` is neither free nor available under jsdom (see
   * `fetchText`, which exists for the same reason).
   */
  fetchBlob(accountId: string, mxcUrl: string): Promise<BlobResult>
  /**
   * Decode an object as text, truncated to `limit` characters — the inline
   * text preview (ADR 0072). Distinct from `acquire()` because a `<pre>` wants
   * a string, not an object URL, and round-tripping one through `fetch()` on a
   * `blob:` URL is neither free nor available under jsdom.
   */
  fetchText(
    accountId: string,
    mxcUrl: string,
    limit: number,
  ): Promise<TextResult>
  /**
   * Stage a file's bytes for a later `send-media` (ADR 0065, the first step of
   * ADR 0059's two-step upload). Returns the `upload_id` the send claims.
   */
  upload(accountId: string, file: File): Promise<UploadResult>
}

/** Zero-ref object URLs kept for instant re-display before revocation. */
const LRU_CAP = 32
/** Matches the browser's classic per-host connection budget. */
const MAX_CONCURRENT = 6
/**
 * Largest file the client will offer to stage, mirroring axon-tui's
 * `MAX_UPLOAD_BYTES` (`clients/tui/src/app.rs`). Checked before any request so
 * an oversized file fails instantly rather than after a long transfer; the
 * server's own `max_upload_bytes` is configurable and may be lower, so the 413
 * path stays live regardless.
 */
export const MAX_UPLOAD_BYTES = 100 * 1024 * 1024

/**
 * The `kind` a file stages as. Unlike axon-tui — which maps a filename
 * extension to a `(kind, content_type)` pair (`media_kind_and_content_type`),
 * because a path on disk is all it has — the browser hands us the platform's
 * own MIME determination, so `kind` is read straight off it. That keeps `kind`
 * and `Content-Type` consistent by construction, which is the invariant the
 * server polices: `kind=image` with a non-`image/*` type is a 400
 * (`axon-sync/src/gateway.rs`). A file whose type the browser could not
 * determine (`''`) stages as `file` with no `Content-Type` at all, which the
 * handler accepts.
 */
export function uploadKind(file: File): UploadKind {
  return file.type.startsWith('image/') ? 'image' : 'file'
}

/** User-facing text for a failed upload. */
export function uploadFailureMessage(failure: MediaFailure): string {
  switch (failure.kind) {
    // The server's own cap is configurable and may be lower than ours, so
    // prefer the message it sent over restating our client-side limit.
    case 'too_large':
      return (
        failure.message ??
        `File is too large to send (limit ${Math.floor(
          MAX_UPLOAD_BYTES / (1024 * 1024),
        )} MB)`
      )
    case 'auth':
      return 'Not signed in'
    case 'rejected':
      return failure.message ?? 'Server rejected the upload'
    default:
      return 'Upload failed'
  }
}

interface CacheEntry {
  url: string
  refs: number
}

/** A FIFO semaphore: at most `max` holders run, the rest wait in order. */
function semaphore(max: number) {
  let active = 0
  const waiters: (() => void)[] = []
  return {
    async run<T>(task: () => Promise<T>): Promise<T> {
      if (active >= max) {
        await new Promise<void>((resolve) => waiters.push(resolve))
      }
      active += 1
      try {
        return await task()
      } finally {
        active -= 1
        waiters.shift()?.()
      }
    },
  }
}

export function createMediaService(deps: {
  auth: AuthProvider
  baseUrl: string
  /**
   * Transport seam (ADR 0102 § 2). Media is the one path that does not go
   * through `openapi-fetch` — it wants bytes, not JSON — so it takes the
   * platform's `fetch` directly. Defaulted to the browser's.
   */
  platform?: Pick<Platform, 'fetch'>
}): MediaService {
  const root = deps.baseUrl.replace(/\/$/, '')
  const fetch = (deps.platform ?? browserPlatform()).fetch
  const gate = semaphore(MAX_CONCURRENT)

  // Refcounted object-URL cache. `refs > 0` entries are never revoked.
  const cache = new Map<string, CacheEntry>()
  // Zero-ref keys in release order (oldest first) — a JS Map preserves
  // insertion order, giving a cheap LRU over the evictable remainder.
  const zeroRef = new Map<string, true>()
  // In-flight fetches, so concurrent acquires of one key share one download.
  const inflight = new Map<string, Promise<MediaResult>>()

  const keyOf = (
    accountId: string,
    mxc: string,
    options?: MediaRequestOptions,
  ) => {
    const thumbnail = options?.thumbnail
    // A re-typed fetch yields a different blob than the raw one, so it needs
    // its own cache slot even though it hits the same proxy URL.
    const type = options?.contentType ?? ''
    if (thumbnail === undefined) {
      return `${accountId}\0full\0${type}\0${mxc}`
    }
    return `${accountId}\0thumb\0${thumbnail.width}x${thumbnail.height}\0${
      thumbnail.method ?? 'scale'
    }\0${type}\0${mxc}`
  }

  const mediaUrl = (
    accountId: string,
    mxcUrl: string,
    options?: MediaRequestOptions,
  ): string | null => {
    const path = mxcToPath(mxcUrl)
    if (path === null) {
      return null
    }
    const base = `${root}/v1/media/${accountId}/${encodeURIComponent(
      path.serverName,
    )}/${encodeURIComponent(path.mediaId)}`
    const thumbnail = options?.thumbnail
    if (thumbnail === undefined) {
      return base
    }
    const params = new URLSearchParams({
      width: String(thumbnail.width),
      height: String(thumbnail.height),
    })
    if (thumbnail.method !== undefined) {
      params.set('method', thumbnail.method)
    }
    return `${base}/thumbnail?${params.toString()}`
  }

  /** Fetch → Blob, without touching the cache. The shared core of every
   *  download: object URL, one-shot download, and text decode. */
  async function fetchBlob(
    accountId: string,
    mxcUrl: string,
    options?: MediaRequestOptions,
  ): Promise<BlobResult> {
    const url = mediaUrl(accountId, mxcUrl, options)
    if (url === null) {
      return { ok: false, error: { kind: 'network' } }
    }

    return gate.run(async () => {
      try {
        const token = await deps.auth.getToken()
        const headers = new Headers()
        if (token !== null) {
          headers.set('authorization', `Bearer ${token}`)
        }
        const res = await fetch(url, { headers })
        if (res.ok) {
          const raw = await res.blob()
          const blob =
            options?.contentType === undefined
              ? raw
              : new Blob([raw], { type: options.contentType })
          return { ok: true, blob }
        }
        if (res.status === 401) {
          // Let the provider drop the revoked token, exactly as the API client
          // does — the failure still flows back to the caller.
          deps.auth.onAuthFailure()
          return { ok: false, error: { kind: 'auth', status: 401 } }
        }
        if (res.status === 404) {
          return { ok: false, error: { kind: 'not_found', status: 404 } }
        }
        if (res.status === 413) {
          return { ok: false, error: { kind: 'too_large', status: 413 } }
        }
        return { ok: false, error: { kind: 'network', status: res.status } }
      } catch {
        return { ok: false, error: { kind: 'network' } }
      }
    })
  }

  /** Fetch → Blob → object URL, without touching the cache. */
  async function download(
    accountId: string,
    mxcUrl: string,
    options?: MediaRequestOptions,
  ): Promise<MediaResult> {
    const result = await fetchBlob(accountId, mxcUrl, options)
    return result.ok
      ? { ok: true, url: URL.createObjectURL(result.blob) }
      : result
  }

  async function fetchText(
    accountId: string,
    mxcUrl: string,
    limit: number,
  ): Promise<TextResult> {
    const result = await fetchBlob(accountId, mxcUrl)
    if (!result.ok) {
      return result
    }
    try {
      const text = await result.blob.text()
      return { ok: true, text: text.slice(0, limit) }
    } catch {
      return { ok: false, error: { kind: 'network' } }
    }
  }

  function evict(): void {
    for (const key of zeroRef.keys()) {
      if (zeroRef.size <= LRU_CAP) {
        break
      }
      zeroRef.delete(key)
      const entry = cache.get(key)
      if (entry !== undefined && entry.refs === 0) {
        URL.revokeObjectURL(entry.url)
        cache.delete(key)
      }
    }
  }

  function releaseKey(key: string): void {
    const entry = cache.get(key)
    if (entry === undefined || entry.refs === 0) {
      return
    }
    entry.refs -= 1
    if (entry.refs === 0) {
      zeroRef.set(key, true)
      evict()
    }
  }

  function handleFor(key: string, result: MediaResult): MediaHandle {
    if (!result.ok) {
      return { result, release() {} }
    }
    let released = false
    return {
      result,
      release() {
        if (released) {
          return
        }
        released = true
        releaseKey(key)
      },
    }
  }

  async function acquire(
    accountId: string,
    mxcUrl: string,
    options?: MediaRequestOptions,
  ): Promise<MediaHandle> {
    const key = keyOf(accountId, mxcUrl, options)

    const cached = cache.get(key)
    if (cached !== undefined) {
      cached.refs += 1
      zeroRef.delete(key)
      return handleFor(key, { ok: true, url: cached.url })
    }

    let pending = inflight.get(key)
    if (pending === undefined) {
      pending = (async () => {
        const result = await download(accountId, mxcUrl, options)
        if (result.ok) {
          // Seed a zero-ref entry; each awaiter registers its own ref below.
          cache.set(key, { url: result.url, refs: 0 })
        }
        return result
      })()
      inflight.set(key, pending)
      void pending.finally(() => inflight.delete(key))
    }

    const result = await pending
    if (!result.ok) {
      return handleFor(key, result)
    }
    // The shared entry exists (seeded above); take a ref against the live url.
    const entry = cache.get(key)
    if (entry === undefined) {
      // Evicted between seeding and here — impossible while refs stay 0 and
      // nothing else evicts, but re-download rather than hand back a dead url.
      return acquire(accountId, mxcUrl, options)
    }
    entry.refs += 1
    zeroRef.delete(key)
    return handleFor(key, { ok: true, url: entry.url })
  }

  function fetchBlobUrl(
    accountId: string,
    mxcUrl: string,
  ): Promise<MediaResult> {
    return download(accountId, mxcUrl)
  }

  /**
   * Stage upload bytes (ADR 0065). Hand-rolled `fetch` rather than the typed
   * client for the same reason `download()` is: openapi-fetch is JSON-oriented
   * and this body is raw `application/octet-stream`. A `File` *is* a `Blob`, so
   * it goes to `fetch` as the body directly — `fetch` then derives
   * `Content-Type` from `file.type`, and sends none when the type is empty,
   * which is exactly what the server wants.
   *
   * Deliberately *not* behind `gate`: that semaphore models the browser's
   * per-host budget for many small thumbnails, and a large upload parked in it
   * would stall every image on screen.
   */
  async function upload(accountId: string, file: File): Promise<UploadResult> {
    if (file.size > MAX_UPLOAD_BYTES) {
      return { ok: false, error: { kind: 'too_large' } }
    }
    const url =
      `${root}/v1/accounts/${encodeURIComponent(accountId)}/media/uploads` +
      `?kind=${uploadKind(file)}&filename=${encodeURIComponent(file.name)}`

    try {
      const token = await deps.auth.getToken()
      const headers = new Headers()
      if (token !== null) {
        headers.set('authorization', `Bearer ${token}`)
      }
      const res = await fetch(url, { method: 'POST', headers, body: file })

      if (res.ok) {
        const body = (await res.json()) as { data?: { upload_id?: unknown } }
        const uploadId = body.data?.upload_id
        if (typeof uploadId !== 'string') {
          return { ok: false, error: { kind: 'network', status: res.status } }
        }
        return { ok: true, uploadId }
      }

      // The failure body is the shared error envelope; reuse the one reader
      // rather than teaching this file a second one.
      const message = apiErrorMessage(await res.json().catch(() => null))
      if (res.status === 401) {
        // Drop the revoked token, exactly as `download()` and the API client's
        // middleware do — the failure still flows back to the caller.
        deps.auth.onAuthFailure()
        return { ok: false, error: { kind: 'auth', status: 401, message } }
      }
      if (res.status === 413) {
        return { ok: false, error: { kind: 'too_large', status: 413, message } }
      }
      if (res.status === 400 || res.status === 403 || res.status === 404) {
        return {
          ok: false,
          error: { kind: 'rejected', status: res.status, message },
        }
      }
      return { ok: false, error: { kind: 'network', status: res.status } }
    } catch {
      return { ok: false, error: { kind: 'network' } }
    }
  }

  return { acquire, fetchBlob, fetchBlobUrl, fetchText, upload }
}
