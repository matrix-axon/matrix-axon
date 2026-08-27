import { eventMedia, isImageMedia } from '../media/event-media'
import type { EventMedia } from '../media/event-media'
import { hasEmojiCandidate } from '../emoji'
import { stateEventNotice } from '../state-event-notice'
import type { TimelineEvent } from '../stores/timeline'
import { FormattedBody } from './FormattedBody'
import { MediaAttachment } from './MediaAttachment'
import { MediaImage } from './MediaImage'

function MediaBody({
  event,
  resolved,
}: {
  event: TimelineEvent
  resolved: EventMedia
}) {
  const { media, previewUrl, pending } = resolved
  return isImageMedia(media) ? (
    <MediaImage
      accountId={event.account_id}
      media={media}
      previewUrl={previewUrl}
      // A still-uploading echo is deliberately not in the viewer's sequence,
      // so it keeps its own lightbox rather than opening one that omits it.
      eventId={pending ? undefined : event.event_id}
      content={event.content}
    />
  ) : (
    <MediaAttachment
      accountId={event.account_id}
      media={media}
      content={event.content}
    />
  )
}

function contentMsgtype(content: unknown): string | null {
  const msgtype = (content as { msgtype?: unknown } | null | undefined)?.msgtype
  return typeof msgtype === 'string' ? msgtype : null
}

function unsupportedEventLabel(event: TimelineEvent): string {
  const msgtype = contentMsgtype(event.content)
  if (event.type === 'm.room.message' && msgtype !== null) {
    return `unsupported message (${msgtype})`
  }
  if (msgtype !== null) {
    return `unsupported event: ${event.type} (${msgtype})`
  }
  return `unsupported event: ${event.type}`
}

export const UTD_RECOVER_HINT =
  'This device does not have the megolm key for this message. Recover keys on the Accounts page if you have a recovery key. Messages stay undecryptable if those session keys were never uploaded to backup.'

function UtdPlaceholder() {
  return (
    <span class="utd-placeholder">
      <span class="muted placeholder">unable to decrypt</span>
      <span class="muted" aria-hidden="true">
        {' '}
        ·{' '}
      </span>
      <a href="/accounts" class="utd-recover" title={UTD_RECOVER_HINT}>
        Recover keys
      </a>
    </span>
  )
}

function hasVisibleText(body: string | null | undefined): boolean {
  return body !== null && body !== undefined && body.trim() !== ''
}

export function isEmojiOnlyMessageBody(
  body: string | null | undefined,
): boolean {
  const text = body?.trim()
  if (text === undefined || text === '') {
    return false
  }
  if (!hasEmojiCandidate(text)) {
    return false
  }
  return /^(?:\s|[\p{Extended_Pictographic}\p{Regional_Indicator}\p{Emoji_Modifier}]|\u200D|\uFE0E|\uFE0F|\u20E3|[0-9#*])+$/u.test(
    text,
  )
}

/**
 * The app's single message-body dispatch (ADR 0046; media in ADR 0064):
 * redactions, state events, UTDs, then message kinds — image/sticker inline,
 * file/audio/video as a download card, and everything else through
 * `FormattedBody`. Used by both the main timeline (`RoomPage`) and the thread
 * panel, so media renders identically in both.
 */
export function EventBody({
  event,
  senderDisplay,
  resolveName,
}: {
  event: TimelineEvent
  /** Resolved sender name for the `m.emote` prefix; falls back to the raw
   *  Matrix id when the caller has no members store (WCR-19). */
  senderDisplay?: string
  /** Members-store lookup for state-event notices (ADR 0083); the notice
   *  names users the event itself may not, e.g. whoever kicked someone. */
  resolveName?: (userId: string) => string
}) {
  // Resolved up front, but rendered in two places: a still-uploading send
  // outranks the placeholders below (it can be neither redacted nor a UTD),
  // while confirmed media must lose to them.
  const resolved = eventMedia(event)
  if (resolved !== null && resolved.pending) {
    return <MediaBody event={event} resolved={resolved} />
  }
  if (event.redacted) {
    return <span class="muted placeholder">message deleted</span>
  }
  const isState = event.state_key !== null && event.state_key !== undefined
  if (isState) {
    // Membership and profile changes read as plain sentences (ADR 0083);
    // everything else keeps the raw type/state-key form, which is what the
    // `all` tier is for.
    const notice = stateEventNotice(event, resolveName)
    if (notice !== null) {
      return <span class="state-notice">{notice}</span>
    }
    return (
      <span class="muted">
        {event.type}
        {event.state_key !== '' && ` (${event.state_key})`}
        {event.body !== null && event.body !== undefined && `: ${event.body}`}
      </span>
    )
  }
  if (event.content === null || event.content === undefined) {
    // The decrypted content never reached the store — a UTD (ADR 0015).
    return <UtdPlaceholder />
  }

  if (resolved !== null) {
    return <MediaBody event={event} resolved={resolved} />
  }

  const msgtype = contentMsgtype(event.content)
  if (msgtype === 'm.emote') {
    return (
      <em>
        * {senderDisplay ?? event.sender}{' '}
        <FormattedBody
          accountId={event.account_id}
          body={event.body}
          content={event.content}
        />
      </em>
    )
  }
  const bodyClassName = isEmojiOnlyMessageBody(event.body)
    ? 'body-emoji-only'
    : undefined
  const body = (
    <FormattedBody
      accountId={event.account_id}
      body={event.body}
      content={event.content}
      className={bodyClassName}
    />
  )
  if (!hasVisibleText(event.body)) {
    return <span class="muted placeholder">{unsupportedEventLabel(event)}</span>
  }
  return msgtype === 'm.notice' ? <span class="muted">{body}</span> : body
}
