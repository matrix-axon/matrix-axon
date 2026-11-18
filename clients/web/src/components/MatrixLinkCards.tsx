import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { formatTimelineDay } from '../calendar-day'
import {
  linkedEventTargets,
  textWithSpoilersWithheld,
  withoutReplyFallback,
  type LinkedEvent,
  type LinkedEventResult,
} from '../linked-events'
import {
  localRoomHref,
  localThreadEventHref,
  matrixToEventLink,
  type LinkedEventTarget,
} from '../matrix-to'
import { MAX_PREVIEW_IMAGE_BYTES } from '../link-previews'
import { parseMedia, type ParsedMedia } from '../media/parse-media'
import { useMediaBlob } from '../media/use-media-blob'
import { scrollParentOf } from '../scroll-parent'
import { useServices } from '../services'
import { roomTitle } from '../stores/room-list'
import type { EventDto, TimelineEvent } from '../stores/timeline'
import { formatEventTime } from './EventStatus'
import { routeRoomPillClick } from './FormattedBody'

/**
 * What a card says when its `sender_trust` does not support the sender's
 * name (ADR 0138 § 5). The card is shown away from the room the message came
 * from, so it says so in plain words; `verified`, `unverified` and `null` add
 * nothing.
 */
export const TRUST_NOTICES: Readonly<Record<string, string>> = {
  verification_violation:
    "The sender's identity has changed since you verified it.",
  unknown: 'Sent from a device that can no longer be identified.',
  mismatched_sender: "Encrypted by another user's device, such as a bridge.",
}

/**
 * Cards for the Matrix links in a received message (ADR 0138 § 2): one per
 * linked event, read with this account's own access, so a card appears only
 * when the account can read the event and nothing is copied into the room.
 */
export function MatrixLinkCards({
  event,
  resolveName,
}: {
  event: TimelineEvent
  /** The message's room members, for naming a sender in the same room. */
  resolveName?: (userId: string) => string
}) {
  const { rooms, settings } = useServices()
  const roomList = rooms.rooms.value
  const enabled = settings.matrixLinkCards.value
  const targets = useMemo(
    () =>
      enabled
        ? linkedEventTargets(event, {
            accountId: event.account_id,
            rooms: roomList,
          })
        : [],
    [enabled, event, roomList],
  )
  if (targets.length === 0) return null
  return (
    <div class="matrix-link-cards" aria-label="Linked messages">
      {targets.map((target) => (
        <MatrixLinkCard
          key={`${target.roomId}\0${target.eventId}`}
          accountId={event.account_id}
          target={target}
          resolveName={
            target.roomId === event.room_id ? resolveName : undefined
          }
        />
      ))}
    </div>
  )
}

/**
 * Start `onNear` once `el` comes within a screen's height of the timeline's
 * visible part (ADR 0138 § 2), so scrolling past a long history reads
 * nothing. Eager where there is no `IntersectionObserver` (jsdom).
 */
function useNearViewport(onNear: () => void, deps: readonly unknown[]) {
  const ref = useRef<HTMLDivElement>(null)
  useEffect(() => {
    const el = ref.current
    if (el === null || typeof IntersectionObserver === 'undefined') {
      onNear()
      return
    }
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          observer.disconnect()
          onNear()
        }
      },
      // The root is the timeline's scroller, as for the room's history
      // prefetch: the scroller clips the card before the document's viewport
      // is consulted, so a margin on the default root would do nothing. A
      // percentage margin is of the root's own height.
      { root: scrollParentOf(el), rootMargin: '100% 0px' },
    )
    observer.observe(el)
    return () => observer.disconnect()
    // eslint-disable-next-line react-hooks/exhaustive-deps -- the caller names the read's own inputs; `onNear` is rebuilt every render
  }, deps)
  return ref
}

