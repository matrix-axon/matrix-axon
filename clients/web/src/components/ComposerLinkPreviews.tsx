import type { ComponentChildren } from 'preact'
import { useEffect, useState } from 'preact/hooks'
import {
  previewFailureMessage,
  type ComposerPreviewCard,
} from '../composer-link-previews'
import type { PreviewThumbnail } from '../link-preview-send'
import { LinkPreviewText } from './LinkPreviews'

/** An object url for a card's thumbnail bytes, revoked with the card. */
function useThumbnailUrl(image: PreviewThumbnail | null): string | null {
  const [url, setUrl] = useState<string | null>(null)
  useEffect(() => {
    if (image === null) {
      setUrl(null)
      return
    }
    const next = URL.createObjectURL(
      new Blob([image.bytes], { type: image.mimeType }),
    )
    setUrl(next)
    return () => URL.revokeObjectURL(next)
  }, [image])
  return url
}

function CardBody({
  card,
  children,
}: {
  card: ComposerPreviewCard
  /** The card's action, below its text. */
  children?: ComponentChildren
}) {
  const { state } = card
  const ready = state.status === 'ready' ? state.result : null
  const thumbnail = useThumbnailUrl(ready?.image ?? null)
  if (ready !== null)
    return (
      <>
        {thumbnail !== null && (
          <img class="composer-link-preview-thumb" src={thumbnail} alt="" />
        )}
        <span class="composer-link-preview-text">
          <LinkPreviewText
            preview={{
              hostname: card.hostname,
              siteName: ready.siteName,
              title: ready.title,
              description: ready.description,
            }}
          />
          {ready.imageLost && (
            <span class="muted">Preview image unavailable</span>
          )}
          {children}
        </span>
      </>
    )
  return (
    <span class="composer-link-preview-text">
      <span class="link-preview-host">{card.hostname}</span>
      {state.status === 'waiting' ||
      (state.status === 'loading' && state.source === 'device') ? (
        <span class="muted">Loading preview…</span>
      ) : state.status === 'loading' ? (
        <span class="muted">Asking your homeserver…</span>
      ) : state.status === 'failed' ? (
        <span class="muted">{previewFailureMessage(state.failure)}</span>
      ) : (
        <span class="muted">
          Your homeserver can preview this link. It will see the link, even in
          an encrypted room.
        </span>
      )}
      {children}
    </span>
  )
}

/**
 * The composer's preview cards (ADR 0132 § 5): what will be sent with the
 * message, each with an X that discards it. A card that has no preview may
 * offer the homeserver, which is asked only from that button.
 */
export function ComposerLinkPreviews({
  cards,
  onDiscard,
  onAskHomeserver,
}: {
  cards: readonly ComposerPreviewCard[]
  onDiscard(identity: string): void
  onAskHomeserver(identity: string): void
}) {
  if (cards.length === 0) return null
  return (
    <ul class="composer-link-previews" aria-label="Link previews to send">
      {cards.map((card) => (
        <li
          key={card.identity}
          class="composer-link-preview"
          aria-busy={
            card.state.status === 'waiting' || card.state.status === 'loading'
          }
        >
          <CardBody card={card}>
            {card.canAskHomeserver && (
              <button
                type="button"
                class="composer-link-preview-ask"
                onClick={() => onAskHomeserver(card.identity)}
              >
                {card.state.status === 'failed' &&
                card.state.failure.source === 'homeserver'
                  ? 'Try again'
                  : 'Try homeserver'}
              </button>
            )}
          </CardBody>
          <button
            type="button"
            class="composer-link-preview-remove"
            aria-label={`Remove the preview of ${card.hostname}`}
            title="Send without this preview"
            onClick={() => onDiscard(card.identity)}
          >
            ✕
          </button>
        </li>
      ))}
    </ul>
  )
}
