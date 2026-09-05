import { useState } from 'preact/hooks'
import { humanSize } from '../media/human-size'
import type { ParsedMedia } from '../media/parse-media'
import { downloadMedia, isDownloadable } from '../media/download-media'
import {
  previewPlan,
  previewPresentation,
  type PreviewKind,
} from '../media/preview-kind'
import { useServices } from '../services'
import { MediaCaption } from './MediaCaption'
import { MediaPreview } from './MediaPreview'
import { MediaTile } from './MediaTile'

const ICON: Record<ParsedMedia['kind'], string> = {
  file: '📄',
  audio: '🎵',
  video: '🎞️',
  image: '🖼️',
  sticker: '🖼️',
}

/** The verb the expand button offers, per previewable kind. Only `audio` and
 *  `text` ever reach this button — `pdf` and `video` are always `framed`
 *  (`previewPresentation`), so they render `MediaTile` instead (below). */
const PREVIEW_VERB: Record<Extract<PreviewKind, 'audio' | 'text'>, string> = {
  audio: 'Play',
  text: 'Preview',
}

/**
 * A non-image attachment (file / audio / video) as a download card (ADR 0064).
 * Still nothing is fetched by scrolling past: ADR 0064's rule was that a native
 * player must not buffer the whole object *unasked*, and that holds — bytes
 * move only when the user clicks Download, or Play/Preview on the kinds
 * `previewPlan()` admits (ADR 0072), which mounts `MediaPreview` inline below
 * the card. The transient anchor is the sanctioned download path
 * (`window.open` is forbidden repo-wide for the Tauri shell, M-W12).
 */
export function MediaAttachment({
  accountId,
  media,
  content,
}: {
  accountId: string
  media: ParsedMedia
  /** Event `content`, so a caption with `formatted_body` can use it. */
  content?: unknown
}) {
  const { media: service, platform } = useServices()
  const [downloading, setDownloading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState(false)

  const plan = previewPlan(media)
  // A lightbox preview is not a disclosure: it takes over the viewport and is
  // dismissed from there, so the button opens it and never reads "Hide".
  const framed = plan !== null && previewPresentation(plan.kind) === 'lightbox'
  const size = humanSize(media.size)
  const downloadable = isDownloadable(media)

  const download = async () => {
    setDownloading(true)
    setError(null)
    const outcome = await downloadMedia(service, accountId, media, platform)
    setDownloading(false)
    if (outcome === 'failed') {
      setError('Download failed')
    }
  }

  const downloadButton = downloadable ? (
    <button
      type="button"
      class="ghost media-attachment-download"
      disabled={downloading}
      onClick={() => void download()}
    >
      {downloading ? 'Downloading…' : 'Download'}
    </button>
  ) : null

  return (
    <div class="media-attachment-card">
      {framed && plan !== null ? (
        // Video and PDF get a tile they click into, not a row with a verb:
        // the thing you want is the media, so the media is the affordance.
        <MediaTile
          accountId={accountId}
          media={media}
          kind={plan.kind}
          content={content}
          onOpen={() => setExpanded(true)}
        >
          {downloadButton}
        </MediaTile>
      ) : (
        <div class="media-attachment">
          <span class="media-attachment-icon" aria-hidden="true">
            {ICON[media.kind]}
          </span>
          <span class="media-attachment-meta">
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
            {error !== null && (
              <span class="local-echo-status error">{error}</span>
            )}
          </span>
          {plan !== null && (
            <button
              type="button"
              class="ghost media-attachment-preview"
              aria-expanded={expanded}
              onClick={() => setExpanded(!expanded)}
            >
              {expanded
                ? 'Hide'
                : // Reachable only for 'audio' | 'text' — this branch is the
                  // `!framed` one, and `previewPresentation` sends every other
                  // kind to `MediaTile` above.
                  PREVIEW_VERB[
                    plan.kind as Extract<PreviewKind, 'audio' | 'text'>
                  ]}
            </button>
          )}
          {downloadButton}
        </div>
      )}
      {framed && error !== null && (
        <span class="local-echo-status error">{error}</span>
      )}
      {plan !== null && expanded && (
        <MediaPreview
          accountId={accountId}
          media={media}
          plan={plan}
          content={content}
          onClose={framed ? () => setExpanded(false) : undefined}
        />
      )}
    </div>
  )
}
