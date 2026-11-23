import { eventMedia, isImageMedia } from '../media/event-media'
import type { EventMedia } from '../media/event-media'
import { hasEmojiCandidate } from '../emoji'
import { stateEventNotice } from '../state-event-notice'
import type { TimelineEvent } from '../stores/timeline'
import { useServices } from '../services'
import type { components } from '../api/schema'
import { AccountTrustActions } from './AccountTrustActions'
import { FormattedBody } from './FormattedBody'
import { MediaAttachment } from './MediaAttachment'
import { MediaImage } from './MediaImage'
import { LinkPreviews } from './LinkPreviews'
import { MatrixLinkCards } from './MatrixLinkCards'

function MediaBody({
  event,
  resolved,
}: {
  event: TimelineEvent
  resolved: EventMedia
}) {
  const { media, previewUrl, pending } = resolved
  const { timelineMediaExpanded } = useServices()
  const expanded = timelineMediaExpanded.value
  return expanded && isImageMedia(media) ? (
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
      compact={!expanded}
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

const WITHHELD_REASON: Record<string, string> = {
  'm.unverified': 'The sender withheld the key from an unverified device.',
  'm.blacklisted': 'The sender blocked this device from receiving the key.',
  'm.unauthorised': 'The sender did not authorize sharing the key.',
  'm.unavailable': 'The sender reported that the key was unavailable.',
  'm.no_olm':
    'The sender could not establish a secure session to share the key.',
  'm.history_not_shared': 'The room history was not shared with this device.',
}

const TRUST_LEVEL: Record<
  components['schemas']['VerificationLevelDto'],
  string
> = {
  unverified_identity: 'the sender’s identity was unverified',
  verification_violation: 'the sender’s verified identity changed',
  unsigned_device: 'the sender’s device was not signed',
  missing_device: 'the sender’s device was unknown',
  insecure_source: 'the key came from an untrusted source',
  mismatched_sender: 'the key did not match the sender',
}

function utdExplanation(
  reason: TimelineEvent['utd_reason'],
): string | undefined {
  switch (reason?.kind) {
    case 'missing_key':
      return 'Axon is missing the key for this message.'
    case 'withheld': {
      const code = reason.withheld_code ?? ''
      return Object.hasOwn(WITHHELD_REASON, code)
        ? WITHHELD_REASON[code]
        : 'The sender withheld the key.'
    }
    case 'sender_not_trusted': {
      const level = reason.verification_level
      return level && Object.hasOwn(TRUST_LEVEL, level)
        ? `Axon could not trust the sender because ${TRUST_LEVEL[level]}.`
        : 'Axon could not trust the sender’s device.'
    }
    case 'other':
      return 'Axon could not decrypt the message with the information available.'
  }
}

function UtdPlaceholder({ event }: { event: TimelineEvent }) {
  const { accounts } = useServices()
  const account = accounts.accounts.value.find(
    (row) => row.account_id === event.account_id,
  )
  const reason = event.utd_reason
  const needsVerification =
    reason?.kind === 'withheld' &&
    reason.withheld_code === 'm.unverified' &&
    account?.state === 'active' &&
    account.verified === false
  const explanation = needsVerification
    ? 'The sender’s app does not share keys with unverified devices. Verify this Axon device to read new messages.'
    : utdExplanation(reason)

  return (
    <span class="utd-placeholder">
      <span class="muted placeholder">unable to decrypt</span>
      {explanation !== undefined && <span class="muted"> · {explanation}</span>}
      {needsVerification ? (
        <AccountTrustActions account={account} />
      ) : (
        <>
          <span class="muted" aria-hidden="true">
            {' '}
            ·{' '}
          </span>
          <a
            href="/accounts"
            class="utd-recover"
            title={
              reason == null || reason.kind === 'missing_key'
                ? UTD_RECOVER_HINT
                : undefined
            }
          >
            Recover keys
          </a>
        </>
      )}
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
  showLinkPreviews = true,
}: {
  event: TimelineEvent
  /** Resolved sender name for the `m.emote` prefix; falls back to the raw
   *  Matrix id when the caller has no members store (WCR-19). */
  senderDisplay?: string
  /** Members-store lookup for state-event notices (ADR 0083); the notice
   *  names users the event itself may not, e.g. whoever kicked someone. */
  resolveName?: (userId: string) => string
  /** Compact thread-root headers show only the existing message body. */
  showLinkPreviews?: boolean
}) {
  const { timelineMediaExpanded } = useServices()
  const expanded = timelineMediaExpanded.value
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
    return <UtdPlaceholder event={event} />
  }

  // Bundled previews, then the cards this client builds for the message's
  // Matrix links (ADR 0138), which collapse with them.
  const previews =
    expanded && showLinkPreviews && event.type === 'm.room.message' ? (
      <>
        <LinkPreviews
          accountId={event.account_id}
          content={event.content}
          body={event.body}
        />
        <MatrixLinkCards event={event} resolveName={resolveName} />
      </>
    ) : null

  if (resolved !== null) {
    return (
      <>
        <MediaBody event={event} resolved={resolved} />
        {previews}
      </>
    )
  }

  const msgtype = contentMsgtype(event.content)
  if (msgtype === 'm.emote') {
    return (
      <>
        <em>
          * {senderDisplay ?? event.sender}{' '}
          <FormattedBody
            accountId={event.account_id}
            hideImages={!expanded}
            body={event.body}
            content={event.content}
          />
        </em>
        {previews}
      </>
    )
  }
  const bodyClassName = isEmojiOnlyMessageBody(event.body)
    ? 'body-emoji-only'
    : undefined
  const body = (
    <FormattedBody
      accountId={event.account_id}
      hideImages={!expanded}
      body={event.body}
      content={event.content}
      className={bodyClassName}
    />
  )
  if (!hasVisibleText(event.body)) {
    return <span class="muted placeholder">{unsupportedEventLabel(event)}</span>
  }
  return (
    <>
      {msgtype === 'm.notice' ? <span class="muted">{body}</span> : body}
      {previews}
    </>
  )
}
