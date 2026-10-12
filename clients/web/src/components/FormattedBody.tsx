import type { ComponentChildren } from 'preact'
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'preact/hooks'
import {
  currentEmojiEntries,
  emojiNameMap,
  hasEmojiCandidate,
  loadEmojiEntries,
} from '../emoji'
import { highlightCode } from '../code/highlight'
import { matchUrls } from '../html/linkify'
import {
  localRoomHref,
  parseMatrixRoomReference,
  resolveMatrixToRoomLink,
  resolveMatrixToUserLink,
} from '../matrix-to'
import { renderMatrixHtml, SPOILER_CLASS } from '../html/sanitize'
import type { MediaHandle } from '../media/media-service'
import { useServices } from '../services'
import type { RoomDto } from '../stores/room-list'
import type { RoomsStore } from '../stores/rooms'

const EMPTY_EMOJI_NAMES: ReadonlyMap<string, string> = new Map()

type GraphemeSegment = {
  segment: string
}

type SegmenterLike = {
  segment(input: string): Iterable<GraphemeSegment>
}

type IntlWithSegmenter = typeof Intl & {
  Segmenter?: new (
    locale?: string,
    options?: { granularity?: 'grapheme' },
  ) => SegmenterLike
}

type EmojiTextPart = {
  text: string
  title?: string
}

/**
 * Linkify bare URLs in plain text, as VNodes — never via innerHTML, so the
 * text needs no escaping. Formatted bodies get the same treatment inside
 * the sanitizer (`linkifyDomTree`).
 */
function linkify(
  text: string,
  context: {
    accountId: string
    rooms: readonly RoomDto[]
    roomTitles: ReadonlyMap<string, string>
  },
  emojiNames: ReadonlyMap<string, string>,
): ComponentChildren {
  const matches = matchUrls(text)
  if (matches.length === 0) {
    return emojiTooltippedText(text, emojiNames)
  }
  const parts: ComponentChildren[] = []
  let last = 0
  for (const { url, start, end } of matches) {
    if (start > last) {
      parts.push(...emojiTooltippedParts(text.slice(last, start), emojiNames))
    }
    const userLink = resolveMatrixToUserLink(url, '', { localDm: true })
    const roomLink =
      userLink === null ? resolveMatrixToRoomLink(url, context) : null
    parts.push(
      userLink !== null ? (
        <a
          class="mention-pill"
          href={userLink.href}
          title={`Open DM with ${userLink.userId}`}
        >
          {userLink.label}
        </a>
      ) : roomLink !== null ? (
        <a
          class={`room-pill${roomLink.action === 'join' ? ' join-pill' : ''}${roomLink.isEventLink ? ' event-pill' : ''}`}
          href={roomLink.href}
          title={roomPillTitle(roomLink)}
        >
          {roomLink.label}
        </a>
      ) : (
        <a href={url} target="_blank" rel="noopener noreferrer">
          {url}
        </a>
      ),
    )
    last = end
  }
  if (last < text.length) {
    parts.push(...emojiTooltippedParts(text.slice(last), emojiNames))
  }
  return parts
}

function roomPillTitle(roomLink: {
  action: 'open' | 'join'
  isEventLink: boolean
}): string {
  if (roomLink.action === 'join') {
    return roomLink.isEventLink ? 'Join room and jump to message' : 'Join room'
  }
  return roomLink.isEventLink
    ? 'Open existing room at this message'
    : 'Open existing room'
}

