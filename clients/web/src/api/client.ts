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
 * The deadline on every typed `/v1` call.
 *
 * A `fetch` has no timeout of its own: it settles when the transport settles,
 * and a TCP connection whose path has gone away does not settle. On iOS a
 * WiFi→cell handover leaves exactly that — the old connection is bound to an
 * interface with no route, no FIN or RST is ever exchanged, and a request
 * issued across the switch hangs until the OS gives up, which can be minutes
 * or never.
 *
 * That is the difference between the two failure shapes callers already
 * handle. A *rejected* request is fine everywhere: `rooms.ts` clears `loading`
 * in a `finally` and `timeline.ts` catches into `error`, so the UI shows a
 * failure and a retry. A request that never settles clears nothing, and the
 * room list sits on "Loading rooms…" or a room on "Loading messages…" until
 * the app is relaunched — the iPhone report this deadline exists to end.
 *
 * Deliberately *longer* than the 15 s the QR flows already impose on their own
 * calls (`stores/matrix-oauth-qr.ts`), so a call site that brought its own
 * deadline always wins the race and keeps its own failure classification. This
 * is the floor under everything that brought none, not a latency budget.
 *
 * **Not** `platform/tauri.ts`'s `REQUEST_TIMEOUT_MS`, which is a different
 * bound for a different reason and deliberately much longer (120 s). That one
 * is a resource backstop on every shell `fetch`, media transfers included, so
 * a blackholed server cannot retain a download permit for the session; it has
 * to be generous enough for a large attachment on a slow link. This one is a
 * UX deadline on the JSON API alone — nothing it covers is a transfer, so
 * nothing it covers has any business taking 20 s. On a packaged build both
 * apply and the shorter one decides, which is the intended order. Media is
 * untouched by this constant either way: `media/media-service.ts` fetches
 * bytes directly rather than through this client.
 */
export const API_REQUEST_TIMEOUT_MS = 20_000

/**
 * The same request with `timeoutMs` added to whatever deadline it already had.
 *
 * `Request.signal` is read-only, so the signal has to be attached by rebuilding
 * the request — the body and headers carry over from `request`. The caller's
 * own signal is *combined* rather than replaced: the QR stores pass one per
 * call (`signal:` in their `api.GET`/`api.POST` options) and aborting on their
 * terms is how they tell their own timeout apart from a transport failure.
 *
 * `platform/tauri.ts`'s `boundedSignal` composes the same two bounds, but it
 * cannot be shared: it attaches through `init.signal`, because the http
 * plugin never consults `input.signal`, and importing it here would pull the
 * Tauri plugin into the browser bundle. This runs a layer above the transport,
 * where the only handle on the request is the `Request` itself.
 */
function withDeadline(request: Request, timeoutMs: number): Request {
  if (timeoutMs <= 0) {
    return request
  }
  return new Request(request, {
    signal: AbortSignal.any([request.signal, AbortSignal.timeout(timeoutMs)]),
  })
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
 *
 * `timeoutMs` is the deadline every request gets (see
 * [`API_REQUEST_TIMEOUT_MS`]); tests shorten it, and `0` disables it.
 */
export function createApiClient(
  auth: AuthProvider,
  baseUrl = '/',
  platform: Pick<Platform, 'fetch'> = browserPlatform(),
  timeoutMs = API_REQUEST_TIMEOUT_MS,
): ApiClient {
  const client = createClient<paths>({ baseUrl, fetch: platform.fetch })

  const bearer: Middleware = {
    async onRequest({ request }) {
      const token = await auth.getToken()
      if (token !== null) {
        request.headers.set('authorization', `Bearer ${token}`)
      }
      // Last, so the rebuilt request carries the header just set on it.
      return withDeadline(request, timeoutMs)
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
