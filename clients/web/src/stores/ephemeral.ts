import { computed, signal, type ReadonlySignal } from '@preact/signals'
import type { EphemeralPassthrough } from '../api/frames'

export const TYPING_TTL_MS = 30_000

export interface ReadReceipt {
  userId: string
  ts: number | null
}

export interface EphemeralStore {
  /** Replace/apply one room-scoped passthrough event from the live socket. */
  apply(accountId: string, event: EphemeralPassthrough): void
  /** Current non-stale typers for a room, excluding the optional own user id. */
  typingUsers(
    accountId: string,
    roomId: string,
    ownUserId?: string | null,
  ): readonly string[]
  /** Public read receipts for an event, excluding the optional own user id. */
  readReceipts(
    accountId: string,
    roomId: string,
    eventId: string,
    ownUserId?: string | null,
  ): readonly ReadReceipt[]
  /** Clear live typing overlays after reconnects/disconnects. */
  clearTyping(): void
  /** Monotonic revision for Preact signal subscriptions. */
  revision: ReadonlySignal<number>
}

interface TypingEntry {
  userIds: readonly string[]
  expiresAt: number
}

export interface ReceiptEntry {
  publicRead: ReadonlyMap<string, number | null>
  privateRead: ReadonlyMap<string, number | null>
}

const keyOf = (accountId: string, roomId: string) =>
  `${accountId}\u0000${roomId}`
const eventKeyOf = (accountId: string, roomId: string, eventId: string) =>
  `${keyOf(accountId, roomId)}\u0000${eventId}`

export function createEphemeralStore(
  now: () => number = () => Date.now(),
): EphemeralStore {
  const typing = new Map<string, TypingEntry>()
  const receipts = new Map<string, ReceiptEntry>()
  const revision = signal(0)
  let pruneTimer: ReturnType<typeof setTimeout> | null = null

  function bump(): void {
    revision.value += 1
  }

  function pruneTyping(): boolean {
    const at = now()
    let changed = false
    for (const [key, entry] of typing) {
      if (entry.expiresAt <= at) {
        typing.delete(key)
        changed = true
      }
    }
    if (changed) {
      bump()
    }
    return changed
  }

  function scheduleTypingPrune(): void {
    if (pruneTimer !== null) {
      clearTimeout(pruneTimer)
      pruneTimer = null
    }
    let nextExpiry: number | null = null
    for (const entry of typing.values()) {
      if (nextExpiry === null || entry.expiresAt < nextExpiry) {
        nextExpiry = entry.expiresAt
      }
    }
    if (nextExpiry === null) {
      return
    }
    pruneTimer = setTimeout(
      () => {
        pruneTimer = null
        pruneTyping()
        scheduleTypingPrune()
      },
      Math.max(0, nextExpiry - now()),
    )
  }

  return {
    revision: computed(() => revision.value),

    apply(accountId, event) {
      if (event.roomId === null) {
        return
      }
      if (event.eventType === 'm.typing') {
        const userIds = parseTypingContent(event.content)
        if (userIds === null) {
          return
        }
        const roomKey = keyOf(accountId, event.roomId)
        if (userIds.length === 0) {
          typing.delete(roomKey)
        } else {
          typing.set(roomKey, { userIds, expiresAt: now() + TYPING_TTL_MS })
        }
        scheduleTypingPrune()
        bump()
        return
      }
      if (event.eventType === 'm.receipt') {
        const parsed = parseReceiptContent(event.content)
        if (parsed.size === 0) {
          return
        }
        applyReceiptUpdate(accountId, event.roomId, parsed)
        bump()
      }
    },

    typingUsers(accountId, roomId, ownUserId = null) {
      if (pruneTyping()) {
        scheduleTypingPrune()
      }
      const entry = typing.get(keyOf(accountId, roomId))
      if (entry === undefined) {
        return []
      }
      return entry.userIds.filter((userId) => userId !== ownUserId)
    },

    readReceipts(accountId, roomId, eventId, ownUserId = null) {
      const entry = receipts.get(eventKeyOf(accountId, roomId, eventId))
      if (entry === undefined) {
        return []
      }
      return [...entry.publicRead.entries()]
        .filter(([userId]) => userId !== ownUserId)
        .sort(([, leftTs], [, rightTs]) => (rightTs ?? 0) - (leftTs ?? 0))
        .map(([userId, ts]) => ({ userId, ts }))
    },

    clearTyping() {
      if (typing.size === 0) {
        return
      }
      if (pruneTimer !== null) {
        clearTimeout(pruneTimer)
        pruneTimer = null
      }
      typing.clear()
      bump()
    },
  }

  function applyReceiptUpdate(
    accountId: string,
    roomId: string,
    parsed: ReadonlyMap<string, ReceiptEntry>,
  ): void {
    for (const [eventId, receipt] of parsed) {
      for (const userId of receipt.publicRead.keys()) {
        deleteReceiptForUser(accountId, roomId, userId, 'public')
      }
      for (const userId of receipt.privateRead.keys()) {
        deleteReceiptForUser(accountId, roomId, userId, 'private')
      }
      const key = eventKeyOf(accountId, roomId, eventId)
      const existing = receipts.get(key)
      receipts.set(key, {
        publicRead: new Map([
          ...(existing?.publicRead.entries() ?? []),
          ...receipt.publicRead.entries(),
        ]),
        privateRead: new Map([
          ...(existing?.privateRead.entries() ?? []),
          ...receipt.privateRead.entries(),
        ]),
      })
    }
  }

  function deleteReceiptForUser(
    accountId: string,
    roomId: string,
    userId: string,
    kind: 'public' | 'private',
  ): void {
    const roomPrefix = `${keyOf(accountId, roomId)}\u0000`
    for (const [key, receipt] of receipts) {
      if (!key.startsWith(roomPrefix)) {
        continue
      }
      const publicRead = new Map(receipt.publicRead)
      const privateRead = new Map(receipt.privateRead)
      if (kind === 'public') {
        publicRead.delete(userId)
      } else {
        privateRead.delete(userId)
      }
      if (publicRead.size === 0 && privateRead.size === 0) {
        receipts.delete(key)
      } else {
        receipts.set(key, { publicRead, privateRead })
      }
    }
  }
}

