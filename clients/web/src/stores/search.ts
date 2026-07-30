import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { apiErrorMessage, type ApiClient } from '../api/client'
import type { components } from '../api/schema'
import { toApiParams, type SearchQuery } from '../search-tokens'

export type SearchResult = components['schemas']['SearchResultDto']

/** Page size for the first page and each `loadMore`. */
const PAGE_LIMIT = 50

export interface SearchStore {
  /** Loaded hits in the server's relevance order (most relevant first).
   *  Date sort is the overlay's view concern, not the store's. */
  results: ReadonlySignal<SearchResult[]>
  /** Total matches across all pages, or null before the first run settles. */
  total: ReadonlySignal<number | null>
  /** True while a `run` replaces the results. */
  loading: ReadonlySignal<boolean>
  /** True while a `loadMore` appends a page. */
  loadingMore: ReadonlySignal<boolean>
  /** True when no further page exists for the last-run query. */
  exhausted: ReadonlySignal<boolean>
  error: Signal<string | null>
  /** The server answered 503: search is disabled there (ADR 0066), a state
   *  the overlay renders as "unavailable", not as an error. */
  unavailable: ReadonlySignal<boolean>
  /** The query that produced the currently cached result set, if any. */
  lastQuery: ReadonlySignal<SearchQuery | null>

  /** Run a query, replacing any loaded results with its first page. */
  run(query: SearchQuery): Promise<void>
  /** Append the next page of the last-run query. */
  loadMore(): Promise<void>
  /** Back to the never-searched state. */
  clear(): void
  /** Keep the cache through the next cross-room navigation from a result. */
  preserveForResultJump(): void
  /** Consume the one-shot preservation requested by a result jump. */
  consumeResultJumpPreservation(): boolean
}

/**
 * The global search surface over `GET /v1/search` (ADR 0066, M-W10): one
 * store for the app, not per-room — a query's scope is a parameter, not the
 * store's identity. Pagination mirrors `timeline.ts`: an opaque cursor and a
 * generation guard so a `loadMore` racing a fresh `run` discards its page
 * instead of appending hits from the previous query.
 */
export function createSearchStore(api: ApiClient): SearchStore {
  const results = signal<SearchResult[]>([])
  const total = signal<number | null>(null)
  const loading = signal(false)
  const loadingMore = signal(false)
  const nextCursor = signal<string | null>(null)
  const error = signal<string | null>(null)
  const unavailable = signal(false)
  let generation = 0
  let preserveForResultJump = false
  const lastQuery = signal<SearchQuery | null>(null)

  async function fetchPage(
    query: SearchQuery,
    cursor: string | null,
  ): Promise<{
    results: SearchResult[]
    total: number
    next: string | null
  } | null> {
    try {
      const {
        data,
        error: apiError,
        response,
      } = await api.GET('/v1/search', {
        params: {
          query: {
            ...toApiParams(query),
            limit: PAGE_LIMIT,
            ...(cursor === null ? {} : { cursor }),
          },
        },
      })
      if (apiError !== undefined) {
        if (response.status === 503) {
          unavailable.value = true
          error.value = null
        } else {
          error.value = apiErrorMessage(apiError)
        }
        return null
      }
      error.value = null
      unavailable.value = false
      return {
        results: data.data.results,
        total: data.data.total,
        next: data.data.next_cursor ?? null,
      }
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : String(cause)
      return null
    }
  }

  async function run(query: SearchQuery): Promise<void> {
    generation += 1
    const started = generation
    lastQuery.value = query
    loading.value = true
    // A superseded in-flight `loadMore` discards its page but cannot clean
    // up after itself (its generation check comes after the await).
    loadingMore.value = false
    results.value = []
    total.value = null
    nextCursor.value = null
    unavailable.value = false
    const page = await fetchPage(query, null)
    if (generation !== started) {
      return
    }
    loading.value = false
    if (page === null) {
      return
    }
    results.value = page.results
    total.value = page.total
    nextCursor.value = page.next
  }

  async function loadMore(): Promise<void> {
    const cursor = nextCursor.value
    if (cursor === null || loadingMore.value || lastQuery.value === null) {
      return
    }
    const started = generation
    loadingMore.value = true
    const page = await fetchPage(lastQuery.value, cursor)
    if (generation !== started) {
      return
    }
    loadingMore.value = false
    if (page === null) {
      return
    }
    results.value = [...results.value, ...page.results]
    total.value = page.total
    nextCursor.value = page.next
  }

  function clear(): void {
    generation += 1
    preserveForResultJump = false
    lastQuery.value = null
    results.value = []
    total.value = null
    loading.value = false
    loadingMore.value = false
    nextCursor.value = null
    error.value = null
    unavailable.value = false
  }

  function consumeResultJumpPreservation(): boolean {
    const preserved = preserveForResultJump
    preserveForResultJump = false
    return preserved
  }

  return {
    results: computed(() => results.value),
    total: computed(() => total.value),
    loading: computed(() => loading.value),
    loadingMore: computed(() => loadingMore.value),
    exhausted: computed(() => nextCursor.value === null),
    error,
    unavailable: computed(() => unavailable.value),
    lastQuery: computed(() => lastQuery.value),
    run,
    loadMore,
    clear,
    preserveForResultJump: () => {
      preserveForResultJump = true
    },
    consumeResultJumpPreservation,
  }
}