function MatrixLinkCard({
  accountId,
  target,
  resolveName,
}: {
  accountId: string
  target: LinkedEventTarget
  resolveName?: (userId: string) => string
}) {
  const { linkedEvents } = useServices()
  const [result, setResult] = useState<LinkedEventResult | null>(null)
  const [cancelled] = useState(() => ({ current: false }))
  useEffect(
    () => () => {
      cancelled.current = true
    },
    [cancelled],
  )
  const ref = useNearViewport(() => {
    void linkedEvents
      .read(accountId, target.roomId, target.eventId)
      .then((read) => {
        if (!cancelled.current) setResult(read)
      })
  }, [linkedEvents, accountId, target.roomId, target.eventId])

  // One fresh read per card when its image is a 404 (§ 4): the server forgot
  // the event's media, and reading it again keeps it again. True when the
  // event was read; a 404 removes the card, and a transient failure keeps
  // the card as it was, without its image.
  const [refreshed, setRefreshed] = useState(false)
  const refresh = async (): Promise<boolean> => {
    setRefreshed(true)
    const read = await linkedEvents.read(
      accountId,
      target.roomId,
      target.eventId,
      { fresh: true },
    )
    if (cancelled.current || read.kind === 'transient') return false
    setResult(read)
    return read.kind === 'event'
  }

  if (result?.kind !== 'event') {
    // A 404 leaves no card; a transient failure is read again the next time
    // the message is rendered. Either way the anchor stays for the observer.
    return <div ref={ref} class="matrix-link-card-anchor" />
  }
  return (
    <CardBody
      accountId={accountId}
      event={result.event}
      via={target.via}
      resolveName={resolveName}
      imageRetry={refreshed ? null : refresh}
    />
  )
}

function CardBody({
  accountId,
  event,
  via,
  resolveName,
  imageRetry,
}: {
  accountId: string
  event: LinkedEvent
  /** The link's `via` servers, for joining from the card. */
  via: readonly string[]
  resolveName?: (userId: string) => string
  imageRetry: (() => Promise<boolean>) | null
}) {
  const { rooms, settings } = useServices()
  const [joinError, setJoinError] = useState<string | null>(null)
  const room = rooms.rooms.value.find(
    (r) => r.account_id === accountId && r.room_id === event.room_id,
  )
  const roomName =
    room === undefined ? event.room_id : roomTitle(room, rooms.titles.value)
  const sender = resolveName?.(event.sender) ?? event.sender
  const time = `${formatTimelineDay(event.origin_ts)} ${formatEventTime(
    event.origin_ts,
    settings.timeFormat.value,
  )}`
  const href = cardHref(accountId, event, room !== undefined, via)
  const notice =
    event.sender_trust === null || event.sender_trust === undefined
      ? undefined
      : TRUST_NOTICES[event.sender_trust]
  const media = useMemo(
    () =>
      event.redacted || event.content === null
        ? null
        : parseMedia(event as unknown as EventDto),
    [event],
  )
  const text = useMemo(
    () => cardText(event, media, sender),
    [event, media, sender],
  )
  return (
    <>
      <a
        class="matrix-link-card"
        href={href}
        onClick={(click) => {
          setJoinError(null)
          routeRoomPillClick(
            click,
            accountId,
            rooms,
            setJoinError,
            'a.matrix-link-card',
          )
        }}
      >
        <span class="matrix-link-card-head">
          <strong class="matrix-link-card-sender">{sender}</strong>
          <span class="muted">
            {' '}
            in {roomName} · {time}
          </span>
        </span>
        {notice !== undefined && (
          <span class="matrix-link-card-trust" role="note">
            {notice}
          </span>
        )}
        {media !== null && (
          <CardMedia
            accountId={accountId}
            event={event}
            media={media}
            retry={imageRetry}
          />
        )}
        <span class="matrix-link-card-text">{text}</span>
      </a>
      {joinError !== null && (
        <span class="event-copy-status error" role="alert">
          {joinError}
        </span>
      )}
    </>
  )
}

/**
 * Where the card leads: the event in its room, in its thread when it is a
 * thread reply, or, for a room the account has not joined, a Matrix link with
 * the original link's `via` servers, which the click router turns into the
 * join flow, as it does for the link's pill. An event read from the
 * homeserver may be outside the room's stored history, which the timeline
 * cannot show; opening that event from its card is #1148.
 */
function cardHref(
  accountId: string,
  event: LinkedEvent,
  joined: boolean,
  via: readonly string[],
): string {
  if (!joined) return matrixToEventLink(event.room_id, event.event_id, via)
  const relation = event.relates_to as
    { rel_type?: unknown; event_id?: unknown } | null | undefined
  if (
    relation?.rel_type === 'm.thread' &&
    typeof relation.event_id === 'string'
  ) {
    return localThreadEventHref(
      accountId,
      event.room_id,
      relation.event_id,
      event.event_id,
    )
  }
  return localRoomHref(accountId, event.room_id, event.event_id)
}

/**
 * Up to three lines of the message's text, as plain text (no nesting).
 * `sender` is the name the card's header gives the sender, for an emote.
 */
