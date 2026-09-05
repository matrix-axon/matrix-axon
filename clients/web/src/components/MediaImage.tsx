import { useEffect, useState } from 'preact/hooks'
import type { ParsedMedia } from '../media/parse-media'
import { useMediaBlob } from '../media/use-media-blob'
import {
  imageDecodeFailureMessage,
  unrenderableImageFormat,
} from '../media/image-format'
import { downloadMedia, isDownloadable } from '../media/download-media'
import { useServices } from '../services'
import {
  useThumbnailFallback,
  THUMBNAIL_MAX,
} from '../media/use-thumbnail-fallback'
import { useMediaViewer } from '../media/media-viewer'
import { Lightbox, LightboxImage } from './Lightbox'
import { MediaCaption } from './MediaCaption'

/** Height held for an image whose event carries no dimensions, in CSS px. */
const UNSIZED_MIN = 180

/**
 * The displayed width for an image of intrinsic `w`×`h`, scaled down so its
 * longest side is at most `THUMBNAIL_MAX`. Never upscales, so a small image
 * keeps its natural size.
 */
function thumbnailWidth(w: number, h: number): number {
  const scale = Math.min(1, THUMBNAIL_MAX / w, THUMBNAIL_MAX / h)
  return Math.round(w * scale)
}

/**
 * An inline image or sticker (ADR 0064). Shows the sender-embedded thumbnail
 * when present, else a homeserver-generated thumbnail for plaintext media,
 * else the full-size image; a click opens the full-size `Lightbox`. The
 * wrapper reserves the image's aspect ratio *before* the blob resolves — the
 * timeline is not re-anchored after mount, so an image that grew on load would
 * shove scrolled-back content around.
 *
 * A ready blob is not necessarily a picture. The media proxy returns raw
 * ciphertext with a 200 when it lacks the decryption key, and a format this
 * browser cannot decode (HEIC, most often) arrives perfectly intact and still
 * will not paint. Both surface only at `<img>` decode, caught by `onError`;
 * `imageDecodeFailureMessage` decides which of them to report, and the
 * placeholder offers Download either way so the bytes are never a dead end
 * (ADR 0101).
 */
