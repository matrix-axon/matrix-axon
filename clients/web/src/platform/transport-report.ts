/**
 * What the shell's transport reports about a request, beyond the `Response`:
 * which HTTP version carried it, and why it failed (ADR 0127 §§ 4–5).
 *
 * A module of its own, with no imports, so the API client can read these
 * without pulling in the shell's IPC. The binding that produces them is
 * `shell-http.ts`.
 *
 * Both are closed sets, because traces are written to disk. Anything else the
 * shell sent is read as unknown rather than stored.
 */

export const TRANSPORT_VERSIONS = [
  'h2',
  'h3',
  'http/1.1',
  'http/1.0',
  'http/0.9',
] as const

export type TransportVersion = (typeof TRANSPORT_VERSIONS)[number]

/**
 * The categories the transport reduces a failure to (`Kind` in
 * `src-tauri/axon-http/src/error.rs`). `cancelled` is a cancel the transport
 * decided on itself, such as the webview going away: a cancel the page asked
 * for rejects with the page's own `AbortError` instead.
 */
export const TRANSPORT_FAILURE_KINDS = [
  'dns',
  'refused',
  'connect_timeout',
  'tls',
  'reset',
  'body',
  'cancelled',
  'scope',
  'other',
] as const

export type TransportFailureKind = (typeof TRANSPORT_FAILURE_KINDS)[number]

/**
 * A failure's category and its detail: an `io::ErrorKind` name such as
 * `ConnectionReset`, a certificate or alert name such as `UnknownIssuer`, or
 * `null`. Never error text, which can name a host.
 */
export interface TransportFailure {
  kind: TransportFailureKind
  detail: string | null
}

/**
 * What a `detail` may look like. The Rust side only sends variant names, and
 * this holds the page to that too: a value that is not one is dropped rather
 * than written to disk.
 */
const DETAIL = /^[A-Za-z]{1,40}$/

const versions = new WeakMap<Response, TransportVersion>()

function isTransportVersion(value: unknown): value is TransportVersion {
  return (
    typeof value === 'string' &&
    (TRANSPORT_VERSIONS as readonly string[]).includes(value)
  )
}

function isFailureKind(value: unknown): value is TransportFailureKind {
  return (
    typeof value === 'string' &&
    (TRANSPORT_FAILURE_KINDS as readonly string[]).includes(value)
  )
}

export function recordTransportVersion(
  response: Response,
  version: unknown,
): void {
  if (isTransportVersion(version)) {
    versions.set(response, version)
  }
}

/**
 * The version that carried `response`, or `null` when no transport said. A
 * browser's `fetch` never says: the readout has Resource Timing's
 * `nextHopProtocol` for those.
 */
export function transportVersion(response: Response): TransportVersion | null {
  return versions.get(response) ?? null
}

/**
 * Read `{ kind, detail }` off whatever the transport rejected with, keeping
 * only what is in the closed sets. `null` when it is not one of ours.
 */
export function asTransportFailure(value: unknown): TransportFailure | null {
  if (typeof value !== 'object' || value === null || !('kind' in value)) {
    return null
  }
  const { kind } = value
  if (!isFailureKind(kind)) {
    return null
  }
  const detail = 'detail' in value ? value.detail : null
  return {
    kind,
    detail: typeof detail === 'string' && DETAIL.test(detail) ? detail : null,
  }
}

/**
 * The transport's failure behind a rejected request, found on its `cause` as
 * the binding puts it, or `null` for any other error.
 */
export function transportFailureOf(error: unknown): TransportFailure | null {
  if (typeof error !== 'object' || error === null || !('cause' in error)) {
    return null
  }
  return asTransportFailure(error.cause)
}
