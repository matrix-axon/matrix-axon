import type { ApiClient } from '../api/client'
import { inBackground, isTransientStatus } from '../api/client'

/**
 * Outbound Matrix ephemeral state — the send side of `stores/ephemeral.ts`'s
 * inbound overlay (ADR 0067 read receipts, ADR 0068 M19a typing). Both routes
 * are best-effort: a failure is never surfaced (WCR-02), and the local
 * read/compose UX never depends on the homeserver acking. Typing is
 * fire-and-forget; a receipt is retried, because it is durable state.
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
 *
 *   The floor only moves when the server **accepts** a receipt (#625). A floor
 *   advanced before the POST settled treated a failed receipt as sent: every
 *   later `noteRead` for that position was ignored, Synapse kept counting the
 *   messages, and the room badge came back after every local clear — for the
 *   life of the page, which on a resumed app is indefinitely. A transient
 *   failure (no answer, a 5xx, a 429) is retried on `RECEIPT_RETRY_DELAYS_MS`,
 *   one request in flight per room, and each retry sends the room's newest
 *   noted position rather than the one that failed. Once the retries run out,
 *   or the server gives a verdict (another 4xx), that position is left alone
 *   for `RECEIPT_GIVE_UP_COOLDOWN_MS`; after that, reading the room again sends
 *   it again.
 * - **Typing** is driven by composer activity: `noteTyping` on each keystroke
 *   (throttled `typing:true` + an idle timer), `stopTyping` on send, idle, or
 *   room switch (`typing:false`).
 */
