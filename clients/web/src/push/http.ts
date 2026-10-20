import { apiUrl } from '../server-url'
import type { PutPushBody } from './registration'

/**
 * A failed registration call. The message is fixed so a device token or an
 * access token in the response body cannot land in a log.
 */
export class PushRequestError extends Error {
  readonly code: string | null

  constructor(code: string | null) {
    super('the push registration was not accepted')
    this.name = 'PushRequestError'
    this.code = code
  }
}

export async function putPushRegistration(
  fetch: typeof globalThis.fetch,
  baseUrl: string,
  accessToken: string,
  body: PutPushBody,
): Promise<{ id: string }> {
  const response = await fetch(apiUrl('/v1/push/registrations', baseUrl), {
    method: 'PUT',
    headers: {
      Authorization: `Bearer ${accessToken}`,
      Accept: 'application/json',
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(body),
  })
  if (!response.ok) {
    throw await pushError(response)
  }
  const id = registrationId(await response.json().catch(() => null))
  if (id === null) {
    throw new PushRequestError(null)
  }
  return { id }
}

export async function deletePushRegistration(
  fetch: typeof globalThis.fetch,
  baseUrl: string,
  accessToken: string,
  id: string,
): Promise<void> {
  const response = await fetch(
    apiUrl(`/v1/push/registrations/${encodeURIComponent(id)}`, baseUrl),
    {
      method: 'DELETE',
      headers: {
        Authorization: `Bearer ${accessToken}`,
        Accept: 'application/json',
      },
    },
  )
  if (response.status >= 200 && response.status < 300) {
    return
  }
  throw await pushError(response)
}

async function pushError(response: Response): Promise<PushRequestError> {
  const payload = await response.json().catch(() => null)
  return new PushRequestError(errorCode(payload))
}

function registrationId(payload: unknown): string | null {
  if (typeof payload !== 'object' || payload === null) {
    return null
  }
  const data = (payload as { data?: { id?: unknown } }).data
  return typeof data?.id === 'string' && data.id !== '' ? data.id : null
}

function errorCode(payload: unknown): string | null {
  if (typeof payload !== 'object' || payload === null) {
    return null
  }
  const code = (payload as { error?: { code?: unknown } }).error?.code
  return typeof code === 'string' ? code : null
}
