import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { apiErrorMessage, inBackground, type ApiClient } from '../api/client'
import { perfMark } from '../perf'
import type { components } from '../api/schema'
import type { EventDto } from './timeline'

export type ThreadSummaryDto = components['schemas']['ThreadSummaryDto']

/** The `m.thread` root id when this event is a thread member, else null. */
export function threadRootId(event: EventDto): string | null {
  const relates = event.relates_to as {
    rel_type?: unknown
    event_id?: unknown
  } | null
  return relates?.rel_type === 'm.thread' &&
    typeof relates.event_id === 'string'
    ? relates.event_id
    : null
}

export interface ThreadsStore {
  /** Thread summaries keyed by root event id (badges + the thread list). */
  summaries: ReadonlySignal<ReadonlyMap<string, ThreadSummaryDto>>
  /** Resolved root events, keyed by root id (for the thread list display). */
  roots: ReadonlySignal<ReadonlyMap<string, EventDto>>
  loading: ReadonlySignal<boolean>
  error: Signal<string | null>

  /** Fetch the room's thread summaries and resolve their root events. */
  refresh(): Promise<void>
  /**
   * Drop the root events still waiting to be fetched; call on leaving the
   * room. Requests already sent finish, and a later `refresh` asks again for
   * whatever was dropped.
   */
  stop(): void
}

/**
 * How many root events are fetched at once.
 *
 * There is one `GET …/events/{id}` per thread and no batch form, and this
 * used to send them all together. A room with about 300 threads therefore
 * issued about 300 requests inside a second on every open (#662). A browser
 * multiplexes those over one connection. The packaged app does not: its
 * transport opens a connection per request (#663), so that was about 300
 * simultaneous TLS handshakes from a phone, and the timeline page of whatever
 * room was opened next failed outright among them.
 *
 * Six is a browser's own per-host limit over HTTP/1.1, and the media
 * service's (`MAX_CONCURRENT`). The cost is that a long thread list fills in
 * over a few seconds rather than at once, which is why the queue is ordered:
 * see `resolveRoots`.
 */
export const ROOT_FETCH_CONCURRENCY = 6

/**
 * One room's threads (ADR 0046, M-W7; ADR 0032 M8 read model): the summary
 * list drives thread badges on root rows in the main timeline and the thread
 * panel's list; members are paged by a thread-scoped timeline store.
 */
export function createThreadsStore(
  api: ApiClient,
  accountId: string,
  roomId: string,
): ThreadsStore {
  const summaries = signal<ReadonlyMap<string, ThreadSummaryDto>>(new Map())
  const roots = signal<ReadonlyMap<string, EventDto>>(new Map())
  const loading = signal(true)
  const error = signal<string | null>(null)
  /**
   * Root ids fetched, in flight, or queued; misses are not retried this
   * session.
   */
  const requestedRoots = new Set<string>()
  /** Root ids waiting for a slot, next first. */
  let queue: string[] = []
  let fetching = 0

  /** Start queued fetches until `ROOT_FETCH_CONCURRENCY` are in flight. */
  function pump(): void {
    while (fetching < ROOT_FETCH_CONCURRENCY) {
      const id = queue.shift()
      if (id === undefined) {
        return
      }
      fetching += 1
      inBackground(
        api
          .GET('/v1/accounts/{account_id}/events/{event_id}', {
            params: { path: { account_id: accountId, event_id: id } },
          })
          .then(({ data }) => {
            if (data !== undefined) {
              roots.value = new Map(roots.value).set(id, data.data)
            }
          })
          .finally(() => {
            fetching -= 1
            pump()
          }),
      )
    }
  }

  /**
   * Queue the roots not yet asked for, most recently active thread first.
   *
   * The order is the thread list's own, newest reply at the top, so the rows
   * a reader can see get their previews first and the tail of a long list is
   * what waits. The whole queue is re-sorted, not only the additions: a
   * refresh after a live reply must put that thread ahead of older ones still
   * waiting from the first pass.
   */
  function resolveRoots(threads: readonly ThreadSummaryDto[]): void {
    for (const { root_event_id: id } of threads) {
      if (!requestedRoots.has(id)) {
        requestedRoots.add(id)
        queue.push(id)
      }
    }
    const latest = new Map(
      threads.map((each) => [each.root_event_id, each.latest_reply_ts ?? 0]),
    )
    queue.sort((a, b) => (latest.get(b) ?? 0) - (latest.get(a) ?? 0))
    pump()
  }

  return {
    summaries: computed(() => summaries.value),
    roots: computed(() => roots.value),
    loading: computed(() => loading.value),
    error,

    async refresh() {
      // Third of the four requests a room open fires at once; marked for the
      // same reason as the members list, which is that on a weak link the
      // question is which of them the timeline page is competing with.
      perfMark('threads:refresh:start', { roomId })
      try {
        const { data, error: apiError } = await api.GET(
          '/v1/accounts/{account_id}/rooms/{room_id}/threads',
          { params: { path: { account_id: accountId, room_id: roomId } } },
        )
        if (apiError !== undefined) {
          error.value = apiErrorMessage(apiError)
          return
        }
        error.value = null
        const next = new Map<string, ThreadSummaryDto>()
        for (const summary of data.data) {
          next.set(summary.root_event_id, summary)
        }
        summaries.value = next
        resolveRoots(data.data)
      } catch (cause) {
        error.value = cause instanceof Error ? cause.message : String(cause)
      } finally {
        loading.value = false
        perfMark('threads:refresh:end', {
          roomId,
          threads: summaries.value.size,
          ok: error.value === null,
        })
      }
    },

    stop() {
      // Forgotten as well as dropped, so they are not mistaken for misses.
      for (const id of queue) {
        requestedRoots.delete(id)
      }
      queue = []
    },
  }
}