export interface EphemeralSender {
  /**
   * The room was read up to `(eventId, arrivalOrder)` — the greatest
   * `arrival_order` among the events the caller has displayed (ADR 0089).
   * Forward-only *in arrival order* (a position at or behind one the server
   * accepted, or one already pending or in flight, is ignored) and debounced;
   * the settled event is sent as a public + private read receipt, and retried
   * if it fails transiently.
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
/**
 * The wait before each retry of a receipt that failed transiently: three
 * retries, four attempts in all. Spaced out because a receipt that fails is
 * usually failing on a homeserver that is already struggling, and the original
 * may still land — axon's timeout drops its own request, not Synapse's — which
 * is harmless, since receipts only move forward.
 */
export const RECEIPT_RETRY_DELAYS_MS: readonly number[] = [
  2_000, 10_000, 30_000,
]
/**
 * After a receipt is given up on, how long the same position is ignored. It
 * bounds a homeserver outage to one round of attempts per room per window,
 * however often the room view re-notes the same event.
 */
export const RECEIPT_GIVE_UP_COOLDOWN_MS = 60_000
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

/** One room's receipt pipeline. Owned by its `createEphemeralSender` closure. */
interface ReceiptState {
  /** The greatest `arrival_order` the server accepted as a receipt. */
  acked: number
  /** The newest noted position not yet sent, waiting on the debounce, the
   *  in-flight request, or a retry. */
  pending: PendingReceipt | null
  debounce: ReturnType<typeof setTimeout> | null
  /** The request on the wire. At most one per room. */
  inFlight: PendingReceipt | null
  /** A transient failure waiting out its backoff; `attempt` is the index of
   *  the attempt that failed. */
  retry: {
    receipt: PendingReceipt
    attempt: number
    timer: ReturnType<typeof setTimeout>
  } | null
  /** The last position given up on, and when — see the cooldown. */
  gaveUp: { arrivalOrder: number; at: number } | null
}

type ReceiptOutcome = 'accepted' | 'transient' | 'rejected'

interface TypingState {
  active: boolean
  lastTrueSentAt: number
  idleTimer: ReturnType<typeof setTimeout> | null
}

export function createEphemeralSender(
  api: ApiClient,
  now: () => number = () => Date.now(),
): EphemeralSender {
  const receipts = new Map<string, ReceiptState>()
  const typing = new Map<string, TypingState>()

  function receiptState(key: string): ReceiptState {
    let state = receipts.get(key)
    if (state === undefined) {
      state = {
        acked: -1,
        pending: null,
        debounce: null,
        inFlight: null,
        retry: null,
        gaveUp: null,
      }
      receipts.set(key, state)
    }
    return state
  }

  async function postReceipt(
    accountId: string,
    roomId: string,
    eventId: string,
  ): Promise<ReceiptOutcome> {
    let response: Response
    try {
      response = (
        await api.POST('/v1/accounts/{account_id}/rooms/{room_id}/read', {
          params: { path: { account_id: accountId, room_id: roomId } },
          body: { event_id: eventId },
        })
      ).response
    } catch {
      // No answer at all: offline, a dropped connection, or the client's own
      // request deadline — which is shorter than the server's receipt timeout,
      // so a slow homeserver usually lands here rather than as a 502.
      return 'transient'
    }
    // openapi-fetch *resolves* on an HTTP error, so read the status.
    if (response.ok) {
      return 'accepted'
    }
    return isTransientStatus(response.status) ? 'transient' : 'rejected'
  }

  function sendReceipt(
    key: string,
    accountId: string,
    roomId: string,
    receipt: PendingReceipt,
    attempt: number,
  ): void {
    const state = receiptState(key)
    state.inFlight = receipt
    void postReceipt(accountId, roomId, receipt.eventId).then((outcome) => {
      state.inFlight = null
      if (outcome === 'accepted') {
        state.acked = Math.max(state.acked, receipt.arrivalOrder)
        state.gaveUp = null
      } else if (
        outcome === 'transient' &&
        attempt < RECEIPT_RETRY_DELAYS_MS.length
      ) {
        state.retry = {
          receipt,
          attempt,
          timer: setTimeout(
            () => retryReceipt(key, accountId, roomId),
            RECEIPT_RETRY_DELAYS_MS[attempt],
          ),
        }
        return
      } else {
        state.gaveUp = { arrivalOrder: receipt.arrivalOrder, at: now() }
      }
      // A newer position noted while this one was on the wire, whose debounce
      // already fired into the in-flight guard.
      if (state.pending !== null && state.debounce === null) {
        flushReceipt(key, accountId, roomId)
      }
    })
  }

  function retryReceipt(key: string, accountId: string, roomId: string): void {
    const state = receiptState(key)
    const failed = state.retry
    state.retry = null
    if (failed === null) {
      return
    }
    // The newest position, not the one that failed: `noteRead` only accepts a
    // pending position ahead of the failed one, and naming it covers both.
    const receipt = state.pending ?? failed.receipt
    state.pending = null
    sendReceipt(key, accountId, roomId, receipt, failed.attempt + 1)
  }

  function flushReceipt(key: string, accountId: string, roomId: string): void {
    const state = receiptState(key)
    state.debounce = null
    // The request on the wire, or the retry waiting out its backoff, picks up
    // `pending` when it settles.
    if (state.inFlight !== null || state.retry !== null) {
      return
    }
    const receipt = state.pending
    state.pending = null
    if (receipt === null || receipt.arrivalOrder <= state.acked) {
      return
    }
    sendReceipt(key, accountId, roomId, receipt, 0)
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
    const state = receiptState(key)
    const floor = Math.max(
      state.acked,
      state.pending?.arrivalOrder ?? -1,
      state.inFlight?.arrivalOrder ?? -1,
      state.retry?.receipt.arrivalOrder ?? -1,
    )
    if (arrivalOrder <= floor) {
      return
    }
    if (
      state.gaveUp !== null &&
      arrivalOrder <= state.gaveUp.arrivalOrder &&
      now() - state.gaveUp.at < RECEIPT_GIVE_UP_COOLDOWN_MS
    ) {
      return
    }
    state.pending = { eventId, arrivalOrder }
    if (state.debounce !== null) {
      clearTimeout(state.debounce)
    }
    state.debounce = setTimeout(
      () => flushReceipt(key, accountId, roomId),
      RECEIPT_DEBOUNCE_MS,
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
