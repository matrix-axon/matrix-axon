import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { createApiClient } from '../api/client'
import {
  createEphemeralSender,
  RECEIPT_DEBOUNCE_MS,
  RECEIPT_GIVE_UP_COOLDOWN_MS,
  RECEIPT_RETRY_DELAYS_MS,
  TYPING_IDLE_MS,
  TYPING_REFRESH_MS,
} from './ephemeral-sender'

const BASE_URL = 'http://axon.test'
const ACCT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const OTHER_ROOM = '!other:hs'
const READ_PATH = `${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/read`
const TYPING_PATH = `${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/typing`

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  server.resetHandlers()
  vi.useRealTimers()
})
afterAll(() => server.close())

function makeApi() {
  return createApiClient(
    {
      getToken: () => 'tok',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
}

/** Collect the `event_id`s the sender POSTs to `.../read`. */
function captureReads(): string[] {
  const reads: string[] = []
  server.use(
    http.post(READ_PATH, async ({ request }) => {
      const body = (await request.json()) as { event_id: string }
      reads.push(body.event_id)
      return HttpResponse.json({ data: {} })
    }),
  )
  return reads
}

/**
 * Like `captureReads`, but each POST is answered by the next entry of
 * `answers` (the last one repeats): a status, or `'network'` for a request
 * that never gets an answer.
 */
function scriptReads(answers: readonly (number | 'network')[]): string[] {
  const reads: string[] = []
  server.use(
    http.post(READ_PATH, async ({ request }) => {
      const body = (await request.json()) as { event_id: string }
      reads.push(body.event_id)
      const answer = answers[Math.min(reads.length, answers.length) - 1]
      if (answer === 'network') {
        return HttpResponse.error()
      }
      return HttpResponse.json(
        answer < 400 ? { data: {} } : { error: { code: 'upstream' } },
        { status: answer },
      )
    }),
  )
  return reads
}

/** Total backoff across every retry, so a test can run a round out. */
const ALL_RETRIES_MS = RECEIPT_RETRY_DELAYS_MS.reduce((a, b) => a + b, 0)

/** Collect the `typing` flags the sender PUTs to `.../typing`. */
function captureTyping(): boolean[] {
  const flags: boolean[] = []
  server.use(
    http.put(TYPING_PATH, async ({ request }) => {
      const body = (await request.json()) as { typing: boolean }
      flags.push(body.typing)
      return HttpResponse.json({ data: {} })
    }),
  )
  return flags
}

describe('read receipts', () => {
  it('debounces a burst to one receipt for the newest event', async () => {
    vi.useFakeTimers()
    const reads = captureReads()
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    sender.noteRead(ACCT, ROOM, '$b', 200)
    // Nothing sent before the debounce elapses.
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS - 1)
    expect(reads).toEqual([])
    await vi.advanceTimersByTimeAsync(1)
    expect(reads).toEqual(['$b'])
  })

  it('ignores backward or equal positions (forward-only)', async () => {
    vi.useFakeTimers()
    const reads = captureReads()
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$b', 200)
    sender.noteRead(ACCT, ROOM, '$older', 50) // behind the pending one → dropped
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$b'])

    // Equal to what was already sent → no new receipt.
    sender.noteRead(ACCT, ROOM, '$b-again', 200)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$b'])

    // Strictly ahead → sends again.
    sender.noteRead(ACCT, ROOM, '$c', 300)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$b', '$c'])
  })

  // ADR 0089. The floor orders on `arrival_order`, not `origin_ts`, and the
  // whole point is the case where the two disagree: a bridge backfills a
  // conversation into a freshly created portal, so the message arrives *after*
  // the portal's own state events while being stamped *before* them. A floor
  // kept on `origin_ts` could never name that message — its timestamp sits below
  // a floor already sent — which is why re-reading the room never cleared it.
  it('receipts a later-arriving event whose origin_ts is older', async () => {
    vi.useFakeTimers()
    const reads = captureReads()
    const sender = createEphemeralSender(makeApi())

    // The portal's `uk.half-shot.bridge` state event: arrival 1_871_424,
    // origin_ts 1_785_928_309_453.
    sender.noteRead(ACCT, ROOM, '$bridge', 1_871_424)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$bridge'])

    // The backfilled message: arrived later (1_871_426) but stamped 1.6s
    // *earlier* (origin_ts 1_785_928_304_987). It must still be receipted.
    sender.noteRead(ACCT, ROOM, '$message', 1_871_426)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$bridge', '$message'])
  })

  it('keeps a separate floor per room', async () => {
    vi.useFakeTimers()
    const reads = captureReads()
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$high', 900)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    // A lower arrival order in a *different* room is not behind anything.
    sender.noteRead(ACCT, OTHER_ROOM, '$low', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$high', '$low'])
  })
})

// #625. The floor used to advance before the POST settled, so a receipt that
// failed once was treated as sent: every later visit to the room re-noted the
// same position, the sender ignored it, and the homeserver kept counting the
// messages for the life of the page.
describe('read receipt failures', () => {
  it('retries a transient failure and holds the floor once accepted', async () => {
    vi.useFakeTimers()
    const reads = scriptReads([502, 200])
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$a'])
    await vi.advanceTimersByTimeAsync(RECEIPT_RETRY_DELAYS_MS[0])
    expect(reads).toEqual(['$a', '$a'])

    // Accepted, so the same position is now behind the floor.
    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(ALL_RETRIES_MS)
    expect(reads).toEqual(['$a', '$a'])
  })

  it('treats a request that got no answer as transient', async () => {
    vi.useFakeTimers()
    const reads = scriptReads(['network', 200])
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(
      RECEIPT_DEBOUNCE_MS + RECEIPT_RETRY_DELAYS_MS[0],
    )
    expect(reads).toEqual(['$a', '$a'])
  })

  it('retries with the newest position noted during the backoff', async () => {
    vi.useFakeTimers()
    const reads = scriptReads([503, 200])
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    sender.noteRead(ACCT, ROOM, '$b', 200)
    // The debounce fires into the waiting retry instead of a second request.
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$a'])
    await vi.advanceTimersByTimeAsync(RECEIPT_RETRY_DELAYS_MS[0])
    expect(reads).toEqual(['$a', '$b'])
    // `$a` is covered by `$b`, so nothing is left to send.
    await vi.advanceTimersByTimeAsync(ALL_RETRIES_MS)
    expect(reads).toEqual(['$a', '$b'])
  })

  it('keeps one request in flight per room', async () => {
    vi.useFakeTimers()
    const reads: string[] = []
    let answerFirst: (() => void) | null = null
    server.use(
      http.post(READ_PATH, async ({ request }) => {
        const body = (await request.json()) as { event_id: string }
        reads.push(body.event_id)
        if (reads.length === 1) {
          await new Promise<void>((resolve) => {
            answerFirst = resolve
          })
        }
        return HttpResponse.json({ data: {} })
      }),
    )
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    sender.noteRead(ACCT, ROOM, '$b', 200)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$a'])

    answerFirst!()
    await vi.advanceTimersByTimeAsync(0)
    await vi.waitFor(() => expect(reads).toEqual(['$a', '$b']))
  })

  it('gives up after the retries, then re-sends once the cooldown passes', async () => {
    vi.useFakeTimers()
    const reads = scriptReads([502])
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS + ALL_RETRIES_MS)
    const attempts = RECEIPT_RETRY_DELAYS_MS.length + 1
    expect(reads).toEqual(Array<string>(attempts).fill('$a'))
    await vi.advanceTimersByTimeAsync(ALL_RETRIES_MS)
    expect(reads).toHaveLength(attempts)

    // Re-reading the room inside the cooldown does not start another round…
    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toHaveLength(attempts)

    // …but a newer position is not held back by it.
    sender.noteRead(ACCT, ROOM, '$b', 200)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads.at(-1)).toBe('$b')
  })

  it('re-sends a failed position after the cooldown (the floor did not move)', async () => {
    vi.useFakeTimers()
    const reads = scriptReads([502, 502, 502, 502, 200])
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS + ALL_RETRIES_MS)
    await vi.advanceTimersByTimeAsync(RECEIPT_GIVE_UP_COOLDOWN_MS)

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toHaveLength(RECEIPT_RETRY_DELAYS_MS.length + 2)
  })

  it('does not retry a 4xx verdict, but does not count it as sent', async () => {
    vi.useFakeTimers()
    const reads = scriptReads([400])
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS + ALL_RETRIES_MS)
    expect(reads).toEqual(['$a'])

    await vi.advanceTimersByTimeAsync(RECEIPT_GIVE_UP_COOLDOWN_MS)
    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$a', '$a'])
  })
})

