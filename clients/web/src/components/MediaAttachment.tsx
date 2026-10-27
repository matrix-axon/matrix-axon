import { useState } from 'preact/hooks'
import { humanSize } from '../media/human-size'
import type { ParsedMedia } from '../media/parse-media'
import { downloadMedia, isDownloadable } from '../media/download-media'
import { previewPlan } from '../media/preview-kind'
import { useServices } from '../services'
import { MediaCaption } from './MediaCaption'
import { MediaPreview } from './MediaPreview'
import { MediaTile } from './MediaTile'
import { MediaDownloadButton } from './MediaDownloadButton'
import { openMediaCard } from './media-card-click'

const ICON: Record<ParsedMedia['kind'], string> = {
  file: '📄',
  audio: '🎵',
  video: '🎞️',
  image: '🖼️',
  sticker: '🖼️',
}

/** Non-image attachment cards fetch only after an explicit preview/download. */
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
  const tiled = plan?.kind === 'video' || plan?.kind === 'pdf'
  const size = humanSize(media.size)
  const downloadable = isDownloadable(media)

  const open = () => {
    setError(null)
    setExpanded(true)
  }

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
    <MediaDownloadButton
      className="media-attachment-download"
      label={`Download ${media.filename}`}
      downloading={downloading}
      onClick={() => void download()}
    />
  ) : null

  return (
    <div class="media-attachment-card">
      {tiled && plan !== null ? (
        // Video and PDF get a tile they click into, not a row with a verb:
        // the thing you want is the media, so the media is the affordance.
        <MediaTile
          accountId={accountId}
          media={media}
          kind={plan.kind}
          content={content}
          onOpen={open}
        >
          {downloadButton}
        </MediaTile>
      ) : (
        <div
          class={`media-attachment${plan !== null ? ' media-attachment-previewable' : ''}`}
          data-media-preview-card={plan !== null ? '' : undefined}
          onClick={
            plan === null ? undefined : (event) => openMediaCard(event, open)
          }
        >
          <div class="media-attachment-heading">
            <span class="media-attachment-icon" aria-hidden="true">
              {ICON[media.kind]}
            </span>
            {plan === null ? (
              <span class="media-attachment-name" title={media.filename}>
                {media.filename}
              </span>
            ) : (
              <button
                type="button"
                class="media-attachment-name media-attachment-open"
                title={media.filename}
                aria-description={`${plan.kind === 'audio' ? 'Play' : 'Preview'} attachment`}
                aria-haspopup="dialog"
              >
                {media.filename}
              </button>
            )}
            {downloadButton}
          </div>
          {media.caption !== null && (
            <div class="media-attachment-caption">
              <MediaCaption
                accountId={accountId}
                caption={media.caption}
                content={content}
              />
            </div>
          )}
          {size !== null && <span class="muted">{size}</span>}
          {!expanded && error !== null && (
            <span class="local-echo-status error" role="alert">
              {error}
            </span>
          )}
        </div>
      )}
      {tiled && !expanded && error !== null && (
        <span class="local-echo-status error">{error}</span>
      )}
      {plan !== null && expanded && (
        <MediaPreview
          accountId={accountId}
          media={media}
          plan={plan}
          content={content}
          onClose={() => {
            setError(null)
            setExpanded(false)
          }}
          onDownload={downloadable ? () => void download() : undefined}
          downloading={downloading}
          downloadError={error}
        />
      )}
    </div>
  )
}
