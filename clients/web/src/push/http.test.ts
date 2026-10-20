import { describe, expect, it } from 'vitest'
import { deletePushRegistration, putPushRegistration } from './http'
import { isPushSessionRefreshRequired } from './registration'

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })
}

describe('push registration HTTP', () => {
  it('PUTs the body with the explicit bearer and reads the id', async () => {
    const calls: Array<{ url: string; init: RequestInit }> = []
    const fetchImpl: typeof fetch = (input, init) => {
      calls.push({ url: String(input), init: init ?? {} })
      return Promise.resolve(
        jsonResponse(200, {
          data: {
            id: '6b53f7f0-0000-4000-8000-0000000000aa',
            provider: 'fcm',
            disclosure: 'activity',
          },
        }),
      )
    }
    const result = await putPushRegistration(
      fetchImpl,
      'http://axon.test/axon/',
      'access-token',
      { provider: 'fcm', token: 'fcm-token', disclosure: 'activity' },
    )
    expect(result.id).toBe('6b53f7f0-0000-4000-8000-0000000000aa')
    const seen = calls[0]
    if (seen === undefined) {
      throw new Error('the registration request was not sent')
    }
    expect(seen.url).toBe('http://axon.test/axon/v1/push/registrations')
    expect(seen.init.method).toBe('PUT')
    const headers = seen.init.headers as Record<string, string>
    expect(headers.Authorization).toBe('Bearer access-token')
    const body = JSON.parse(String(seen.init.body)) as Record<string, unknown>
    expect(body.provider).toBe('fcm')
    expect('environment' in body).toBe(false)
  })

  it('surfaces push_session_refresh_required without the response text', async () => {
    const fetchImpl: typeof fetch = () =>
      Promise.resolve(
        jsonResponse(400, {
          error: {
            code: 'push_session_refresh_required',
            message: 'secret-access-token',
          },
        }),
      )
    const error = await putPushRegistration(
      fetchImpl,
      'http://axon.test',
      'access-token',
      {
        provider: 'apns',
        token: 'device',
        disclosure: 'activity',
        environment: 'production',
      },
    ).catch((caught: unknown) => caught)
    expect(isPushSessionRefreshRequired(error)).toBe(true)
    expect(error).toBeInstanceOf(Error)
    expect((error as Error).message).not.toContain('secret-access-token')
  })

  it('treats any 2xx DELETE as success and encodes the id', async () => {
    let seen = ''
    const fetchImpl: typeof fetch = (input) => {
      seen = String(input)
      return Promise.resolve(new Response(null, { status: 204 }))
    }
    await deletePushRegistration(
      fetchImpl,
      'http://axon.test',
      'access-token',
      'id/with space',
    )
    expect(seen).toBe(
      'http://axon.test/v1/push/registrations/id%2Fwith%20space',
    )
  })
})
