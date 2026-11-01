import { invoke } from '@tauri-apps/api/core'
import {
  asTransportFailure,
  recordTransportVersion,
  type TransportFailure,
} from './transport-report'

/**
 * The page's side of the shell's HTTP transport (ADR 0127), the in-repo
 * plugin at `src-tauri/axon-http/`.
 *
 * `tauri-plugin-http` built a new `reqwest` client for every request, so each
 * one paid its own DNS lookup and TCP and TLS handshakes. On networks that cap
 * new connections per device the app stalled for 35 s at a time. The plugin
 * behind this binding holds one pooled client: HTTP/2 to a TLS server, a
 * keep-alive pool for a plain-`http` LAN one.
 *
 * `shellFetch` keeps `fetch`'s contract where callers rely on it:
 *
 * - It resolves with a real `Response` whose headers were set at
 *   construction, so `blob()` carries the `Content-Type`.
 * - A failure rejects with a `TypeError`, whose `cause` is the transport's
 *   `{ kind, detail }`. That is a closed set with no free text, so nothing that
 *   could name a host reaches a trace.
 * - An abort rejects with the signal's own reason, an `AbortError` or a
 *   `TimeoutError`, whatever the Rust side reported and in whichever order the
 *   two arrive. So an abort is never reported as a network failure.
 *
 * It does not keep all of it. The plugin always follows redirects, so a
 * `redirect` mode other than `follow` is refused rather than ignored, and there
 * are no cookies.
 */

/** The invoke header that carries the request line (`REQUEST_HEADER` in Rust). */
const REQUEST_HEADER = 'axon-http-request'

/** What `send` resolves with once headers arrive (`Head` in Rust). */
interface Head {
  status: number
  headers: [string, string][]
  url: string
  version: string
  /**
   * `false` when the response is known to be empty and so was never stored:
   * the body is `null`, and there is nothing to read or release.
   */
  body: boolean
}

/** The counters `transport:stats` logs (`Stats` in Rust). */
export interface TransportStats {
  connectionsOpened: number
  requestsSent: number
  retries: { nack: number; fastReset: number; requeue: number }
  resets: number
  liveEntries: number
}

/**
 * Request ids, unique for the life of this page. The plugin keys entries by
 * webview and id and forgets a webview's entries when a page starts loading,
 * so a fresh page may start again from 1. Never reused within a page, so a
 * late `drop_body` or `cancel` can only reach the request it was meant for.
 */
let nextId = 1

/**
 * Release a body nobody read once its stream is collected (ADR 0127 § 2).
 *
 * What is registered is neither the `Response` nor its stream but a plain
 * object only the stream's own `pull` and `cancel` hold, so it lives exactly as
 * long as the stream can still be read. A `Response` and a `ReadableStream` are
 * the engine's objects, and Chromium (WebView2, Android's WebView) drops and
 * recreates the script wrapper of one nothing in script refers to, even while a
 * reader is pulling from it. Registered on the stream, this finalizer fired for
 * 49 of 50 bodies still being read, and the read then failed as `cancelled`.
 *
 * Absent in a webview without the API, where the entry instead waits for the
 * request's signal or the page to go.
 */
const unreadBodies =
  typeof FinalizationRegistry === 'function'
    ? new FinalizationRegistry<number>((id) => {
        void invoke('plugin:axon-http|drop_body', { id }).catch(() => {})
      })
    : null

/** `fetch`, through the shell's transport. `signal` is the one honoured. */
export async function shellFetch(
  input: RequestInfo | URL,
  init: RequestInit | undefined,
  signal: AbortSignal,
): Promise<Response> {
  if (signal.aborted) {
    throw signal.reason
  }
  // Built only to read what the caller asked for in one shape: the URL
  // resolved, the method normalised, forbidden headers already dropped, and
  // the body as bytes whatever form it was given in.
  const request = new Request(input, init)
  if (request.redirect !== 'follow') {
    throw new TypeError(
      `the shell's transport always follows redirects (redirect: "${request.redirect}")`,
    )
  }
  // One copy of the body, sent as the raw invoke payload rather than a JSON
  // array of numbers (ADR 0127 § 3). Empty is no body.
  const body = new Uint8Array(await request.arrayBuffer())
  if (signal.aborted) {
    throw signal.reason
  }
  const id = nextId++
  const line = encodeURIComponent(
    JSON.stringify({
      id,
      method: request.method,
      url: request.url,
      headers: Array.from(request.headers.entries()),
    }),
  )
  // Registered before `send`, because a cancel may reach the plugin before
  // `send` does and is remembered there. It stays registered through the body.
  const cancel = () => {
    void invoke('plugin:axon-http|cancel', { id }).catch(() => {})
  }
  signal.addEventListener('abort', cancel, { once: true })

  let head: Head
  try {
    head = await untilAborted(
      invoke<Head>('plugin:axon-http|send', body, {
        headers: { [REQUEST_HEADER]: line },
      }),
      signal,
    )
  } catch (error) {
    if (signal.aborted) {
      throw signal.reason
    }
    signal.removeEventListener('abort', cancel)
    throw failure(error)
  }
  // An abort in the microtasks between `send` answering and this line has
  // already run `cancel`, which removed the entry, and the stream's listener
  // below would never hear it. Everything from here to the return is
  // synchronous, so this is the last gap.
  if (signal.aborted) {
    throw signal.reason
  }

  const stream = head.body ? bodyStream(id, signal, cancel) : null
  if (stream === null) {
    signal.removeEventListener('abort', cancel)
  }
  let response: Response
  try {
    response = new Response(stream, {
      status: head.status,
      headers: head.headers,
    })
  } catch {
    // A status `Response` cannot represent (outside 200–599), or a header
    // value it will not take. Neither is a `/v1` answer.
    void stream?.cancel().catch(() => {})
    throw failure({ kind: 'other', detail: null })
  }
  return finished(response, head)
}

