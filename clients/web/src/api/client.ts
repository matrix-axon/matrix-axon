import createClient, { type Client, type Middleware } from 'openapi-fetch'
import type { AuthProvider } from '../auth/provider'
import { browserPlatform, type Platform } from '../platform'
import type { paths } from './schema'

/**
 * The typed API client over the generated schema (`pnpm gen:api`). Every
 * success body is the `{ data: … }` envelope and every error body the
 * `{ error: { code, message } }` envelope, exactly as the spec declares them —
 * no unwrapping magic here, so what callers see matches the wire.
 */
export type ApiClient = Client<paths>

/** The error envelope every non-2xx `/v1` response carries. */
export interface ErrorEnvelope {
  error: { code: string; message: string }
}

/**
 * Build the API client over the auth seam (ADR 0046, M-W2).
 *
 * Two middleware concerns, matching the server's bearer scheme (ADR 0029):
 * attach `Authorization: Bearer <token>` when the provider has a token, and
 * report any 401 back to the provider — the response still flows to the
 * caller, so call sites see the error envelope while the provider handles the
 * session consequence (e.g. dropping a revoked token).
 *
 * `baseUrl` defaults to same-origin (`/`), which is the Vite dev-proxy setup;
 * a separately-hosted browser deployment passes the server origin and relies
 * on the server's CORS allow-list (M-W1.5, still unbuilt).
 *
 * `platform` is the transport seam (ADR 0102 § 2). A packaged build passes one
 * whose `fetch` runs in the shell process, which is why it is never a CORS
 * client; the browser gets its own `fetch` and behaves exactly as before.
 */
export function createApiClient(
  auth: AuthProvider,
  baseUrl = '/',
  platform: Pick<Platform, 'fetch'> = browserPlatform(),
): ApiClient {
  const client = createClient<paths>({ baseUrl, fetch: platform.fetch })

  const bearer: Middleware = {
    async onRequest({ request }) {
      const token = await auth.getToken()
      if (token !== null) {
        request.headers.set('authorization', `Bearer ${token}`)
      }
      return request
    },
    onResponse({ response }) {
      if (response.status === 401) {
        auth.onAuthFailure()
      }
      return response
    },
  }
  client.use(bearer)
  return client
}

/**
 * The rejection policy for fire-and-forget API work (AGENTS.md guardrail;
 * WCR-02). openapi-fetch returns HTTP errors in the `{ error }` envelope, but
 * a *network-level* failure — server unreachable, connection dropped
 * mid-flight — **rejects**. A background task nobody awaits must catch that
 * rejection or it surfaces as an unhandled promise rejection (and fails the
 * vitest gate). Failures are swallowed by design: every call site has a
 * rendered fallback (a stub row, a stale cache, a retry on the next trigger),
 * so the only wrong outcome is the uncaught rejection itself. Work whose
 * failure needs UI (an error signal, a re-queue) handles rejection itself
 * instead of using this.
 */
export function inBackground(task: Promise<unknown>): void {
  void task.catch(() => {})
}

/** Whether an openapi-fetch `error` value is the server's error envelope. */
export function isErrorEnvelope(error: unknown): error is ErrorEnvelope {
  if (typeof error !== 'object' || error === null || !('error' in error)) {
    return false
  }
  const body = (error as { error: unknown }).error
  return (
    typeof body === 'object' &&
    body !== null &&
    typeof (body as { code?: unknown }).code === 'string' &&
    typeof (body as { message?: unknown }).message === 'string'
  )
}

/** The envelope's machine-readable code, or `null` for non-envelope errors. */
export function apiErrorCode(error: unknown): string | null {
  return isErrorEnvelope(error) ? error.error.code : null
}

/**
 * A human-readable message for any openapi-fetch `error` value — the
 * envelope's message when the server sent one, a generic fallback when the
 * body was something else (a proxy error page, an empty body, …).
 */
export function apiErrorMessage(error: unknown): string {
  return isErrorEnvelope(error)
    ? error.error.message
    : 'unexpected server response'
}
