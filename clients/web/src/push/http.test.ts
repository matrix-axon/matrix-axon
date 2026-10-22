import { describe, expect, it } from 'vitest'
import { deletePushRegistration, putPushRegistration } from './http'
import { isPushSessionRefreshRequired } from './registration'

function jsonResponse(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })
}

async function seenRequest(input: RequestInfo | URL): Promise<{
  url: string
  method: string
  authorization: string | null
  body: unknown
}> {
  const request = input instanceof Request ? input : new Request(input)
  const body =
    request.method === 'GET' || request.method === 'DELETE'
      ? undefined
      : await request.clone().json()
  return {
    url: request.url,
    method: request.method,
    authorization: request.headers.get('authorization'),
    body,
  }
}

describe('push registration HTTP', () => {
  it('PUTs the body with the explicit bearer and reads the id', async () => {
    const calls: Array<Awaited<ReturnType<typeof seenRequest>>> = []
    const fetchImpl: typeof fetch = async (input) => {
      calls.push(await seenRequest(input))
      return jsonResponse(200, {
        data: {
          id: '6b53f7f0-0000-4000-8000-0000000000aa',
          provider: 'fcm',
          disclosure: 'activity',
        },
      })
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
    expect(seen.method).toBe('PUT')
    expect(seen.authorization).toBe('Bearer access-token')
    expect(seen.body).toEqual({
      provider: 'fcm',
      token: 'fcm-token',
      disclosure: 'activity',
    })
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
      const request = input instanceof Request ? input : new Request(input)
      seen = request.url
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
