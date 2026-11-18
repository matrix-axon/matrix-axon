import type { ApiClient } from './api/client'
import type { components } from './api/schema'
import { renderMatrixHtml } from './html/sanitize'
import { matchUrls } from './html/linkify'
import { linkedEventTarget, type LinkedEventTarget } from './matrix-to'
import type { RoomDto } from './stores/room-list'

export type LinkedEvent = components['schemas']['LinkedEventDto']

/**
 * One linked-event read (ADR 0138 § 3): the event, `gone` for a 404 (nothing
 * this account can show), or `transient` for anything else, which is tried
 * again the next time the message is rendered.
 */
export type LinkedEventResult =
  | { kind: 'event'; event: LinkedEvent }
  | { kind: 'gone' }
  | { kind: 'transient' }

/** At most this many cards under one message (ADR 0138 § 2). */
export const MAX_MATRIX_LINK_CARDS = 3

/** Results kept for the session, oldest first out past this many. */
const MAX_CACHED = 500

/**
 * Linked-event reads for the session, keyed by account, room and event.
 * Concurrent reads of one event share one request. An event and a 404 are
 * kept; a transient failure is not. Wiped on sign-out.
 */
export interface LinkedEventCache {
  /**
   * Read an event. `fresh` reads past the kept result, as a card does once
   * when its image is a 404 (§ 4), and replaces it; concurrent fresh reads of
   * one event share one request.
   */
  read(
    accountId: string,
    roomId: string,
    eventId: string,
    options?: { fresh?: boolean },
  ): Promise<LinkedEventResult>
  /**
   * Whether a card's image was given up on this session: still a 404 after
   * its event was read again (§ 4). `key` names the account, room, event and
   * `mxc://` URI.
   */
  imageExhausted(key: string): boolean
  /** Remember that a card's image is not to be asked for again. */
  exhaustImage(key: string): void
  clear(): void
}

export function createLinkedEventCache(api: ApiClient): LinkedEventCache {
  const settled = new Map<string, LinkedEventResult>()
  const inflight = new Map<
    string,
    { promise: Promise<LinkedEventResult>; fresh: boolean }
  >()
  const exhausted = new Set<string>()
  let generation = 0

  function request(
    key: string,
    accountId: string,
    roomId: string,
    eventId: string,
    fresh: boolean,
  ): Promise<LinkedEventResult> {
    const started = generation
    const promise = api
      .GET('/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}', {
        params: {
          path: { account_id: accountId, room_id: roomId, event_id: eventId },
        },
      })
      .then(({ data, response }): LinkedEventResult => {
        if (data !== undefined) return { kind: 'event', event: data.data }
        return response.status === 404
          ? { kind: 'gone' }
          : { kind: 'transient' }
      })
      .catch((): LinkedEventResult => ({ kind: 'transient' }))
      .then((result) => {
        if (inflight.get(key)?.promise === promise) inflight.delete(key)
        if (started === generation && result.kind !== 'transient') {
          settled.delete(key)
          settled.set(key, result)
          while (settled.size > MAX_CACHED) {
            settled.delete(settled.keys().next().value!)
          }
        }
        return result
      })
    inflight.set(key, { promise, fresh })
    return promise
  }

  return {
    read(accountId, roomId, eventId, options = {}) {
      const key = `${accountId}\0${roomId}\0${eventId}`
      const fresh = options.fresh === true
      const pending = inflight.get(key)
      if (fresh) {
        if (pending?.fresh === true) return pending.promise
        return request(key, accountId, roomId, eventId, true)
      }
      const hit = settled.get(key)
      if (hit !== undefined) return Promise.resolve(hit)
      if (pending !== undefined) return pending.promise
      return request(key, accountId, roomId, eventId, false)
    },
    imageExhausted(key) {
      return exhausted.has(key)
    },
    exhaustImage(key) {
      exhausted.delete(key)
      exhausted.add(key)
      while (exhausted.size > MAX_CACHED) {
        exhausted.delete(exhausted.values().next().value!)
      }
    },
    clear() {
      generation += 1
      settled.clear()
      inflight.clear()
      exhausted.clear()
    },
  }
}