export function parseTypingContent(content: unknown): readonly string[] | null {
  if (typeof content !== 'object' || content === null) {
    return null
  }
  const userIds = (content as Record<string, unknown>).user_ids
  if (!Array.isArray(userIds)) {
    return null
  }
  const unique = new Set<string>()
  for (const userId of userIds) {
    if (typeof userId !== 'string') {
      return null
    }
    unique.add(userId)
  }
  return [...unique]
}

/** The receipt types Matrix defines for a read position. */
const RECEIPT_TYPES = ['m.read', 'm.read.private'] as const

/** One user's receipt on one event, as it appears on the wire. */
export interface ReceiptRecord {
  eventId: string
  receiptType: (typeof RECEIPT_TYPES)[number]
  userId: string
  ts: number | null
  /** MSC3771 thread scope: a thread root, the literal `"main"`, or absent. */
  threadId: string | null
}

/**
 * Walk an `m.receipt` content object, yielding one record per
 * `content[eventId][receiptType][userId]`.
 *
 * The single place that knows this wire shape. Consumers want different slices
 * of it — the ephemeral overlay wants every user's timestamp per event to render
 * read avatars, `connectThreadReceipts` wants this user's thread-scoped ones —
 * and both used to walk it separately, so a fix to one had no reason to reach
 * the other (review, non-blocking).
 */
export function* walkReceiptContent(
  content: unknown,
): Generator<ReceiptRecord> {
  if (typeof content !== 'object' || content === null) {
    return
  }
  for (const [eventId, receiptTypes] of Object.entries(
    content as Record<string, unknown>,
  )) {
    if (typeof receiptTypes !== 'object' || receiptTypes === null) {
      continue
    }
    for (const receiptType of RECEIPT_TYPES) {
      const byUser = (receiptTypes as Record<string, unknown>)[receiptType]
      if (typeof byUser !== 'object' || byUser === null) {
        continue
      }
      for (const [userId, metadata] of Object.entries(
        byUser as Record<string, unknown>,
      )) {
        const fields =
          typeof metadata === 'object' && metadata !== null
            ? (metadata as Record<string, unknown>)
            : {}
        yield {
          eventId,
          receiptType,
          userId,
          ts: typeof fields.ts === 'number' ? fields.ts : null,
          threadId:
            typeof fields.thread_id === 'string' ? fields.thread_id : null,
        }
      }
    }
  }
}

export function parseReceiptContent(
  content: unknown,
): ReadonlyMap<string, ReceiptEntry> {
  const parsed = new Map<
    string,
    {
      publicRead: Map<string, number | null>
      privateRead: Map<string, number | null>
    }
  >()
  for (const record of walkReceiptContent(content)) {
    let entry = parsed.get(record.eventId)
    if (entry === undefined) {
      entry = { publicRead: new Map(), privateRead: new Map() }
      parsed.set(record.eventId, entry)
    }
    const target =
      record.receiptType === 'm.read' ? entry.publicRead : entry.privateRead
    target.set(record.userId, record.ts)
  }
  return parsed
}