// Review on #751. The sender is app-wide and outlives a sign-out. A retry that
// fired afterwards went out with whatever credentials existed then; its 401
// reached `onAuthFailure()`, which discarded the token of a session signed in
// meanwhile.
describe('session reset', () => {
  it('cancels a retry waiting out its backoff', async () => {
    vi.useFakeTimers()
    const reads = scriptReads([502, 200])
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$a'])

    sender.resetSession()
    await vi.advanceTimersByTimeAsync(ALL_RETRIES_MS)
    expect(reads).toEqual(['$a'])
  })

  it('cancels a receipt still in its debounce', async () => {
    vi.useFakeTimers()
    const reads = captureReads()
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    sender.resetSession()
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS + ALL_RETRIES_MS)
    expect(reads).toEqual([])
  })

  it('aborts the request on the wire, and its completion schedules nothing', async () => {
    vi.useFakeTimers()
    const signals: AbortSignal[] = []
    // A homeserver that never answers: the request settles only by abort.
    const api = createApiClient(
      {
        getToken: () => 'tok',
        onAuthFailure: () => {},
        LoginBootstrap: () => null,
      },
      BASE_URL,
      {
        fetch: (input) => {
          signals.push((input as Request).signal)
          return new Promise<Response>(() => {})
        },
      },
      0,
    )
    const sender = createEphemeralSender(api)

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(signals).toHaveLength(1)
    expect(signals[0].aborted).toBe(false)

    sender.resetSession()
    expect(signals[0].aborted).toBe(true)
    // The abort rejects the POST, which reads as transient; settling it under
    // the old session must not arm a retry in the new one.
    await vi.advanceTimersByTimeAsync(ALL_RETRIES_MS)
    expect(signals).toHaveLength(1)
  })

  it('starts the next session with no floor', async () => {
    vi.useFakeTimers()
    const reads = captureReads()
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    sender.resetSession()

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toEqual(['$a', '$a'])
  })

  // The room's state is rebuilt by the next session, so an old timer left armed
  // would fire into *that* state rather than into nothing.
  it('does not let an old retry timer fire a retry of the next session early', async () => {
    vi.useFakeTimers()
    const reads = scriptReads([502])
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    // The old session's retry is due RECEIPT_RETRY_DELAYS_MS[0] from here.
    await vi.advanceTimersByTimeAsync(100)
    sender.resetSession()
    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS)
    expect(reads).toHaveLength(2)

    // Past the old retry's due time, short of the new one's.
    await vi.advanceTimersByTimeAsync(RECEIPT_RETRY_DELAYS_MS[0] - 100)
    expect(reads).toHaveLength(2)
    await vi.advanceTimersByTimeAsync(100)
    expect(reads).toHaveLength(3)
  })

  it('does not let an old debounce send a receipt of the next session early', async () => {
    vi.useFakeTimers()
    const reads = captureReads()
    const sender = createEphemeralSender(makeApi())

    sender.noteRead(ACCT, ROOM, '$a', 100)
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS / 2)
    sender.resetSession()
    sender.noteRead(ACCT, ROOM, '$b', 200)
    // Past the old debounce, short of the new one.
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS / 2 + 1)
    expect(reads).toEqual([])
    await vi.advanceTimersByTimeAsync(RECEIPT_DEBOUNCE_MS / 2)
    expect(reads).toEqual(['$b'])
  })

  it('does not let an old idle timer stop typing in the next session', async () => {
    vi.useFakeTimers()
    const flags = captureTyping()
    const sender = createEphemeralSender(makeApi())

    sender.noteTyping(ACCT, ROOM)
    await vi.advanceTimersByTimeAsync(TYPING_IDLE_MS / 2)
    sender.resetSession()
    sender.noteTyping(ACCT, ROOM)
    // Past the old idle deadline, short of the new one.
    await vi.advanceTimersByTimeAsync(TYPING_IDLE_MS / 2 + 1)
    expect(flags).toEqual([true, true])
  })

  it('cancels the typing idle timer without sending typing:false', async () => {
    vi.useFakeTimers()
    const flags = captureTyping()
    const sender = createEphemeralSender(makeApi())

    sender.noteTyping(ACCT, ROOM)
    await vi.advanceTimersByTimeAsync(0)
    expect(flags).toEqual([true])

    sender.resetSession()
    await vi.advanceTimersByTimeAsync(TYPING_IDLE_MS)
    expect(flags).toEqual([true])
  })
})