export function MediaImage({
  accountId,
  media,
  previewUrl,
  eventId,
  content,
}: {
  accountId: string
  media: ParsedMedia
  /**
   * A local object url for an image still uploading (ADR 0065) — rendered
   * directly, since there is no mxc to fetch yet. Its presence is what makes
   * this component the *only* one that needs to know a send can be in flight.
   */
  previewUrl?: string | null
  /**
   * Opens the surrounding surface's shared, pageable viewer instead of this
   * component's own single-image lightbox (ADR 0081). Without it — or outside
   * a `MediaViewerProvider` — behaviour is exactly as before, which is what
   * `MediaPreview` and search results depend on.
   */
  eventId?: string
  /** Event `content`, so a caption with `formatted_body` can use it. */
  content?: unknown
}) {
  const viewer = useMediaViewer()
  const { media: service } = useServices()
  const [status, setStatus] = useState<'idle' | 'error'>('idle')
  const { displayUrl, thumbnail } = useThumbnailFallback(media, status)
  // A null url makes the hook a no-op, so a local preview skips the proxy fetch
  // entirely while keeping the hook call unconditional.
  const { ref, state } = useMediaBlob<HTMLDivElement>(
    accountId,
    previewUrl === undefined || previewUrl === null ? displayUrl : null,
    { thumbnail },
  )
  // Feed the load outcome back so the hook can fall back off a bad thumbnail.
  useEffect(() => {
    setStatus(state.status === 'error' ? 'error' : 'idle')
  }, [state.status])
  const [lightboxOpen, setLightboxOpen] = useState(false)
  const [decodeFailed, setDecodeFailed] = useState(false)
  const [downloading, setDownloading] = useState(false)
  const [downloadError, setDownloadError] = useState<string | null>(null)

  const saveUndisplayable = async () => {
    setDownloading(true)
    setDownloadError(null)
    const outcome = await downloadMedia(service, accountId, media)
    setDownloading(false)
    if (outcome === 'failed') {
      setDownloadError('Download failed')
    }
  }

  const hasDimensions = media.w !== undefined && media.h !== undefined
  // Cap the inline thumbnail to a modest box (never upscaling), so a large
  // photo renders small; `max-width: 100%` still lets it shrink on a narrow
  // pane while `aspect-ratio` keeps its shape and reserves scroll space.
  const boxStyle = hasDimensions
    ? {
        aspectRatio: `${media.w} / ${media.h}`,
        width: `${thumbnailWidth(media.w!, media.h!)}px`,
        maxWidth: '100%',
      }
    : {
        width: `${THUMBNAIL_MAX}px`,
        maxWidth: '100%',
        maxHeight: `${THUMBNAIL_MAX}px`,
        // No `w`/`h` on the event — older bridges, some clients, stickers — so
        // there is no ratio to hold. Reserve a plausible box anyway until the
        // bytes arrive: an unsized image otherwise occupies no height at all
        // and then snaps to full size on decode, shoving the timeline mid
        // scroll. Released once the image is up, so a short image does not sit
        // in a tall empty frame; the residual shift is from the reservation to
        // the real height, not from zero.
        ...(state.status === 'ready' ? {} : { minHeight: `${UNSIZED_MIN}px` }),
      }

  const alt = media.caption ?? media.filename
  /**
   * Whether offering to save the undisplayable bytes helps. Identical to the
   * pageable viewer's `saveable` gate, and identical for the same reason: a
   * named format is a real file another application will open, whereas bytes
   * we cannot identify are most likely the ciphertext-fallback 200, and
   * writing those to `photo.jpg` is worse than offering nothing (ADR 0101).
   */
  const saveable = unrenderableImageFormat(media) !== null
  const canOpen =
    state.status === 'ready' && !decodeFailed && media.url !== null

  const figure = (
    <figure class="media-figure">
      <div ref={ref} class="media-image" style={boxStyle}>
        <div
          class={`media-thumbnail${hasDimensions ? '' : ' media-thumbnail-unsized'}`}
        >
          {previewUrl !== undefined && previewUrl !== null ? (
            // Still uploading: the local file, not the proxy. No open/lightbox —
            // there is nothing on the server to open yet.
            <img
              class="media-preview"
              src={previewUrl}
              alt={alt}
              decoding="async"
            />
          ) : decodeFailed ? (
            // Not a dead end when we can name the format: those bytes are a
            // real file a local tool will open, so the placeholder carries the
            // same Download the attachment card would have offered. Withheld
            // for unidentifiable bytes — see `saveable`.
            <div class="media-undisplayable">
              <p class="muted placeholder">
                {imageDecodeFailureMessage(media)}
              </p>
              {saveable && isDownloadable(media) && (
                <button
                  type="button"
                  class="ghost"
                  disabled={downloading}
                  onClick={() => void saveUndisplayable()}
                >
                  {downloading ? 'Downloading…' : 'Download'}
                </button>
              )}
              {downloadError !== null && (
                <p class="muted placeholder" role="alert">
                  {downloadError}
                </p>
              )}
            </div>
          ) : state.status === 'error' ? (
            <p class="muted placeholder">Could not load image</p>
          ) : state.status === 'ready' && state.url !== undefined ? (
            <button
              type="button"
              class="media-open"
              aria-label={`Open ${media.kind}: ${media.filename}`}
              disabled={!canOpen}
              onClick={() => {
                if (viewer !== null && eventId !== undefined) {
                  viewer.open(eventId)
                } else {
                  setLightboxOpen(true)
                }
              }}
            >
              <img
                src={state.url}
                alt={alt}
                // Keep decode off the main thread: a photo decoding inline is
                // a stutter in the middle of a scroll gesture on a phone.
                decoding="async"
                onError={() => setDecodeFailed(true)}
              />
            </button>
          ) : (
            <div class="media-skeleton" aria-hidden="true" />
          )}
        </div>
      </div>
      {media.caption !== null && (
        <figcaption class="media-caption">
          <MediaCaption
            accountId={accountId}
            caption={media.caption}
            content={content}
          />
        </figcaption>
      )}
    </figure>
  )

  return (
    <>
      {figure}
      {lightboxOpen && media.url !== null && (
        <Lightbox
          label={alt}
          caption={
            media.caption === null ? null : (
              <MediaCaption
                accountId={accountId}
                caption={media.caption}
                content={content}
              />
            )
          }
          onClose={() => setLightboxOpen(false)}
        >
          <LightboxImage accountId={accountId} media={media} />
        </Lightbox>
      )}
    </>
  )
}
