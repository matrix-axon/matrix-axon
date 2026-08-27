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
}

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
  /** Root ids fetched (or in flight); misses are not retried this session. */
  const requestedRoots = new Set<string>()

  function resolveRoots(rootIds: string[]): void {
    for (const id of rootIds) {
      if (requestedRoots.has(id)) {
        continue
      }
      requestedRoots.add(id)
      inBackground(
        api
          .GET('/v1/accounts/{account_id}/events/{event_id}', {
            params: { path: { account_id: accountId, event_id: id } },
          })
          .then(({ data }) => {
            if (data !== undefined) {
              roots.value = new Map(roots.value).set(id, data.data)
            }
          }),
      )
    }
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
        resolveRoots([...next.keys()])
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
  }
}