describe('typing notices', () => {
  it('sends typing:true once per throttle window and false on stop', async () => {
    vi.useFakeTimers()
    const flags = captureTyping()
    const sender = createEphemeralSender(makeApi())

    sender.noteTyping(ACCT, ROOM)
    sender.noteTyping(ACCT, ROOM) // within throttle → no second true
    await vi.advanceTimersByTimeAsync(0)
    expect(flags).toEqual([true])

    sender.stopTyping(ACCT, ROOM)
    await vi.advanceTimersByTimeAsync(0)
    expect(flags).toEqual([true, false])
  })

  it('auto-stops after the idle window', async () => {
    vi.useFakeTimers()
    const flags = captureTyping()
    const sender = createEphemeralSender(makeApi())

    sender.noteTyping(ACCT, ROOM)
    await vi.advanceTimersByTimeAsync(0)
    expect(flags).toEqual([true])

    await vi.advanceTimersByTimeAsync(TYPING_IDLE_MS)
    expect(flags).toEqual([true, false])
  })

  it('refreshes typing:true while composition continues', async () => {
    vi.useFakeTimers()
    const flags = captureTyping()
    const sender = createEphemeralSender(makeApi())

    sender.noteTyping(ACCT, ROOM)
    await vi.advanceTimersByTimeAsync(0)
    // Keep typing just before idle would fire, deferring the idle stop.
    await vi.advanceTimersByTimeAsync(TYPING_IDLE_MS - 100)
    sender.noteTyping(ACCT, ROOM)
    // Past the refresh window from the first true → resend.
    await vi.advanceTimersByTimeAsync(
      TYPING_REFRESH_MS - (TYPING_IDLE_MS - 100),
    )
    sender.noteTyping(ACCT, ROOM)
    await vi.advanceTimersByTimeAsync(0)
    expect(flags).toEqual([true, true])
  })

  it('stopTyping is a no-op when no notice is active', async () => {
    vi.useFakeTimers()
    const flags = captureTyping()
    const sender = createEphemeralSender(makeApi())

    sender.stopTyping(ACCT, ROOM)
    await vi.advanceTimersByTimeAsync(0)
    expect(flags).toEqual([])
  })
})