function routeRoomPillClick(
  event: MouseEvent,
  accountId: string,
  rooms: RoomsStore,
  onError: (message: string) => void,
): boolean {
  if (
    event.defaultPrevented ||
    event.button !== 0 ||
    event.ctrlKey ||
    event.metaKey ||
    event.altKey ||
    event.shiftKey
  ) {
    return false
  }
  const anchor = (event.target as Element | null)?.closest?.('a.room-pill')
  if (!(anchor instanceof HTMLAnchorElement)) {
    return false
  }
  if (anchor.target !== '' || anchor.download !== '') {
    return false
  }
  event.preventDefault()
  if (anchor.origin !== window.location.origin) {
    const reference = parseMatrixRoomReference(anchor.href)
    if (reference === null) {
      return true
    }
    void rooms
      .joinRoom(accountId, reference.roomIdOrAlias, reference.serverNames)
      .then((result) => {
        if (!result.ok) {
          onError(
            `Could not join ${reference.roomIdOrAlias}: ${result.message}`,
          )
          return
        }
        const next = localRoomHref(accountId, result.roomId, reference.eventId)
        window.history.pushState(null, '', next)
        window.dispatchEvent(new PopStateEvent('popstate'))
      })
    return true
  }
  const next = `${anchor.pathname}${anchor.search}${anchor.hash}`
  window.history.pushState(null, '', next)
  window.dispatchEvent(new PopStateEvent('popstate'))
  return true
}

function routeLocalMentionPillClick(
  event: MouseEvent,
  accountId: string,
  rooms: RoomsStore,
): boolean {
  if (
    event.defaultPrevented ||
    event.button !== 0 ||
    event.ctrlKey ||
    event.metaKey ||
    event.altKey ||
    event.shiftKey
  ) {
    return false
  }
  const anchor = (event.target as Element | null)?.closest?.('a.mention-pill')
  if (!(anchor instanceof HTMLAnchorElement)) {
    return false
  }
  if (
    anchor.target !== '' ||
    anchor.download !== '' ||
    anchor.origin !== window.location.origin ||
    anchor.pathname !== '/rooms/dm'
  ) {
    return false
  }
  const userId = new URL(anchor.href).searchParams.get('user')
  if (userId === null || !userId.startsWith('@')) {
    return false
  }
  event.preventDefault()
  // preact-iso follows the href from a window listener and ignores
  // defaultPrevented, so the click has to stop before it reaches window.
  // Otherwise /rooms/dm opens before createDm can select an existing room.
  event.stopPropagation()
  void rooms.createDm(accountId, userId).then((result) => {
    const next = result.ok
      ? localRoomHref(accountId, result.roomId, null)
      : `${anchor.pathname}${anchor.search}${anchor.hash}`
    window.history.pushState(null, '', next)
    window.dispatchEvent(new PopStateEvent('popstate'))
  })
  return true
}

function emojiTooltippedText(
  text: string,
  emojiNames: ReadonlyMap<string, string>,
): ComponentChildren {
  const parts = emojiTooltippedParts(text, emojiNames)
  return parts.length === 1 && typeof parts[0] === 'string' ? parts[0] : parts
}

function emojiTooltippedParts(
  text: string,
  emojiNames: ReadonlyMap<string, string>,
): ComponentChildren[] {
  return emojiTextParts(text, emojiNames).map((part) =>
    part.title === undefined ? (
      part.text
    ) : (
      <span class="emoji-tooltip" title={part.title}>
        {part.text}
      </span>
    ),
  )
}

function addEmojiTooltipsToHtml(
  html: string,
  emojiNames: ReadonlyMap<string, string>,
): string {
  if (emojiNames.size === 0 || !hasEmojiCandidate(html)) {
    return html
  }
  const template = document.createElement('template')
  template.innerHTML = html
  const walker = document.createTreeWalker(
    template.content,
    NodeFilter.SHOW_TEXT,
  )
  const textNodes: Text[] = []
  let node = walker.nextNode()
  while (node !== null) {
    textNodes.push(node as Text)
    node = walker.nextNode()
  }
  for (const textNode of textNodes) {
    if (textNode.parentElement?.closest('a, code, pre') != null) {
      continue
    }
    const parts = emojiTextParts(textNode.data, emojiNames)
    if (parts.every((part) => part.title === undefined)) {
      continue
    }
    const fragment = document.createDocumentFragment()
    for (const part of parts) {
      if (part.title === undefined) {
        fragment.append(part.text)
        continue
      }
      const span = document.createElement('span')
      span.className = 'emoji-tooltip'
      span.title = part.title
      span.textContent = part.text
      fragment.append(span)
    }
    textNode.replaceWith(fragment)
  }
  return template.innerHTML
}