/**
 * A linked event's text with its spoilers withheld, for its card: the
 * formatted body as plain text, each spoiler replaced by `[spoiler]` and the
 * reply fallback left out. `null` when the formatted body has no spoiler, so
 * the plain body (or a media caption) serves as it is.
 */
export function textWithSpoilersWithheld(content: unknown): string | null {
  const fields = (content ?? {}) as Record<string, unknown>
  if (
    fields.format !== 'org.matrix.custom.html' ||
    typeof fields.formatted_body !== 'string' ||
    !/data-mx-spoiler/i.test(fields.formatted_body)
  )
    return null
  const template = document.createElement('template')
  template.innerHTML = renderMatrixHtml(fields.formatted_body)
  const root = template.content
  for (const reply of root.querySelectorAll('mx-reply')) reply.remove()
  for (const spoiler of root.querySelectorAll('[data-mx-spoiler], .spoiler'))
    spoiler.replaceWith('[spoiler]')
  for (const breakLine of root.querySelectorAll('br'))
    breakLine.replaceWith(' ')
  return (root.textContent ?? '').replace(/\s+/g, ' ').trim()
}

/** A message, as far as choosing its Matrix-link cards needs it. */
export interface CardSource {
  event_id: string
  content?: unknown
  body?: string | null
}

/**
 * The events a message's Matrix links point at, for its cards (ADR 0138 § 2):
 * event links only, distinct, in text order, at most
 * {@link MAX_MATRIX_LINK_CARDS}, never the message itself. The links come
 * from the body the message shows: the formatted body with its reply
 * fallback (`<mx-reply>`) removed, or the plain body without its quoted
 * fallback lines. A message with a spoiler gets none, as it gets no previews,
 * so a card cannot show what the spoiler hides.
 */
export function linkedEventTargets(
  message: CardSource,
  context: { accountId: string; rooms: readonly RoomDto[] },
): LinkedEventTarget[] {
  const content = (message.content ?? {}) as Record<string, unknown>
  const hrefs: string[] = []
  if (
    content.format === 'org.matrix.custom.html' &&
    typeof content.formatted_body === 'string'
  ) {
    if (/data-mx-spoiler/i.test(content.formatted_body)) return []
    const template = document.createElement('template')
    template.innerHTML = renderMatrixHtml(content.formatted_body)
    for (const anchor of template.content.querySelectorAll('a[href]')) {
      hrefs.push(anchor.getAttribute('href')!)
    }
  } else if (typeof message.body === 'string') {
    hrefs.push(
      ...matchUrls(withoutReplyFallback(message.body, content)).map(
        ({ url }) => url,
      ),
    )
  }
  const seen = new Set<string>()
  const targets: LinkedEventTarget[] = []
  for (const href of hrefs) {
    const target = linkedEventTarget(href, context)
    if (target === null || target.eventId === message.event_id) continue
    const key = `${target.roomId}\0${target.eventId}`
    if (seen.has(key)) continue
    seen.add(key)
    targets.push(target)
    if (targets.length === MAX_MATRIX_LINK_CARDS) break
  }
  return targets
}

/**
 * A plain-text reply's body without the `> ` lines an older client quoted
 * the replied-to message in, and the blank line after them.
 */
function withoutReplyFallback(
  body: string,
  content: Record<string, unknown>,
): string {
  const relation = content['m.relates_to'] as
    { 'm.in_reply_to'?: unknown } | undefined
  if (relation?.['m.in_reply_to'] === undefined || !body.startsWith('> '))
    return body
  const lines = body.split('\n')
  let start = 0
  while (start < lines.length && lines[start].startsWith('>')) start += 1
  if (start < lines.length && lines[start] === '') start += 1
  return lines.slice(start).join('\n')
}
