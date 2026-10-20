import type { AuthProvider } from '../auth/provider'
import { apiErrorCode, createApiClient } from '../api/client'
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

/**
 * The registration calls carry an explicit bearer: sign-out still holds the
 * session that owns the row, which is not always `auth.getToken()` after a
 * refresh. The client is the shared one, so the JSON API deadline applies.
 */
function clientFor(
  fetch: typeof globalThis.fetch,
  baseUrl: string,
  accessToken: string,
) {
  const auth: AuthProvider = {
    getToken: () => accessToken,
    onAuthFailure() {},
    LoginBootstrap: () => null,
  }
  return createApiClient(auth, baseUrl, { fetch })
}

export async function putPushRegistration(
  fetch: typeof globalThis.fetch,
  baseUrl: string,
  accessToken: string,
  body: PutPushBody,
): Promise<{ id: string }> {
  const { data, error } = await clientFor(fetch, baseUrl, accessToken).PUT(
    '/v1/push/registrations',
    { body },
  )
  if (error !== undefined) {
    throw new PushRequestError(apiErrorCode(error))
  }
  const id = data?.data.id
  if (typeof id !== 'string' || id === '') {
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
  const { error, response } = await clientFor(
    fetch,
    baseUrl,
    accessToken,
  ).DELETE('/v1/push/registrations/{id}', { params: { path: { id } } })
  if (error !== undefined || !response.ok) {
    throw new PushRequestError(apiErrorCode(error))
  }
}
