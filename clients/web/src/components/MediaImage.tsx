import { useEffect, useRef, useState } from 'preact/hooks'
import type { SniffedFormat } from '../media/media-service'
import type { ParsedMedia } from '../media/parse-media'
import { useMediaBlob } from '../media/use-media-blob'
import { imageDecodeFailureMessage } from '../media/image-format'
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
 * A ready blob is not necessarily a picture: a format this browser cannot
 * decode (HEIC, most often) arrives perfectly intact and still will not paint,
 * surfacing only at `<img>` decode, caught by `onError` (ADR 0101).
 *
 * But a third cause outnumbers them on iOS, and it is not about the bytes at
 * all: the same `onError` fires when WebKit fumbles a perfectly good image —
 * an object URL revoked out from under it, memory pressure, a decoder that
 * gave up. So the first failure is *not* a verdict. It re-fetches under a new
 * `attempt`, which mints a fresh object URL, and only a second failure paints
 * the placeholder — which then always offers Retry, because in a PWA there is
 * no reload and the alternative is force-quitting the app (issue #359).
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
  // Retry generation. Part of the media cache key, so bumping it re-fetches
  // and mints an object URL that is not the one that just failed to decode.
  const [attempt, setAttempt] = useState(0)
  // One automatic retry per object, then the reader decides. A ref, not state:
  // it must not itself cause a render, and it is read inside `onError` where a
  // stale closure over a state value would grant a second free retry.
  const autoRetried = useRef(false)
  // A null url makes the hook a no-op, so a local preview skips the proxy fetch
  // entirely while keeping the hook call unconditional.
  const { ref, state, invalidate } = useMediaBlob<HTMLDivElement>(
    accountId,
    previewUrl === undefined || previewUrl === null ? displayUrl : null,
    { thumbnail, attempt },
  )
  // Feed the load outcome back so the hook can fall back off a bad thumbnail.
  useEffect(() => {
    setStatus(state.status === 'error' ? 'error' : 'idle')
  }, [state.status])
  const [lightboxOpen, setLightboxOpen] = useState(false)
  // The decode verdict, bound to the object url it was reached on and to what
  // those bytes sniffed as.
  const [failure, setFailure] = useState<{
    url: string
    format?: SniffedFormat
  } | null>(null)
  const [downloading, setDownloading] = useState(false)
  const [downloadError, setDownloadError] = useState<string | null>(null)

  // Hold the verdict until a *different* url arrives. Clearing it on the retry
  // click instead would re-mount the url that just failed: `useMediaBlob` keeps
  // its previous `ready` state until its effect runs after paint, so the render
  // in between points the `<img>` at the failed blob again — and a browser
  // fires `error` for it before that effect lands, relatching the placeholder
  // and hiding the bytes the retry went and fetched.
  const decodeFailed =
    failure !== null && (state.status !== 'ready' || state.url === failure.url)

  // A different object means a fresh verdict — the same reset `LightboxImage`
  // does when the viewer pages. Without it a row recycled onto another event
  // (or falling back off a bad thumbnail) inherits the previous object's
  // failure and never tries.
  useEffect(() => {
    autoRetried.current = false
    setFailure(null)
    setAttempt(0)
  }, [displayUrl])

  const retry = () => {
    autoRetried.current = true
    setAttempt((previous) => previous + 1)
  }

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
  // Read the verdict off the *failed* fetch, so the message does not wobble to
  // the metadata fallback while a retry is in flight.
  const failedFormat = failure?.format
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
          ) : state.status === 'error' ? (
            // The retry's own fetch failed. Reported as itself rather than as
            // a decode verdict — and it carries Retry, or one decode glitch
            // followed by one network glitch would rebuild the dead end this
            // whole change exists to remove.
            <div class="media-undisplayable">
              <p class="muted placeholder">Could not load image</p>
              <div class="media-undisplayable-actions">
                <button type="button" class="ghost" onClick={retry}>
                  Retry
                </button>
              </div>
            </div>
          ) : decodeFailed ? (
            // Never a dead end: Retry, because on iOS the most likely cause is
            // transient and a PWA has no reload, and Download, because bytes
            // that arrived are worth opening elsewhere whatever they turned out
            // to be. ADR 0101 withheld Download for bytes it could not name,
            // on the grounds they were probably the proxy's ciphertext-fallback
            // 200 — a path that turned out to be near-unreachable (see
            // `image-format.ts`), so that gate only cost readers the one action
            // that helps.
            <div class="media-undisplayable">
              <p class="muted placeholder">
                {imageDecodeFailureMessage(media, failedFormat)}
              </p>
              <div class="media-undisplayable-actions">
                <button type="button" class="ghost" onClick={retry}>
                  Retry
                </button>
                {isDownloadable(media) && (
                  <button
                    type="button"
                    class="ghost"
                    disabled={downloading}
                    onClick={() => void saveUndisplayable()}
                  >
                    {downloading ? 'Downloading…' : 'Download'}
                  </button>
                )}
              </div>
              {downloadError !== null && (
                <p class="muted placeholder" role="alert">
                  {downloadError}
                </p>
              )}
            </div>
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
                onError={() => {
                  // These bytes did not decode, so drop them from the cache
                  // before anything else: entries outlive their holders, and a
                  // retry — or simply leaving the room and coming back — would
                  // otherwise be handed the very object that just failed,
                  // without a request leaving the browser.
                  invalidate()
                  // The first failure is not a verdict: re-fetch before
                  // believing the bytes are at fault. No `failure` is recorded
                  // on that path, so the reader never sees a placeholder flash
                  // for a glitch that recovered.
                  if (!autoRetried.current) {
                    retry()
                    return
                  }
                  if (state.url !== undefined) {
                    setFailure({ url: state.url, format: state.format })
                  }
                }}
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
