import { effect } from '@preact/signals'
import type { ComponentChildren } from 'preact'
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

function PreviewImage({
  accountId,
  image,
  preview,
}: {
  accountId: string
  image: LinkPreviewImage
  preview: LinkPreview
}) {
  const { auth } = useServices()
  return !auth.signedIn.value ? (
    <PreviewLink preview={preview} />
  ) : (
    <PreviewImageMedia
      key={`${auth.sessionId.value}\0${accountId}\0${image.mxc}\0${image.encrypted}`}
      accountId={accountId}
      image={image}
      preview={preview}
    />
  )
}

type ImageAttempt = 'thumbnail' | 'original' | 'retry'
type ImageFailure = MediaFailure['kind'] | 'decode' | 'unsupported'

function PreviewImageMedia({
  accountId,
  image,
  preview,
}: {
  accountId: string
  image: LinkPreviewImage
  preview: LinkPreview
}) {
  const { auth, live } = useServices()
  const initial = image.encrypted ? 'original' : 'thumbnail'
  const [attempt, setAttempt] = useState<ImageAttempt>(initial)
  const [failure, setFailure] = useState<{
    kind: ImageFailure
    reconnect: number
  } | null>(null)
  const retryable = failure?.kind === 'auth' || failure?.kind === 'network'
  const retry = useCallback(() => {
    setAttempt(initial)
    setFailure(null)
  }, [initial])
  const fail = useCallback(
    (kind: ImageFailure) => {
      if (
        attempt === 'thumbnail' &&
        (kind === 'not_found' || kind === 'decode')
      ) {
        setAttempt('original')
      } else if (
        attempt === 'original' &&
        image.encrypted &&
        kind === 'decode'
      ) {
        setAttempt('retry')
      } else setFailure({ kind, reconnect: live.reconnects.peek() })
    },
    [attempt, image.encrypted, live],
  )
  useEffect(
    () =>
      effect(() => {
        const reconnect = live.reconnects.value
        if (retryable && failure !== null && reconnect > failure.reconnect)
          retry()
      }),
    [live, retryable, failure, retry],
  )
  useEffect(
    () =>
      auth.onAccessTokenRefreshed(() => {
        if (failure?.kind === 'auth') retry()
      }),
    [auth, failure, retry],
  )
  return (
    <div class="link-preview">
      <PreviewLink preview={preview}>
        {failure !== null ? (
          <span class="link-preview-image">
            <span class="muted">Preview image unavailable</span>
          </span>
        ) : (
          // Each fallback starts idle and releases the old handle. The hook's
          // previous result cannot masquerade as this request's new failure.
          <PreviewImageRequest
            key={attempt}
            accountId={accountId}
            image={image}
            thumbnail={attempt === 'thumbnail'}
            fail={fail}
          />
        )}
      </PreviewLink>
      {retryable && (
        <button type="button" class="link-preview-retry" onClick={retry}>
          Retry image
        </button>
      )}
    </div>
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
  return preview.image === null ? (
    <PreviewLink preview={preview} />
  ) : (
    <PreviewImage
      accountId={accountId}
      image={preview.image}
      preview={preview}
    />
  )
}

function PreviewLink({
  preview,
  children,
}: {
  preview: LinkPreview
  children?: ComponentChildren
}) {
  return (
    <a
      class="link-preview-card"
      href={preview.url}
      target="_blank"
      rel="noopener noreferrer"
    >
      {children}
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
