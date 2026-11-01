import { http, HttpResponse } from 'msw'
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
import { memoryStorage } from '../test/memory-storage'
import {
  createInvitesStore,
  REJECT_PENDING_MESSAGE,
  type InviteDto,
} from './invites'
import { createRoomsStore, PENDING_MEMBERSHIP_REFRESH_DELAYS_MS } from './rooms'

const BASE = 'http://axon.test'
const ACCOUNT = '11111111-1111-1111-1111-111111111111'

function invite(roomId: string, name: string): InviteDto {
  return {
    account_id: ACCOUNT,
    account_user_id: '@me:hs',
    room_id: roomId,
    name,
    avatar_url: null,
    topic: null,
    canonical_alias: null,
    room_type: null,
    inviter_user_id: '@alice:hs',
    inviter_display_name: 'Alice',
    is_direct: false,
    encrypted: true,
    invited_at: '2026-08-14T12:00:00+00:00',
  }
}

describe('invites store', () => {
  const server = setupServer()
  beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
  afterEach(() => server.resetHandlers())
  afterAll(() => server.close())

  function store() {
    const api = createApiClient(
      {
        getToken: () => 'tok-test',
        onAuthFailure: () => {},
        LoginBootstrap: () => null,
      },
      BASE,
    )
    const rooms = createRoomsStore(api, memoryStorage())
    return createInvitesStore(api, rooms)
  }

  it('loads invites and reports the count', async () => {
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
    )
    const invites = store()
    await invites.refresh()
    expect(invites.count.value).toBe(1)
    expect(invites.invites.value[0].name).toBe('Alpha')
    expect(invites.loading.value).toBe(false)
  })

  it('drops a row on accept and calls join', async () => {
    const joined: string[] = []
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          const body = (await request.json()) as { room_id_or_alias: string }
          joined.push(body.room_id_or_alias)
          return HttpResponse.json({ data: { room_id: body.room_id_or_alias } })
        },
      ),
      http.get(`${BASE}/v1/rooms`, () => HttpResponse.json({ data: [] })),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.accept(invites.invites.value[0])
    expect(result).toEqual({ ok: true })
    expect(joined).toEqual(['!a:hs'])
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!b:hs'])
  })

  it('drops a row on reject and ignores a stale GET that still lists it', async () => {
    const left: string[] = []
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          left.push(String(params.roomId))
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.reject(invites.invites.value[0])
    expect(result).toEqual({ ok: true })
    expect(left).toEqual(['!a:hs'])
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!b:hs'])
  })

  it('keeps the row when reject fails', async () => {
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
      http.post(`${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'room not found' } },
          { status: 404 },
        ),
      ),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.reject(invites.invites.value[0])
    expect(result).toEqual({ ok: false, message: 'room not found' })
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
    expect(invites.rejectPending(invite('!a:hs', 'Alpha'))).toBe(false)
  })

  it('rejectAll leaves every invite even when a later GET is stale', async () => {
    const left: string[] = []
    let inviteReads = 0
    server.use(
      http.get(`${BASE}/v1/invites`, () => {
        inviteReads += 1
        // After both leaves, a lagged watcher still returns the second row.
        if (left.length >= 2) {
          return HttpResponse.json({ data: [invite('!b:hs', 'Beta')] })
        }
        return HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        })
      }),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          left.push(String(params.roomId))
          return HttpResponse.json({ data: {} })
        },
      ),
      http.get(`${BASE}/v1/rooms`, () => HttpResponse.json({ data: [] })),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.rejectAll()
    expect(result).toEqual({ succeeded: 2, pending: 0, failed: [] })
    expect(left).toEqual(['!a:hs', '!b:hs'])
    expect(inviteReads).toBeGreaterThan(1)
    expect(invites.invites.value).toEqual([])
  })

  it('rejects without aborting the rest of rejectAll', async () => {
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        }),
      ),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        ({ params }) => {
          if (params.roomId === '!a:hs') {
            return HttpResponse.json(
              { error: { code: 'forbidden', message: 'nope' } },
              { status: 403 },
            )
          }
          return HttpResponse.json({ data: {} })
        },
      ),
      http.get(`${BASE}/v1/rooms`, () => HttpResponse.json({ data: [] })),
    )
    const invites = store()
    await invites.refresh()
    const result = await invites.rejectAll()
    expect(result.succeeded).toBe(1)
    expect(result.failed).toHaveLength(1)
    expect(result.failed[0].invite.room_id).toBe('!a:hs')
  })

  it('applies live added and removed frames', () => {
    const invites = store()
    invites.noteAdded(invite('!a:hs', 'Alpha'))
    expect(invites.count.value).toBe(1)
    invites.noteRemoved(ACCOUNT, '!a:hs')
    expect(invites.count.value).toBe(0)
  })

  it('a server-driven removal leaves no suppression behind', async () => {
    // `invite.removed` says the row is already gone upstream, so there is
    // nothing for a later GET to wrongly restore and nothing to suppress.
    // Recording it as locally-resolved is a trap: every later GET that does
    // carry the key keeps the suppression alive, so the row can never come
    // back — the ADR's "GET /v1/invites is the reconnect source of truth"
    // stops holding.
    server.use(
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
    )
    const invites = store()
    invites.noteRemoved(ACCOUNT, '!a:hs')
    expect(invites.count.value).toBe(0)
    await invites.refresh()
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
    await invites.refresh()
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
  })

  it('shows a re-invite after a local reject', async () => {
    // Same shape, but the suppression is legitimate at first: we rejected
    // locally, so a lagging GET must not restore the row. An `invite.added`
    // is the server saying it exists *now*, which outranks that.
    server.use(
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        () => new HttpResponse(null, { status: 200 }),
      ),
      http.get(`${BASE}/v1/invites`, () =>
        HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] }),
      ),
    )
    const invites = store()
    await invites.refresh()
    await invites.reject(invite('!a:hs', 'Alpha'))
    expect(invites.count.value).toBe(0)

    invites.noteAdded(invite('!a:hs', 'Alpha'))
    expect(invites.count.value).toBe(1)
    await invites.refresh()
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
  })

  it('keeps an invite added while a GET was in flight', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE}/v1/invites`, async () => {
        await held
        return HttpResponse.json({ data: [invite('!a:hs', 'Alpha')] })
      }),
    )
    const invites = store()
    const pending = invites.refresh()
    // The response was already on the wire, so it cannot carry this one.
    invites.noteAdded(invite('!b:hs', 'Beta'))
    release?.()
    await pending
    expect(invites.invites.value.map((row) => row.room_id).sort()).toEqual([
      '!a:hs',
      '!b:hs',
    ])
  })

  it('does not let a GET issued before a removal frame restore the row', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE}/v1/invites`, async () => {
        await held
        return HttpResponse.json({
          data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
        })
      }),
    )
    const invites = store()
    const pending = invites.refresh()
    invites.noteRemoved(ACCOUNT, '!a:hs')
    // Removed and then re-invited within the same fetch: the later frame wins.
    invites.noteRemoved(ACCOUNT, '!b:hs')
    invites.noteAdded(invite('!b:hs', 'Beta'))
    release?.()
    await pending
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!b:hs'])
  })

  it('does not treat a single live frame as a settled list', async () => {
    // The initial GET fails, then one frame arrives. `rejectAll` must still
    // confirm the list rather than act on the single row it can see.
    let attempt = 0
    server.use(
      http.get(`${BASE}/v1/invites`, () => {
        attempt += 1
        return attempt === 1
          ? new HttpResponse(null, { status: 503 })
          : HttpResponse.json({
              data: [invite('!a:hs', 'Alpha'), invite('!b:hs', 'Beta')],
            })
      }),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        () => new HttpResponse(null, { status: 200 }),
      ),
    )
    const invites = store()
    await invites.refresh()
    expect(invites.error.value).not.toBeNull()

    invites.noteAdded(invite('!a:hs', 'Alpha'))
    expect(invites.count.value).toBe(1)

    const result = await invites.rejectAll()
    expect(result.succeeded).toBe(2)
  })

  it('refresh after a mutation does not resolve against an older GET', async () => {
    const gets: (() => void)[] = []
    let served = 0
    server.use(
      http.get(`${BASE}/v1/invites`, async () => {
        served += 1
        const mine = served
        await new Promise<void>((resolve) => gets.push(resolve))
        // Only the second GET knows the invite is gone.
        return HttpResponse.json({
          data: mine === 1 ? [invite('!a:hs', 'Alpha')] : [],
        })
      }),
      http.post(
        `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        () => new HttpResponse(null, { status: 200 }),
      ),
    )
    /** Wait until the handler for GET number `n` is parked. */
    async function waitForGet(n: number): Promise<void> {
      for (let i = 0; i < 200 && gets.length < n; i += 1) {
        await new Promise((resolve) => setTimeout(resolve, 5))
      }
      expect(gets.length).toBeGreaterThanOrEqual(n)
    }

    const invites = store()
    const first = invites.refresh()
    await waitForGet(1)
    // A second caller arrives while the first GET is still in flight; it must
    // get a fetch issued after this point, not the one already on the wire.
    const second = invites.refresh()
    expect(second).not.toBe(first)

    gets[0]()
    await first
    expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])

    await waitForGet(2)
    gets[1]()
    await second
    expect(served).toBe(2)
    expect(invites.invites.value).toEqual([])
  })

  it('discards a refresh that finishes after resetSession', async () => {
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(`${BASE}/v1/invites`, async () => {
        await held
        return HttpResponse.json({ data: [invite('!a:hs', 'Stale')] })
      }),
    )
    const invites = store()
    const pending = invites.refresh()
    invites.resetSession()
    release?.()
    await pending
    expect(invites.invites.value).toEqual([])
  })

  describe('a reject axon timed out on (ADR 0121, issue #743)', () => {
    const ALPHA = invite('!a:hs', 'Alpha')
    /** The whole pending window: every refresh delay, end to end. */
    const WINDOW_MS = PENDING_MEMBERSHIP_REFRESH_DELAYS_MS.reduce(
      (sum, ms) => sum + ms,
      0,
    )
    const PENDING = {
      ok: false,
      pending: true,
      message: REJECT_PENDING_MESSAGE,
    }

    afterEach(() => {
      vi.useRealTimers()
    })

    /** `/v1/invites` listing `listed()`, a leave that answers 504, and a join
     * that answers 504 too. */
    function useTimedOutReject(listed: () => InviteDto[]) {
      const calls = { list: 0, leave: 0 }
      server.use(
        http.get(`${BASE}/v1/invites`, () => {
          calls.list += 1
          return HttpResponse.json({ data: listed() })
        }),
        http.post(`${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`, () => {
          calls.leave += 1
          return HttpResponse.json(
            {
              error: {
                code: 'gateway_timeout',
                message: 'leave timed out after 30s',
              },
            },
            { status: 504 },
          )
        }),
        http.post(`${BASE}/v1/accounts/${ACCOUNT}/rooms/join`, () =>
          HttpResponse.json(
            {
              error: {
                code: 'gateway_timeout',
                message: 'join timed out after 30s',
              },
            },
            { status: 504 },
          ),
        ),
      )
      return calls
    }

    it('reports a 504 as pending and keeps the row', async () => {
      useTimedOutReject(() => [ALPHA])
      const invites = store()
      await invites.refresh()

      const result = await invites.reject(ALPHA)

      expect(result).toEqual(PENDING)
      // Axon keeps the invite on a 504, and so does the list: the leave may
      // yet fail, and only the homeserver can say it went through.
      expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
      expect(invites.error.value).toBeNull()
      expect(invites.rejectPending(ALPHA)).toBe(true)
    })

    it('keeps a 504 from accept a failure', async () => {
      useTimedOutReject(() => [ALPHA])
      const invites = store()
      await invites.refresh()

      const result = await invites.accept(ALPHA)

      expect(result).toEqual({
        ok: false,
        message: 'join timed out after 30s',
      })
      expect(invites.rejectPending(ALPHA)).toBe(false)
    })

    it('holds back a second reject until the first step, then lets one through', async () => {
      vi.useFakeTimers()
      const calls = useTimedOutReject(() => [ALPHA])
      const invites = store()
      await invites.refresh()
      await invites.reject(ALPHA)

      expect(await invites.reject(ALPHA)).toEqual(PENDING)
      expect(calls.leave).toBe(1)

      await vi.advanceTimersByTimeAsync(PENDING_MEMBERSHIP_REFRESH_DELAYS_MS[0])
      expect(await invites.reject(ALPHA)).toEqual(PENDING)
      expect(calls.leave).toBe(2)
    })

    it('ends on the removal frame the homeserver finishing sends', async () => {
      useTimedOutReject(() => [ALPHA])
      const invites = store()
      await invites.refresh()
      await invites.reject(ALPHA)

      invites.noteRemoved(ACCOUNT, '!a:hs')

      expect(invites.invites.value).toEqual([])
      expect(invites.rejectPending(ALPHA)).toBe(false)
    })

    it('refreshes until the row is gone, then stops', async () => {
      vi.useFakeTimers()
      let listed = [ALPHA]
      const calls = useTimedOutReject(() => listed)
      const invites = store()
      await invites.refresh()
      await invites.reject(ALPHA)
      const before = calls.list

      await vi.advanceTimersByTimeAsync(PENDING_MEMBERSHIP_REFRESH_DELAYS_MS[0])
      expect(calls.list).toBe(before + 1)
      expect(invites.rejectPending(ALPHA)).toBe(true)

      // The removal frame was lost; the next refresh no longer lists it.
      listed = []
      await vi.advanceTimersByTimeAsync(PENDING_MEMBERSHIP_REFRESH_DELAYS_MS[1])
      expect(calls.list).toBe(before + 2)
      expect(invites.invites.value).toEqual([])
      expect(invites.rejectPending(ALPHA)).toBe(false)

      await vi.advanceTimersByTimeAsync(WINDOW_MS)
      expect(calls.list).toBe(before + 2)
    })

    it('ends after the last step with the row still listed', async () => {
      vi.useFakeTimers()
      const calls = useTimedOutReject(() => [ALPHA])
      const invites = store()
      await invites.refresh()
      await invites.reject(ALPHA)
      const before = calls.list

      await vi.advanceTimersByTimeAsync(WINDOW_MS)

      expect(calls.list).toBe(
        before + PENDING_MEMBERSHIP_REFRESH_DELAYS_MS.length,
      )
      expect(invites.rejectPending(ALPHA)).toBe(false)
    })

    it('counts a 504 in rejectAll as pending and carries on', async () => {
      const BETA = invite('!b:hs', 'Beta')
      const left: string[] = []
      server.use(
        http.get(`${BASE}/v1/invites`, () =>
          HttpResponse.json({
            data: [ALPHA, BETA].filter((row) => !left.includes(row.room_id)),
          }),
        ),
        http.post(
          `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
          ({ params }) => {
            if (params.roomId === '!a:hs') {
              return HttpResponse.json(
                {
                  error: {
                    code: 'gateway_timeout',
                    message: 'leave timed out after 30s',
                  },
                },
                { status: 504 },
              )
            }
            left.push(String(params.roomId))
            return HttpResponse.json({ data: {} })
          },
        ),
      )
      const invites = store()
      await invites.refresh()

      const result = await invites.rejectAll()

      expect(result).toEqual({ succeeded: 1, pending: 1, failed: [] })
      expect(left).toEqual(['!b:hs'])
      expect(invites.rejectPending(ALPHA)).toBe(true)
    })

    it('keeps a row removed while a stepped refresh was in flight', async () => {
      vi.useFakeTimers()
      let release: () => void = () => {}
      let hold = false
      const calls = { list: 0 }
      server.use(
        http.get(`${BASE}/v1/invites`, async () => {
          calls.list += 1
          if (hold) {
            await new Promise<void>((resolve) => {
              release = resolve
            })
          }
          // Issued before the homeserver finished, so it still lists it.
          return HttpResponse.json({ data: [ALPHA] })
        }),
        http.post(`${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`, () =>
          HttpResponse.json(
            {
              error: {
                code: 'gateway_timeout',
                message: 'leave timed out after 30s',
              },
            },
            { status: 504 },
          ),
        ),
      )
      const invites = store()
      await invites.refresh()
      await invites.reject(ALPHA)
      hold = true
      const before = calls.list

      await vi.advanceTimersByTimeAsync(PENDING_MEMBERSHIP_REFRESH_DELAYS_MS[0])
      expect(calls.list).toBe(before + 1)
      // The homeserver finishes while that GET is out.
      invites.noteRemoved(ACCOUNT, '!a:hs')
      release()
      await vi.advanceTimersByTimeAsync(0)

      expect(invites.invites.value).toEqual([])
      expect(invites.rejectPending(ALPHA)).toBe(false)

      // A GET issued after the removal still decides: one that lists the
      // invite puts it back.
      hold = false
      await invites.refresh()
      expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
    })

    it('holds an empty-bodied 504 as pending too', async () => {
      server.use(
        http.get(`${BASE}/v1/invites`, () =>
          HttpResponse.json({ data: [ALPHA] }),
        ),
        // What a proxy sends: no error envelope, so openapi-fetch reports
        // `error: undefined`.
        http.post(
          `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
          () =>
            new HttpResponse(null, {
              status: 504,
              headers: { 'Content-Length': '0' },
            }),
        ),
      )
      const invites = store()
      await invites.refresh()

      expect(await invites.reject(ALPHA)).toEqual(PENDING)
      expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
      expect(invites.rejectPending(ALPHA)).toBe(true)
      invites.resetSession()
    })

    it('keeps an empty-bodied 502 a failure, not a success', async () => {
      server.use(
        http.get(`${BASE}/v1/invites`, () =>
          HttpResponse.json({ data: [ALPHA] }),
        ),
        http.post(
          `${BASE}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
          () =>
            new HttpResponse(null, {
              status: 502,
              headers: { 'Content-Length': '0' },
            }),
        ),
      )
      const invites = store()
      await invites.refresh()

      expect(await invites.reject(ALPHA)).toEqual({
        ok: false,
        message: 'unexpected server response',
      })
      // Not dropped, and not suppressed: the next GET still shows it.
      await invites.refresh()
      expect(invites.invites.value.map((row) => row.room_id)).toEqual(['!a:hs'])
      expect(invites.rejectPending(ALPHA)).toBe(false)
    })

    it('drops a pending reject on a session reset', async () => {
      vi.useFakeTimers()
      const calls = useTimedOutReject(() => [ALPHA])
      const invites = store()
      await invites.refresh()
      await invites.reject(ALPHA)
      const before = calls.list

      invites.resetSession()
      await vi.advanceTimersByTimeAsync(WINDOW_MS)

      expect(invites.rejectPending(ALPHA)).toBe(false)
      expect(calls.list).toBe(before)
    })
  })
})