/**
 * `response` with what its constructor cannot set: the final `url` after
 * redirects, which `clone()` would otherwise drop again, and the version.
 */
function finished(response: Response, head: Head): Response {
  Object.defineProperty(response, 'url', { value: head.url })
  recordTransportVersion(response, head.version)
  const clone = response.clone.bind(response)
  Object.defineProperty(response, 'clone', {
    value: () => finished(clone(), head),
  })
  return response
}

/**
 * The body as a pull stream, so `ReadableStream` backpressure reaches the
 * socket: each `pull` asks the plugin for one chunk.
 *
 * Each frame is a chunk with a trailing `0`, or the single byte `1` at the end
 * of the body, so a chunk may be empty.
 */
function bodyStream(
  id: number,
  signal: AbortSignal,
  cancel: () => void,
): ReadableStream<Uint8Array> {
  // The finalizer's target (see `unreadBodies`). `settle` refers to it, so the
  // stream's `pull` and `cancel` keep it alive for as long as the stream is.
  const token = {}
  let done = false
  let controller: ReadableStreamDefaultController<Uint8Array> | null = null
  const settle = () => {
    done = true
    signal.removeEventListener('abort', onAbort)
    signal.removeEventListener('abort', cancel)
    unreadBodies?.unregister(token)
  }
  // `cancel` above tells the plugin. This fails the reader now, with the
  // signal's reason, rather than when the plugin's answer arrives.
  const onAbort = () => {
    if (done) {
      return
    }
    done = true
    signal.removeEventListener('abort', onAbort)
    unreadBodies?.unregister(token)
    controller?.error(signal.reason)
  }
  const stream = new ReadableStream<Uint8Array>({
    start(started) {
      controller = started
      signal.addEventListener('abort', onAbort, { once: true })
    },
    async pull(pulled) {
      let frame: ArrayBuffer
      try {
        frame = await invoke<ArrayBuffer>('plugin:axon-http|read_body', { id })
      } catch (error) {
        if (!done) {
          settle()
          pulled.error(failure(error))
        }
        return
      }
      if (done) {
        return
      }
      const bytes = new Uint8Array(frame)
      if (bytes.length === 0) {
        // Not a frame the plugin sends: the entry is in no known state, so
        // release it rather than read on.
        settle()
        void invoke('plugin:axon-http|drop_body', { id }).catch(() => {})
        pulled.error(failure({ kind: 'other', detail: null }))
        return
      }
      if (bytes[bytes.length - 1] !== 0) {
        // The end, and the plugin has already let the entry go.
        settle()
        pulled.close()
        return
      }
      pulled.enqueue(bytes.subarray(0, bytes.length - 1))
    },
    cancel() {
      if (done) {
        return
      }
      settle()
      void invoke('plugin:axon-http|drop_body', { id }).catch(() => {})
    },
  })
  unreadBodies?.register(token, id, token)
  return stream
}

/**
 * `work`, or the signal's reason as soon as it fires, whichever is first. The
 * listener goes when `work` settles, so a later abort rejects nothing.
 */
function untilAborted<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(signal.reason)
    signal.addEventListener('abort', onAbort, { once: true })
    work.then(
      (value) => {
        signal.removeEventListener('abort', onAbort)
        resolve(value)
      },
      (error: unknown) => {
        signal.removeEventListener('abort', onAbort)
        reject(error)
      },
    )
  })
}

/**
 * The transport's rejection as the `TypeError` `fetch` promises, with the
 * category on its `cause`. A rejection that is not one of the transport's own
 * (an ACL denial, a command the shell does not have) is `other`: its text is
 * the invoke layer's and names no host, so it is logged for whoever is
 * debugging, and kept out of the error itself.
 */
function failure(error: unknown): TypeError {
  let cause: TransportFailure | null = asTransportFailure(error)
  if (cause === null) {
    console.warn(
      "the shell's transport rejected a call",
      typeof error === 'string' ? error : String(error),
    )
    cause = { kind: 'other', detail: null }
  }
  return new TypeError(`the shell's transport failed (${cause.kind})`, {
    cause,
  })
}

/**
 * Swap in fresh clients after resume (ADR 0127 § 6). In-flight requests finish
 * on the old ones, and the origins known to speak HTTP/2 are kept.
 */
export function resetShellTransport(): Promise<void> {
  return invoke<void>('plugin:axon-http|reset')
}

/** The transport's counters, which persist across a reset. */
export function shellTransportStats(): Promise<TransportStats> {
  return invoke<TransportStats>('plugin:axon-http|stats')
}
