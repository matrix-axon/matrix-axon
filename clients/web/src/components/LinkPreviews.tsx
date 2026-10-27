import { useCallback, useEffect, useMemo, useState } from 'preact/hooks'
import type { MediaFailure } from '../media/media-service'
import {
  MAX_PREVIEW_IMAGE_BYTES,
  parseLinkPreviews,
  type LinkPreview,
  type LinkPreviewImage,
} from '../link-previews'
import { useMediaBlob } from '../media/use-media-blob'
import { useServices } from '../services'

let nextReader = 0

function PreviewImage({
  accountId,
  image,
}: {
  accountId: string
  image: LinkPreviewImage
}) {
  const { auth } = useServices()
  const credential = auth.peekToken()
  // Track the reactive credential without putting a token in a DOM/cache key.
  const reader = useMemo(() => ({ credential, id: ++nextReader }), [credential])
  return credential === null ? null : (
    <PreviewImageMedia
      key={`${reader.id}\0${accountId}\0${image.mxc}\0${image.encrypted}`}
      accountId={accountId}
      image={image}
    />
  )
}

type ImageAttempt = 'thumbnail' | 'original' | 'retry' | 'failed'
type ImageFailure = MediaFailure['kind'] | 'decode' | 'unsupported'

function PreviewImageMedia({
  accountId,
  image,
}: {
  accountId: string
  image: LinkPreviewImage
}) {
  const [attempt, setAttempt] = useState<ImageAttempt>(
    image.encrypted ? 'original' : 'thumbnail',
  )
  const fail = useCallback(
    (kind: ImageFailure) => {
      if (
        attempt === 'thumbnail' &&
        !['auth', 'undecryptable', 'too_large', 'unsupported'].includes(kind)
      ) {
        setAttempt('original')
      } else if (
        attempt === 'original' &&
        image.encrypted &&
        kind === 'decode'
      ) {
        setAttempt('retry')
      } else setAttempt('failed')
    },
    [attempt, image.encrypted],
  )
  return attempt === 'failed' ? (
    <span class="link-preview-image">
      <span class="muted">Preview image unavailable</span>
    </span>
  ) : (
    // The hook retains its old result until its effect runs. A keyed request
    // makes each fallback start idle, releasing the previous handle and never
    // interpreting a thumbnail's old error as an original's new failure.
    <PreviewImageRequest
      key={attempt}
      accountId={accountId}
      image={image}
      thumbnail={attempt === 'thumbnail'}
      fail={fail}
    />
  )
}

function PreviewImageRequest({
  accountId,
  image,
  thumbnail,
  fail,
}: {
  accountId: string
  image: LinkPreviewImage
  thumbnail: boolean
  fail: (kind: ImageFailure) => void
}) {
  const { ref, state, invalidate } = useMediaBlob<HTMLSpanElement>(
    accountId,
    image.mxc,
    {
      thumbnail: thumbnail
        ? { width: 480, height: 270, method: 'scale' }
        : undefined,
      maxBytes: MAX_PREVIEW_IMAGE_BYTES,
    },
  )
  const unsupported =
    state.status === 'ready' &&
    state.format !== undefined &&
    !['PNG', 'JPEG', 'WebP'].includes(state.format ?? '')
  useEffect(() => {
    if (unsupported) fail('unsupported')
    else if (state.status === 'error') fail(state.error?.kind ?? 'network')
  }, [unsupported, state.status, state.error?.kind, fail])
  return (
    <span ref={ref} class="link-preview-image">
      {state.status === 'ready' && !unsupported ? (
        <img
          src={state.url}
          alt=""
          decoding="async"
          onError={() => {
            invalidate()
            fail('decode')
          }}
        />
      ) : (
        <span class="muted">Loading preview image…</span>
      )}
    </span>
  )
}

/** Consumes the shared validated shape; pending-share UI can reuse this card. */
export function LinkPreviewCard({
  accountId,
  preview,
}: {
  accountId: string
  preview: LinkPreview
}) {
  return (
    <a
      class="link-preview-card"
      href={preview.url}
      target="_blank"
      rel="noopener noreferrer"
    >
      {preview.image !== null && (
        <PreviewImage accountId={accountId} image={preview.image} />
      )}
      <span class="link-preview-host">{preview.hostname}</span>
      {preview.siteName !== null && (
        <span class="link-preview-site">{preview.siteName}</span>
      )}
      {preview.title !== null && <strong>{preview.title}</strong>}
      {preview.description !== null && (
        <span class="link-preview-description">{preview.description}</span>
      )}
    </a>
  )
}

export function LinkPreviews({
  accountId,
  content,
  body,
}: {
  accountId: string
  content: unknown
  body: string | null | undefined
}) {
  const previews = useMemo(
    () => parseLinkPreviews(content, body),
    [content, body],
  )
  if (previews.length === 0) return null
  return (
    <div class="link-previews" aria-label="Link previews">
      {previews.slice(0, 2).map((preview) => (
        <LinkPreviewCard
          key={preview.url}
          accountId={accountId}
          preview={preview}
        />
      ))}
      {previews.length > 2 && (
        <details>
          <summary>Show {previews.length - 2} more previews</summary>
          {previews.slice(2).map((preview) => (
            <LinkPreviewCard
              key={preview.url}
              accountId={accountId}
              preview={preview}
            />
          ))}
        </details>
      )}
    </div>
  )
}
