import type { ApiClient } from '../api/client'
import type { Platform } from '../platform'
import { mxcToPath } from './parse-media'

/**
 * Stream tickets (ADR 0139 § 5 and § 6): a URL a `<video>` or `<audio>`
 * element loads without an `Authorization` header, so the browser streams the
 * object with its own range requests instead of the page buffering it into a
 * blob first.
 *
 * The ticket path is a credential for one object. It is handed to the media
 * element and the error probe, and nowhere else: never logged, never stored.
 */

/** Where every ticket path the server issues begins. */
const STREAM_PREFIX = '/v1/media-stream/'

/** How long the error probe waits before calling the server unreachable. */
const PROBE_TIMEOUT_MS = 15_000

export type StreamTicketOutcome =
  /** The element's `src`. */
  | { ok: true; src: string }
  /**
   * The server will not stream this object: it would not serve it as
   * `video/*` or `audio/*` (`400`). The caller falls back to the blob path.
   */
  | { ok: false; kind: 'refused' }
  /** Anything else: the event is gone, the server is down, a network error. */
  | { ok: false; kind: 'failed' }

/**
 * What a `Range: bytes=0-0` probe of a ticket URL says about a media
 * element's `error`, which reports a refused request and an unsupported codec
 * alike (ADR 0139 § 6).
 */
export type StreamProbe =
  /** The bytes are reachable, so the failure was the element's own. */
  | 'reachable'
  /** The ticket expired, its token ended, or the server restarted. */
  | 'expired'
  /** The encrypted attachment failed its hash check. Terminal. */
  | 'undecryptable'
  /** Anything else, including no answer at all. */
  | 'failed'

export interface StreamTickets {
  /**
   * Whether this platform's media elements can load a ticket URL at all. When
   * false, video and audio keep the blob path (ADR 0139 § 1).
   */
  readonly available: boolean
  /** Ask the server for a ticket for one object, and resolve its `src`. */
  issue(accountId: string, mxcUrl: string): Promise<StreamTicketOutcome>
  /** Probe a ticket `src` after its media element reported an error. */
  probe(src: string): Promise<StreamProbe>
}

export function createStreamTickets(deps: {
  api: ApiClient
  baseUrl: string
  platform: Pick<Platform, 'mediaStreamSrc'>
  /**
   * The probe's transport. The page's own `fetch`, not the platform's: the
   * probe must reach exactly the URL the media element loaded, which in a
   * shell is the `axon-media` scheme rather than the server (ADR 0139 § 7).
   */
  fetch?: typeof globalThis.fetch
}): StreamTickets {
  const root = deps.baseUrl.replace(/\/$/, '')
  const toSrc = deps.platform.mediaStreamSrc
  const probeFetch =
    deps.fetch ??
    ((...args: Parameters<typeof globalThis.fetch>) =>
      globalThis.fetch(...args))

  /** Resolve a ticket path against the server base, as `mediaUrl` does. */
  const resolve = (path: string): string => {
    const joined = `${root}${path}`
    return typeof document === 'undefined'
      ? joined
      : new URL(joined, document.baseURI).href
  }

  return {
    available: toSrc !== null,

    async issue(accountId, mxcUrl) {
      const parts = mxcToPath(mxcUrl)
      if (toSrc === null || parts === null) {
        return { ok: false, kind: 'refused' }
      }
      try {
        const { data, response } = await deps.api.POST(
          '/v1/media/{account_id}/{server_name}/{media_id}/stream-ticket',
          {
            params: {
              path: {
                account_id: accountId,
                server_name: parts.serverName,
                media_id: parts.mediaId,
              },
            },
          },
        )
        if (data === undefined) {
          return response.status === 400
            ? { ok: false, kind: 'refused' }
            : { ok: false, kind: 'failed' }
        }
        const path = data.data.path
        // Ours to load, and nothing else: a path outside the prefix would
        // turn a server bug into the page loading an arbitrary URL.
        if (!path.startsWith(STREAM_PREFIX) || path.includes('..')) {
          return { ok: false, kind: 'failed' }
        }
        return { ok: true, src: await toSrc(resolve(path)) }
      } catch {
        return { ok: false, kind: 'failed' }
      }
    },

    async probe(src) {
      let response: Response
      try {
        response = await probeFetch(src, {
          // The header tells the shell's `axon-media` scheme to ask the
          // server rather than answer from its read-ahead window, which may
          // still hold these bytes after the ticket has expired (ADR 0139
          // § 7). The server ignores it.
          headers: { Range: 'bytes=0-0', 'Axon-Stream-Probe': '1' },
          cache: 'no-store',
          referrerPolicy: 'no-referrer',
          signal: AbortSignal.timeout(PROBE_TIMEOUT_MS),
        })
      } catch {
        return 'failed'
      }
      // One byte at most, but there is no reason to hold the stream open.
      void response.body?.cancel().catch(() => {})
      if (response.ok) {
        return 'reachable'
      }
      switch (response.status) {
        case 401:
        case 403:
        case 404:
          return 'expired'
        case 422:
          return 'undecryptable'
        default:
          return 'failed'
      }
    },
  }
}