export function cardText(
  event: LinkedEvent,
  media: ParsedMedia | null,
  sender: string = event.sender,
): string {
  if (event.redacted) return 'Message deleted'
  if (event.content === null || event.content === undefined)
    return 'Message could not be decrypted'
  // A spoiler stays hidden here as it does in the event's own room.
  const withheld = textWithSpoilersWithheld(event.content)
  if (media !== null) {
    const caption = withheld ?? media.caption
    if (media.kind === 'image' || media.kind === 'sticker') return caption ?? ''
    return caption === null || caption === ''
      ? media.filename
      : `${media.filename}: ${caption}`
  }
  const msgtype = (event.content as { msgtype?: unknown }).msgtype
  // Without an older client's quoted reply fallback, so the card's lines go
  // to the reply itself.
  const body =
    withheld ??
    withoutReplyFallback(
      event.body ?? '',
      event.content as Record<string, unknown>,
    ).trim()
  if (event.type === 'm.room.message' && body !== '') {
    return msgtype === 'm.emote' ? `* ${sender} ${body}` : body
  }
  return typeof msgtype === 'string'
    ? `${event.type} (${msgtype})`
    : `${event.type} event`
}

/**
 * An image's or sticker's thumbnail, or a video's poster, read through the
 * event that names it (`room_id`/`event_id`, ADR 0138 § 4). A file or audio
 * message shows no image, only its name.
 */
function CardMedia({
  accountId,
  event,
  media,
  retry,
}: {
  accountId: string
  event: LinkedEvent
  media: ParsedMedia
  retry: (() => Promise<boolean>) | null
}) {
  const { linkedEvents, timelineMediaExpanded } = useServices()
  const picture = media.kind === 'image' || media.kind === 'sticker'
  const mxc = picture
    ? media.encrypted
      ? (media.thumbnailUrl ?? media.url)
      : media.url
    : media.kind === 'video'
      ? media.thumbnailUrl
      : null
  if (mxc === null || !timelineMediaExpanded.value) return null
  const imageKey = `${accountId}\0${event.room_id}\0${event.event_id}\0${mxc}`
  // An image given up on earlier this session is not asked for again.
  if (linkedEvents.imageExhausted(imageKey)) return null
  return (
    <CardImage
      key={imageKey}
      imageKey={imageKey}
      accountId={accountId}
      event={event}
      mxc={mxc}
      // A plaintext image is thumbnailed by the homeserver; encrypted media
      // can only be fetched whole, so it is bounded instead.
      thumbnail={!media.encrypted}
      retry={retry}
    />
  )
}

function CardImage({
  imageKey,
  accountId,
  event,
  mxc,
  thumbnail,
  retry,
}: {
  /** Names the image for `LinkedEventCache.exhaustImage`. */
  imageKey: string
  accountId: string
  event: LinkedEvent
  mxc: string
  thumbnail: boolean
  retry: (() => Promise<boolean>) | null
}) {
  const { linkedEvents } = useServices()
  const [attempt, setAttempt] = useState(0)
  const [failed, setFailed] = useState(false)
  const { ref, state, invalidate } = useMediaBlob<HTMLSpanElement>(
    accountId,
    mxc,
    {
      thumbnail: thumbnail
        ? { width: 320, height: 240, method: 'scale' }
        : undefined,
      maxBytes: thumbnail ? undefined : MAX_PREVIEW_IMAGE_BYTES,
      event: { roomId: event.room_id, eventId: event.event_id },
      attempt,
    },
  )
  const kind = state.status === 'error' ? state.error?.kind : undefined
  useEffect(() => {
    if (kind === undefined) return
    if (kind !== 'not_found') {
      setFailed(true)
      return
    }
    if (retry === null) {
      // Still a 404 after the event was read again: given up for the session.
      linkedEvents.exhaustImage(imageKey)
      setFailed(true)
      return
    }
    // Once per card: re-read the event, which keeps its media again on the
    // server, and ask for the image once more only if that read succeeded.
    void retry().then((read) => {
      if (!read) {
        setFailed(true)
        return
      }
      invalidate()
      setAttempt((n) => n + 1)
    })
    // eslint-disable-next-line react-hooks/exhaustive-deps -- reacts to a new failure only; `retry` turns null after its one use
  }, [kind])
  if (failed) return null
  return (
    <span ref={ref} class="matrix-link-card-image">
      {state.status === 'ready' && (
        <img
          src={state.url}
          alt=""
          decoding="async"
          onError={() => setFailed(true)}
        />
      )}
    </span>
  )
}
