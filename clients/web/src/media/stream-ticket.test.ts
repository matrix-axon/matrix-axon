import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import type { AuthProvider } from '../auth/provider'
import { createStreamTickets } from './stream-ticket'

const BASE = 'http://axon.test'
const ACCOUNT = '11111111-1111-4111-8111-111111111111'
const MINT = `${BASE}/v1/media/:account/:server/:media/stream-ticket`

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const auth: AuthProvider = {
  getToken: () => Promise.resolve('tok'),
} as unknown as AuthProvider

function tickets(mediaStreamSrc: ((url: string) => string) | null) {
  return createStreamTickets({
    api: createApiClient(auth, BASE),
    baseUrl: BASE,
    platform: { mediaStreamSrc },
  })
}

function issuePath(path: string) {
  server.use(
    http.post(MINT, () =>
      HttpResponse.json({
        data: { path, expires_at: '2099-01-01T00:00:00Z' },
      }),
    ),
  )
}

describe('createStreamTickets', () => {
  it('resolves the ticket path against the server base, through the platform', async () => {
    issuePath('/v1/media-stream/abc')
    const outcome = await tickets((url) => `mapped:${url}`).issue(
      ACCOUNT,
      'mxc://hs/clip',
    )
    expect(outcome).toEqual({
      ok: true,
      src: `mapped:${BASE}/v1/media-stream/abc`,
    })
  })

  it('asks for nothing where the platform cannot load a ticket URL', async () => {
    // No mint handler: a request would fail the test.
    const streams = tickets(null)
    expect(streams.available).toBe(false)
    expect(await streams.issue(ACCOUNT, 'mxc://hs/clip')).toEqual({
      ok: false,
      kind: 'refused',
    })
  })

  it('refuses to load a path outside the stream prefix', async () => {
    for (const path of [
      '/v1/media/x',
      'https://elsewhere.test/v1/media-stream/abc',
      '/v1/media-stream/../accounts',
    ]) {
      issuePath(path)
      expect(
        await tickets((url) => url).issue(ACCOUNT, 'mxc://hs/clip'),
      ).toEqual({ ok: false, kind: 'failed' })
    }
  })

  it('reads a 400 as a refusal and anything else as a failure', async () => {
    const streams = tickets((url) => url)
    for (const [status, kind] of [
      [400, 'refused'],
      [404, 'failed'],
      [503, 'failed'],
    ] as const) {
      server.use(
        http.post(MINT, () =>
          HttpResponse.json({ error: { code: 'x', message: 'x' } }, { status }),
        ),
      )
      expect(await streams.issue(ACCOUNT, 'mxc://hs/clip')).toEqual({
        ok: false,
        kind,
      })
    }
  })

  it('reads an unreachable server as a failure', async () => {
    server.use(http.post(MINT, () => HttpResponse.error()))
    expect(await tickets((url) => url).issue(ACCOUNT, 'mxc://hs/clip')).toEqual(
      { ok: false, kind: 'failed' },
    )
  })
})
