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
function withDeadline(request: Request, deadline: AbortSignal | null): Request {
  if (deadline === null) {
    return request
  }
  return new Request(request, {
    signal: AbortSignal.any([request.signal, deadline]),
  })
}

/**
 * `work`, but rejecting as soon as `deadline` fires.
 *
 * The deadline has to cover **`auth.getToken()` as well as the request**, and
 * that is not a refinement — it is the difference between the bound working
 * and not existing at all. `getToken()` may itself go to the network: the
 * OAuth provider refreshes an access token near expiry by POSTing the token
 * endpoint (`auth/oauth.tsx`). Attaching a signal to the outgoing request
 * *after* awaiting the token leaves that await unbounded, so on a dead path
 * the middleware never returns, `fetch` is never called, no timer is ever
 * created, and the request hangs forever with a deadline that was never
 * reached. Starting the clock first and racing the token against it closes
 * that.
 *
 * The listener is removed when `work` settles rather than left to `once`. A
 * signal that aborts after the race was won would otherwise reject a promise
 * nobody is holding — an unhandled rejection, which is the WCR-02 tripwire and
 * a failing vitest gate, not a stray warning.
 */
function withinDeadline<T>(
  work: Promise<T>,
  deadline: AbortSignal,
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    if (deadline.aborted) {
      reject(deadline.reason)
      return
    }
    const onAbort = () => reject(deadline.reason)
    deadline.addEventListener('abort', onAbort)
    work.then(
      (value) => {
        deadline.removeEventListener('abort', onAbort)
        resolve(value)
      },
      (error: unknown) => {
        deadline.removeEventListener('abort', onAbort)
        reject(error instanceof Error ? error : new Error(String(error)))
      },
    )
  })
}

/**
 * What the reader sees when a request never got an answer.
 *
 * "Fetch is aborted" is WebKit's words for our own deadline firing, and it
 * tells a reader nothing they can act on — it names a mechanism, blames
 * something that sounds like a bug, and does not mention the one thing that
 * would actually help. These say what happened and what to try, in the
 * vocabulary `matrix-oauth-qr.ts` already uses for the same situation.
 */
export const REQUEST_TIMEOUT_MESSAGE =
  'Axon did not respond in time. Check the connection and try again.'
export const REQUEST_UNREACHABLE_MESSAGE =
  'Could not reach Axon. Check the connection and try again.'

/**
 * A request that never produced a response, reworded for a reader.
 *
 * Both abort names are the deadline. Per spec `AbortSignal.timeout` aborts
 * with a `TimeoutError`, and `AbortSignal.any` forwards that reason — but
 * WebKit rejects the fetch with its own generic `AbortError` ("Fetch is
 * aborted") instead, which is what an iPhone actually reports. Keying on
 * either is what makes this work on the engine the reports come from.
 *
 * Treating an abort as a deadline is sound here because this client aborts
 * for exactly one reason. The QR stores pass a signal of their own, but they
 * decide what happened from `controller.signal.aborted` rather than from the
 * message, so their classification is unaffected by this rewording.
 *
 * A `TypeError` is how `fetch` reports a transport failure — DNS, refused,
 * connection cut mid-flight. Anything else keeps its own message: a real bug
 * should not be dressed up as a network blip.
 */
export function requestFailureMessage(cause: unknown): string {
  switch (causeField(cause, 'name')) {
    case 'TimeoutError':
    case 'AbortError':
      return REQUEST_TIMEOUT_MESSAGE
    case 'TypeError':
      return REQUEST_UNREACHABLE_MESSAGE
    default:
      return causeField(cause, 'message') ?? REQUEST_UNREACHABLE_MESSAGE
  }
}

/**
 * Read a string field off a thrown value, whatever it is.
 *
 * Structural rather than `instanceof Error`, because **an abort is not
 * necessarily an `Error`**. A fetch aborts with a `DOMException`, and while
 * `DOMException` inherits from `Error` in a browser, it does not under jsdom —
 * `new DOMException(…) instanceof Error` is `false` there. An `instanceof`
 * guard therefore passes its unit tests on the exact input it is meant to
 * classify and then takes the wrong branch, or the reverse. Reading the field
 * is true in both.
 */
function causeField(cause: unknown, field: 'name' | 'message'): string | null {
  if (typeof cause !== 'object' || cause === null || !(field in cause)) {
    return null
  }
  const value = (cause as Record<string, unknown>)[field]
  return typeof value === 'string' && value !== '' ? value : null
}

/**
 * The rejection a caller sees for a request that never got a response.
 *
 * `name` and `cause` are carried over from the original so anything that
 * classifies by them still can — only the human-facing `message` changes.
 */
export class RequestFailedError extends Error {
  constructor(cause: unknown) {
    super(requestFailureMessage(cause), { cause })
    this.name = causeField(cause, 'name') ?? 'RequestFailedError'
  }
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
      // Created before the token is asked for, because acquiring one can be a
      // network round trip of its own — see `withinDeadline`.
      const deadline = timeoutMs > 0 ? AbortSignal.timeout(timeoutMs) : null
      // Only a provider that *went* somewhere can fail to come back, and the
      // synchronous ones are the common case (a pasted token is a string).
      // Racing those would add microtask hops to every request in the client
      // for a hang that cannot happen — and the ordering shift is observable,
      // not merely wasteful: it moves when the request leaves relative to the
      // render that a caller may already have painted from the optimistic echo
      // beside it.
      const pending = auth.getToken()
      const token =
        deadline === null || !(pending instanceof Promise)
          ? await pending
          : await withinDeadline(pending, deadline)
      if (token !== null) {
        request.headers.set('authorization', `Bearer ${token}`)
      }
      // Last, so the rebuilt request carries the header just set on it.
      return withDeadline(request, deadline)
    },
    onResponse({ response }) {
      if (response.status === 401) {
        auth.onAuthFailure()
      }
      return response
    },
    // One place to reword every request that never got an answer. Doing it
    // here rather than at each `catch` is what makes the wording uniform:
    // roughly twenty stores stringify a caught error straight into a signal
    // the UI renders, and each one would otherwise have to remember.
    onError({ error }) {
      return error instanceof RequestFailedError
        ? error
        : new RequestFailedError(error)
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
