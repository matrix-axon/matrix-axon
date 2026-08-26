import { humanDuration, humanSize } from '../media/human-size'
import type { ParsedMedia } from '../media/parse-media'
import type { PreviewKind } from '../media/preview-kind'
import { useMediaBlob } from '../media/use-media-blob'
import { MediaCaption } from './MediaCaption'

/** The widest a tile gets, in CSS px — matches `MediaImage`'s thumbnail cap so
 *  a video and a photo in the same room line up. */
const TILE_MAX = 320

/**
 * The card for media that opens in the lightbox (ADR 0072): a video or a PDF,
 * shown as a tile you click into rather than a row of text with a button.
 *
 * The tile is a *placeholder*, not a rendered frame, and deliberately so. A
 * real poster would mean downloading the whole object just to scroll past it —
 * the cost ADR 0064 refused — and for encrypted media (the common case here)
 * the homeserver cannot generate one either, since it cannot decrypt the bytes.
 * When the sender did attach a thumbnail, that *is* shown; Element sends them
 * for video, ours does not yet (see the ADR's follow-up).
 */
export function MediaTile({
  accountId,
  media,
  kind,
  content,
  onOpen,
  children,
}: {
  accountId: string
  media: ParsedMedia
  /** Only the lightbox kinds reach here (`previewPresentation`); anything
   *  else falls back to the document treatment rather than failing. */
  kind: PreviewKind
  /** Event `content`, so a caption with `formatted_body` can use it. */
  content?: unknown
  onOpen: () => void
  /** The Download button — rendered into the tile's footer. */
  children?: preact.ComponentChildren
}) {
  // Lazy, not eager: this is decoration on a card that may never be looked at,
  // so it waits for the tile to near the viewport like any other thumbnail.
  const { ref, state } = useMediaBlob<HTMLDivElement>(
    accountId,
    media.thumbnailUrl,
    { thumbnail: { width: TILE_MAX, height: TILE_MAX, method: 'scale' } },
  )
  const poster =
    state.status === 'ready' && state.url !== undefined ? state.url : null

  const isVideo = kind === 'video'
  const duration = humanDuration(media.duration)
  const size = humanSize(media.size)
  const verb = isVideo ? 'Play' : 'Open'
  // A video reserves its own shape so the timeline does not jump when a poster
  // resolves; a PDF gets page-ish proportions.
  const aspect = !isVideo
    ? '4 / 3'
    : media.w !== undefined && media.h !== undefined
      ? `${media.w} / ${media.h}`
      : '16 / 9'

  return (
    <div class={`media-tile media-tile-${kind}`}>
      <button
        type="button"
        class="media-tile-surface"
        style={{ aspectRatio: aspect, width: `${TILE_MAX}px` }}
        aria-label={`${verb} ${media.filename}`}
        aria-haspopup="dialog"
        onClick={onOpen}
      >
        <div ref={ref} class="media-tile-poster">
          {poster !== null && <img src={poster} alt="" decoding="async" />}
        </div>
        <span class="media-tile-glyph" aria-hidden="true">
          {isVideo ? '▶' : '📄'}
        </span>
        {!isVideo && (
          <span class="media-tile-kind" aria-hidden="true">
            PDF
          </span>
        )}
        {duration !== null && (
          <span class="media-tile-duration" aria-hidden="true">
            {duration}
          </span>
        )}
      </button>
      <div class="media-tile-footer">
        <span class="media-tile-meta">
          <span class="media-attachment-name">{media.filename}</span>
          {media.caption !== null && (
            <span class="media-attachment-caption">
              <MediaCaption
                accountId={accountId}
                caption={media.caption}
                content={content}
              />
            </span>
          )}
          {size !== null && <span class="muted">{size}</span>}
        </span>
        {children}
      </div>
    </div>
  )
}
