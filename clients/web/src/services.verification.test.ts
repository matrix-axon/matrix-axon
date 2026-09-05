import { computed, signal } from '@preact/signals'
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
import { createApiClient } from './api/client'
import { VERIFICATION_DONE, VERIFICATION_REQUESTED } from './api/frames'
import { connectLiveVerification } from './services'
import type { AccountsStore } from './stores/accounts'
import { createLiveConnection } from './stores/live-connection'
import { createVerificationStore } from './stores/verification'
import { FakeWebSocket } from './test/fake-socket'

const ACCT = '11111111-1111-1111-1111-111111111111'
const ME = '@me:hs'
const BASE = 'http://axon.test'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function harness() {
  let socket: FakeWebSocket | undefined
  const live = createLiveConnection({
    socketFactory: () => {
      socket = new FakeWebSocket()
      return socket.asWebSocket()
    },
  })
  const api = createApiClient(
    {
      getToken: () => 't',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE,
  )
  const verification = createVerificationStore(api)
  const known = signal([{ account_id: ACCT, user_id: ME, state: 'active' }])
  let refreshes = 0
  const accounts = {
    accounts: computed(() => known.value),
    refresh: () => {
      refreshes += 1
      return Promise.resolve()
    },
  } as unknown as AccountsStore
  connectLiveVerification(live, verification, accounts)
  live.start()
  socket!.emitOpen()
  return {
    verification,
    socket: () => socket!,
    refreshes: () => refreshes,
    setAccounts: (next: typeof known.value) => {
      known.value = next
    },
    live,
  }
}

describe('connectLiveVerification', () => {
  it('routes a verification.requested frame into the store', () => {
    const { verification, socket } = harness()
    socket().emitMessage(
      JSON.stringify({
        type: VERIFICATION_REQUESTED,
        account_id: ACCT,
        payload: {
          flow_id: '$flow',
          user_id: ME,
          device_id: 'ELEMENT',
        },
      }),
    )
    expect(verification.inboxCount.value).toBe(1)
    expect(verification.flows.value[0].direction).toBe('incoming')
  })

  it('refreshes accounts on verification.done', () => {
    const { socket, refreshes, verification } = harness()
    socket().emitMessage(
      JSON.stringify({
        type: VERIFICATION_REQUESTED,
        account_id: ACCT,
        payload: { flow_id: '$flow', user_id: ME, device_id: 'ELEMENT' },
      }),
    )
    socket().emitMessage(
      JSON.stringify({
        type: VERIFICATION_DONE,
        account_id: ACCT,
        payload: { flow_id: '$flow', user_id: ME, device_id: 'ELEMENT' },
      }),
    )
    expect(verification.inbox.value[0].stage).toBe('done')
    expect(refreshes()).toBe(1)
  })

  it('an accounts change after a reconnect does not re-GET flows', async () => {
    vi.useFakeTimers()
    let gets = 0
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCT}/verify`, () => {
        gets += 1
        return HttpResponse.json({ data: [] })
      }),
    )
    const { live, socket, setAccounts } = harness()
    socket().emitClose()
    await vi.advanceTimersByTimeAsync(1000)
    socket().emitOpen()
    await vi.advanceTimersByTimeAsync(0)
    expect(live.reconnects.value).toBe(1)
    expect(gets).toBe(1)
    // A `verification.done` frame refreshes accounts; if the reconnect effect
    // had subscribed to that store it would loop straight back into a GET.
    setAccounts([{ account_id: ACCT, user_id: ME, state: 'active' }])
    await vi.advanceTimersByTimeAsync(0)
    expect(gets).toBe(1)
    vi.useRealTimers()
  })

  it('re-GETs flows on reconnect (skip 0)', async () => {
    vi.useFakeTimers()
    let gets = 0
    server.use(
      http.get(`${BASE}/v1/accounts/${ACCT}/verify`, () => {
        gets += 1
        return HttpResponse.json({
          data: [
            {
              flow_id: '$re',
              user_id: ME,
              device_id: 'ELEMENT',
              stage: 'requested',
              emoji: null,
              decimals: null,
              cancel_reason: null,
            },
          ],
        })
      }),
    )
    const { verification, live, socket } = harness()
    expect(gets).toBe(0)
    expect(live.reconnects.value).toBe(0)
    socket().emitClose()
    await vi.advanceTimersByTimeAsync(1000)
    socket().emitOpen()
    await vi.advanceTimersByTimeAsync(0)
    expect(live.reconnects.value).toBe(1)
    expect(gets).toBe(1)
    expect(verification.flows.value.some((flow) => flow.flowId === '$re')).toBe(
      true,
    )
    vi.useRealTimers()
  })
})
