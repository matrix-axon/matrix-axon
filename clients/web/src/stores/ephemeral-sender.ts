import type { ApiClient } from '../api/client'
import { inBackground } from '../api/client'

/**
 * Outbound Matrix ephemeral state — the send side of `stores/ephemeral.ts`'s
 * inbound overlay (ADR 0067 read receipts, ADR 0068 M19a typing). Both routes
 * are best-effort, fire-and-forget: a failure is swallowed (WCR-02), the local
 * read/compose UX never depends on the homeserver acking.
 *
 * - **Read receipts** piggyback on the room-read choke point (`RoomPage`'s
 *   read-marker effect, which already advances the cross-device marker
 *   forward-only). This store keeps its own forward-only floor and a debounce
 *   so a burst of newly-read events collapses to one `POST …/read` for the
 *   newest event — mirroring how the device-state PUT is debounced, per
 *   ADR 0067's "same call site and timer".
 *
 *   That floor is keyed on `arrival_order`, **not** `origin_ts` (ADR 0089). A
 *   receipt is interpreted by the homeserver in arrival order while every axon
 *   read surface sorts by `origin_ts`, and the two disagree whenever a bridge
 *   backfills a conversation into a freshly created portal. An `origin_ts`
 *   floor could then never name the backfilled message at all — its timestamp
 *   sits *below* a floor already sent — which is half of the bug ADR 0089
 *   fixes. `RoomPage` picks the target; this store only orders it.
 * - **Typing** is driven by composer activity: `noteTyping` on each keystroke
 *   (throttled `typing:true` + an idle timer), `stopTyping` on send, idle, or
 *   room switch (`typing:false`).
 */
export interface EphemeralSender {
  /**
   * The room was read up to `(eventId, arrivalOrder)` — the greatest
   * `arrival_order` among the events the caller has displayed (ADR 0089).
   * Forward-only *in arrival order* (an earlier or equal position is ignored)
   * and debounced; the settled event is sent as a public + private read receipt.
   */
  noteRead(
    accountId: string,
    roomId: string,
    eventId: string,
    arrivalOrder: number,
  ): void
  /**
   * The user is composing in this room (a non-empty, non-command draft). Sends
   * `typing:true` (throttled) and arms an idle timer that stops typing after a
   * pause.
   */
  noteTyping(accountId: string, roomId: string): void
  /** The user stopped composing (send, idle, blur, or room switch). Sends
   * `typing:false` if a notice is currently active. Idempotent. */
  stopTyping(accountId: string, roomId: string): void
}

/** How long a burst of newly-read events settles before its receipt is sent. */
export const RECEIPT_DEBOUNCE_MS = 800
/** Minimum gap between `typing:true` refreshes while typing continues. Kept
 *  below the homeserver's typing timeout so the notice never lapses mid-type. */
export const TYPING_REFRESH_MS = 4000
/** Inactivity after the last keystroke before `typing:false` is sent. */
export const TYPING_IDLE_MS = 4000

const SEP = '\0'
const keyOf = (accountId: string, roomId: string) =>
  `${accountId}${SEP}${roomId}`

interface PendingReceipt {
  eventId: string
  arrivalOrder: number
}

interface TypingState {
  active: boolean
  lastTrueSentAt: number
  idleTimer: ReturnType<typeof setTimeout> | null
}

export function createEphemeralSender(
  api: ApiClient,
  now: () => number = () => Date.now(),
): EphemeralSender {
  /** The greatest `arrival_order` already sent as a receipt, per room. */
  const receiptFloor = new Map<string, number>()
  const pendingReceipt = new Map<string, PendingReceipt>()
  const receiptTimers = new Map<string, ReturnType<typeof setTimeout>>()
  const typing = new Map<string, TypingState>()

  function flushReceipt(key: string, accountId: string, roomId: string): void {
    receiptTimers.delete(key)
    const pending = pendingReceipt.get(key)
    pendingReceipt.delete(key)
    if (pending === undefined) {
      return
    }
    receiptFloor.set(key, pending.arrivalOrder)
    inBackground(
      api.POST('/v1/accounts/{account_id}/rooms/{room_id}/read', {
        params: { path: { account_id: accountId, room_id: roomId } },
        body: { event_id: pending.eventId },
      }),
    )
  }

  function sendTyping(
    accountId: string,
    roomId: string,
    isTyping: boolean,
  ): void {
    inBackground(
      api.PUT('/v1/accounts/{account_id}/rooms/{room_id}/typing', {
        params: { path: { account_id: accountId, room_id: roomId } },
        body: { typing: isTyping },
      }),
    )
  }

  function noteRead(
    accountId: string,
    roomId: string,
    eventId: string,
    arrivalOrder: number,
  ): void {
    const key = keyOf(accountId, roomId)
    const floor = Math.max(
      receiptFloor.get(key) ?? -1,
      pendingReceipt.get(key)?.arrivalOrder ?? -1,
    )
    if (arrivalOrder <= floor) {
      return
    }
    pendingReceipt.set(key, { eventId, arrivalOrder })
    const existing = receiptTimers.get(key)
    if (existing !== undefined) {
      clearTimeout(existing)
    }
    receiptTimers.set(
      key,
      setTimeout(
        () => flushReceipt(key, accountId, roomId),
        RECEIPT_DEBOUNCE_MS,
      ),
    )
  }

  function stopTyping(accountId: string, roomId: string): void {
    const state = typing.get(keyOf(accountId, roomId))
    if (state === undefined) {
      return
    }
    if (state.idleTimer !== null) {
      clearTimeout(state.idleTimer)
      state.idleTimer = null
    }
    if (state.active) {
      state.active = false
      state.lastTrueSentAt = 0
      sendTyping(accountId, roomId, false)
    }
  }

  function noteTyping(accountId: string, roomId: string): void {
    const key = keyOf(accountId, roomId)
    const state = typing.get(key) ?? {
      active: false,
      lastTrueSentAt: 0,
      idleTimer: null,
    }
    typing.set(key, state)
    if (state.idleTimer !== null) {
      clearTimeout(state.idleTimer)
    }
    state.idleTimer = setTimeout(
      () => stopTyping(accountId, roomId),
      TYPING_IDLE_MS,
    )
    if (!state.active || now() - state.lastTrueSentAt >= TYPING_REFRESH_MS) {
      state.active = true
      state.lastTrueSentAt = now()
      sendTyping(accountId, roomId, true)
    }
  }

  return {
    noteRead,
    noteTyping,
    stopTyping,
  }
}
