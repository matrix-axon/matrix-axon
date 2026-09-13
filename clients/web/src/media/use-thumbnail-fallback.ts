import { useEffect, useState } from 'preact/hooks'
import type { ParsedMedia } from './parse-media'

/** The longest side an inline thumbnail is allowed, in CSS px (Element-ish). */
export const THUMBNAIL_MAX = 320

export interface ThumbnailRequest {
  width: number
  height: number
  method: 'scale' | 'crop'
}

/**
 * Which object to display for a piece of media, and how to recover when the
 * thumbnail is no good.
 *
 * Three cases, in order of preference: a server-generated thumbnail, which
 * exists only for plaintext media because `resolve_thumbnail_spec` rejects
 * encrypted objects outright; a sender-embedded thumbnail; or the full-size
 * image. The server's comes first even when the sender attached one, because
 * Synapse applies the original's EXIF orientation when it thumbnails, while a
 * sender's thumbnail can arrive with raw sideways pixels and the tag stripped
 * — bridged iPhone photos do exactly this, and no `<img>` can recover it. An
 * encrypted attachment (`content.file`) keeps the sender's, the only one that
 * can exist; note an E2EE room's events can still carry plaintext attachments
 * (`content.url`), and those get the server's. The subtlety worth sharing
 * rather than forking is the
 * recovery — a broken thumbnail (malformed or purged sender thumbnail, a
 * homeserver that cannot generate one) must not hide an image whose full-size
 * object would load fine, so an error on the thumbnail falls back to it
 * instead of showing the error placeholder.
 *
 * `method` decides the server request shape: `scale` for an inline image that
 * keeps its aspect ratio, `crop` for a square gallery cell. `keyOf` in
 * `media-service` includes the method, so the two never collide in the cache.
 */
export function useThumbnailFallback(
  media: ParsedMedia,
  status: 'idle' | 'loading' | 'ready' | 'error',
  options: { size?: number; method?: 'scale' | 'crop' } = {},
): { displayUrl: string | null; thumbnail: ThumbnailRequest | undefined } {
  const { size = THUMBNAIL_MAX, method = 'scale' } = options
  const [thumbnailFailed, setThumbnailFailed] = useState(false)

  // Only plaintext media can be thumbnailed by the server; for an encrypted
  // object the sender's embedded thumbnail is the only one that can exist.
  const generatedThumbnailUrl = media.encrypted ? null : media.url
  const thumbnailUrl = generatedThumbnailUrl ?? media.thumbnailUrl
  const usingThumbnail = !thumbnailFailed && thumbnailUrl !== null

  useEffect(() => {
    if (
      status === 'error' &&
      !thumbnailFailed &&
      usingThumbnail &&
      media.url !== null
    ) {
      setThumbnailFailed(true)
    }
  }, [status, thumbnailFailed, usingThumbnail, media.url])

  // A different object means a fresh verdict.
  useEffect(() => {
    setThumbnailFailed(false)
  }, [media.url, media.thumbnailUrl])

  return {
    displayUrl: usingThumbnail ? thumbnailUrl : media.url,
    thumbnail:
      usingThumbnail && generatedThumbnailUrl !== null
        ? { width: size, height: size, method }
        : undefined,
  }
}