function emojiTextParts(
  text: string,
  emojiNames: ReadonlyMap<string, string>,
): EmojiTextPart[] {
  if (emojiNames.size === 0 || !hasEmojiCandidate(text)) {
    return [{ text }]
  }
  const parts: EmojiTextPart[] = []
  let pending = ''
  for (const segment of graphemeSegments(text)) {
    const title = emojiNames.get(normalizeTooltipEmoji(segment))
    if (title === undefined) {
      pending += segment
      continue
    }
    if (pending !== '') {
      parts.push({ text: pending })
      pending = ''
    }
    parts.push({ text: segment, title })
  }
  if (pending !== '') {
    parts.push({ text: pending })
  }
  return parts.length === 0 ? [{ text }] : parts
}

function graphemeSegments(text: string): string[] {
  const Segmenter = (Intl as IntlWithSegmenter).Segmenter
  if (Segmenter === undefined) {
    return Array.from(text)
  }
  return [
    ...new Segmenter(undefined, { granularity: 'grapheme' }).segment(text),
  ].map((segment) => segment.segment)
}

function normalizeTooltipEmoji(emoji: string): string {
  return emoji.replaceAll('\uFE0F', '')
}

/**
 * A message body: sanitized Matrix HTML when the event carries
 * `format: "org.matrix.custom.html"`, plain text (with bare URLs linkified)
 * otherwise. Spoilers render hidden and reveal on click / Enter (ADR 0046,
 * M-W5).
 */
export function FormattedBody({
  accountId,
  body,
  content,
  className,
}: {
  accountId: string
  body: string | null | undefined
  content: unknown
  className?: string
}) {
  const { media, rooms } = useServices()
  const rootRef = useRef<HTMLSpanElement>(null)
  const [roomLinkError, setRoomLinkError] = useState<string | null>(null)
  const roomList = rooms.rooms.value
  const roomTitles = rooms.titles.value
  const formattedBody = (
    content as { formatted_body?: unknown } | null | undefined
  )?.formatted_body
  const bodyHasEmoji =
    hasEmojiCandidate(body) ||
    (typeof formattedBody === 'string' && hasEmojiCandidate(formattedBody))
  const [emojiEntries, setEmojiEntries] = useState(() => currentEmojiEntries())
  useEffect(() => {
    if (!bodyHasEmoji) {
      return
    }
    let cancelled = false
    void loadEmojiEntries().then((entries) => {
      if (!cancelled) {
        setEmojiEntries(entries)
      }
    })
    return () => {
      cancelled = true
    }
  }, [bodyHasEmoji])
  const emojiNames = useMemo(
    () => (bodyHasEmoji ? emojiNameMap(emojiEntries) : EMPTY_EMOJI_NAMES),
    [bodyHasEmoji, emojiEntries],
  )
  const html = useMemo(() => {
    const roomLinkContext = {
      accountId,
      rooms: roomList,
      roomTitles,
    }
    const c = content as
      { format?: unknown; formatted_body?: unknown } | null | undefined
    if (
      c?.format === 'org.matrix.custom.html' &&
      typeof c.formatted_body === 'string'
    ) {
      const rendered = renderMatrixHtml(c.formatted_body, {
        resolveUserLink: (href, label) =>
          resolveMatrixToUserLink(href, label, { localDm: true }),
        resolveRoomLink: (href, label) =>
          resolveMatrixToRoomLink(href, roomLinkContext, label),
      })
      return bodyHasEmoji
        ? addEmojiTooltipsToHtml(rendered, emojiNames)
        : rendered
    }
    return null
  }, [content, accountId, roomList, roomTitles, bodyHasEmoji, emojiNames])

  // Resolve inline `<img data-mxc>` (the sanitizer moved a safe `mxc://` src
  // here) to authenticated blob URLs after mount. This is imperative DOM
  // because the subtree came from `dangerouslySetInnerHTML`; each handle is
  // released on cleanup so the blob can be revoked. A failed acquire leaves the
  // `<img>` srcless, so its `alt` renders.
  useLayoutEffect(() => {
    const root = rootRef.current
    if (root === null || html === null) {
      return
    }
    const handles: MediaHandle[] = []
    let cancelled = false
    for (const img of root.querySelectorAll<HTMLImageElement>(
      'img[data-mxc]',
    )) {
      const mxc = img.getAttribute('data-mxc')
      if (mxc === null) {
        continue
      }
      void media.acquire(accountId, mxc).then((handle) => {
        if (cancelled) {
          handle.release()
          return
        }
        handles.push(handle)
        if (handle.result.ok) {
          img.src = handle.result.url
        }
      })
    }
    return () => {
      cancelled = true
      for (const handle of handles) {
        handle.release()
      }
    }
  }, [media, accountId, html])

  // Syntax-highlight fenced code blocks (ADR 0073). The sanitizer already
  // preserves spec-shaped `language-*` classes on `<code>`, which is the only
  // signal Matrix gives for a block's language. Same imperative shape as the
  // image resolution above, and for the same reason: the subtree came from
  // `dangerouslySetInnerHTML`.
  useLayoutEffect(() => {
    const root = rootRef.current
    if (root === null || html === null) {
      return
    }
    let cancelled = false
    for (const el of root.querySelectorAll<HTMLElement>('pre code[class]')) {
      const declared = [...el.classList]
        .find((cls) => cls.startsWith('language-'))
        ?.slice('language-'.length)
      if (declared === undefined) {
        continue
      }
      const source = el.textContent ?? ''
      void highlightCode(source, declared).then((highlighted) => {
        // `textContent` still matching guards the race where the effect re-ran
        // for new content before this resolved.
        if (!cancelled && highlighted !== null && el.textContent === source) {
          el.innerHTML = highlighted
          el.classList.add('hljs')
        }
      })
    }
    return () => {
      cancelled = true
    }
  }, [html])

  if (html === null) {
    const roomLinkContext = {
      accountId,
      rooms: roomList,
      roomTitles,
    }
    return (
      <>
        <span
          class={
            className === undefined ? 'body-text' : `body-text ${className}`
          }
          onClick={(event) => {
            setRoomLinkError(null)
            if (!routeLocalMentionPillClick(event, accountId, rooms)) {
              routeRoomPillClick(event, accountId, rooms, setRoomLinkError)
            }
          }}
        >
          {linkify(body ?? '', roomLinkContext, emojiNames)}
        </span>
        {roomLinkError !== null && (
          <span class="event-copy-status error" role="alert">
            {roomLinkError}
          </span>
        )}
      </>
    )
  }

  const toggleSpoiler = (target: EventTarget | null) => {
    const spoiler = (target as Element | null)?.closest?.(`.${SPOILER_CLASS}`)
    if (spoiler !== null && spoiler !== undefined) {
      spoiler.classList.toggle('spoiler-revealed')
    }
  }

  return (
    <>
      <span
        class={className === undefined ? 'body-html' : `body-html ${className}`}
        ref={rootRef}
        // Sanitized by renderMatrixHtml (DOMPurify + Matrix allowlist).
        dangerouslySetInnerHTML={{ __html: html }}
        onClick={(event) => {
          setRoomLinkError(null)
          if (
            !routeLocalMentionPillClick(event, accountId, rooms) &&
            !routeRoomPillClick(event, accountId, rooms, setRoomLinkError)
          ) {
            toggleSpoiler(event.target)
          }
        }}
        onKeyDown={(event) => {
          if (event.key === 'Enter' || event.key === ' ') {
            toggleSpoiler(event.target)
          }
        }}
      />
      {roomLinkError !== null && (
        <span class="event-copy-status error" role="alert">
          {roomLinkError}
        </span>
      )}
    </>
  )
}
