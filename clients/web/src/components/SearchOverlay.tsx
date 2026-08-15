import { useLocation } from 'preact-iso'
import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import {
  currentRoomFromPath,
  hasNarrowingFilter,
  highlightSnippet,
  parseSearchTokens,
  queryTerms,
  serializeSearchTokens,
  withoutSearchParam,
  withSearchParam,
  type ParsedSearch,
  type SearchQuery,
  type SearchTokenContext,
} from '../search-tokens'
import { useServices } from '../services'
import { useShortcuts } from '../shortcuts'
import type { SearchResult } from '../stores/search'
import { accountLabels, localpart, roomTitle } from '../stores/room-list'
import { threadRootId } from '../stores/threads'
import { localRoomHref, localThreadEventHref } from '../matrix-to'
import { useModalFocus } from './use-modal-focus'

type ResultSort = 'relevance' | 'newest' | 'oldest'

/**
 * The message-search overlay (ADR 0066, M-W10). Its open state and query live
 * in the URL — `ShellChrome` mounts it while `?search=` is present, and every
 * submit re-routes with the new token string — so a search is shareable, the
 * Back button closes it, and this component holds only editing state: the
 * text box between submits.
 *
 * Chips and tokens are two views of the same `SearchQuery`: the URL param is
 * parsed into chips, and a chip edit or input submit is serialized back into
 * the URL. Nothing here talks to the store directly except the one effect
 * that runs whatever query the URL currently carries.
 */
export function SearchOverlay() {
  const location = useLocation()
  const { search, rooms, accounts } = useServices()
  const { containerRef } = useModalFocus<HTMLDivElement>()

  const tokens =
    typeof location.query.search === 'string' ? location.query.search : ''
  const sort: ResultSort =
    location.query.ssort === 'newest' || location.query.ssort === 'oldest'
      ? location.query.ssort
      : 'relevance'

  const ctx: SearchTokenContext = {
    rooms: rooms.rooms.value,
    accounts: accounts.accounts.value,
    current: currentRoomFromPath(location.path),
  }
  const parsed: ParsedSearch = useMemo(
    () => parseSearchTokens(tokens, ctx),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- ctx is rebuilt per render; its signals are the deps
    [tokens, rooms.rooms.value, accounts.accounts.value, location.path],
  )

  // The text box holds the free text between submits; chips render the rest.
  // A URL change (submit, Back, a shared link) resets it to the URL's truth.
  const [input, setInput] = useState(parsed.query.text)
  /** Parse errors from the *box*, blocking a submit — distinct from
   *  `parsed.errors`, which belong to the URL's already-submitted tokens. */
  const [inputErrors, setInputErrors] = useState<string[]>([])
  // Only on an actual change: the mount run is a no-op by intent (the state
  // above initializes from the same value), but effects flush after paint, so
  // it would clobber anything typed into a box the user can already see.
  const seenTokens = useRef(tokens)
  useEffect(() => {
    if (seenTokens.current === tokens) {
      return
    }
    seenTokens.current = tokens
    setInput(parsed.query.text)
    setInputErrors([])
    // eslint-disable-next-line react-hooks/exhaustive-deps -- reset on URL change only, not on store loads re-parsing
  }, [tokens])

  // Cold-loaded (a shared link): the resolvers need the lists nobody else on
  // a utility page has fetched yet.
  useEffect(() => {
    void rooms.ensureLoaded()
    if (accounts.accounts.value.length === 0) {
      void accounts.refresh()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once on open
  }, [])

  const runnable =
    parsed.errors.length === 0 &&
    (parsed.query.text.trim() !== '' || hasNarrowingFilter(parsed.query))

  // Run whatever the URL carries. Keyed on the canonical serialization, not
  // the raw param: a rooms-store load re-parses the same tokens to the same
  // query, and must not re-fire the request.
  const lastRan = useRef<string | null>(null)
  useEffect(() => {
    if (tokens === '' || !runnable) {
      return
    }
    const canonical = serializeSearchTokens(parsed.query)
    if (lastRan.current === canonical) {
      return
    }
    if (
      search.lastQuery.value !== null &&
      search.total.value !== null &&
      serializeSearchTokens(search.lastQuery.value) === canonical
    ) {
      lastRan.current = canonical
      return
    }
    lastRan.current = canonical
    void search.run(parsed.query)
  }, [tokens, runnable, parsed, search])

  const close = () => location.route(withoutSearchParam(location.url))
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        close()
      },
    },
    { whileTyping: true, capture: true },
  )

  /** Serialize a query into the URL (replacing — one Back closes the whole
   *  search session, per the ADR) and let the run effect pick it up. */
  const apply = (next: SearchQuery) => {
    location.route(
      withSearchParam(location.url, serializeSearchTokens(next)),
      true,
    )
  }

  /**
   * The submitted chips with the box's tokens merged over them: a typed
   * `field:` wins over its chip; an untyped field keeps its chip. `null`
   * blocks the caller: a token that failed to parse (an unresolvable
   * `room:`, a bad date) must stop the submit and say so — silently
   * searching *without* the filter the user typed answers a question they
   * did not ask. (Live-check finding: `room:` typed before the rooms list
   * loads is exactly this case.)
   */
  const merged = (): SearchQuery | null => {
    const typed = parseSearchTokens(input, ctx)
    if (typed.errors.length > 0) {
      setInputErrors(typed.errors)
      return null
    }
    setInputErrors([])
    return {
      text: typed.query.text,
      scope: typed.explicit.scope ? typed.query.scope : parsed.query.scope,
      sender: typed.explicit.sender ? typed.query.sender : parsed.query.sender,
      from: typed.explicit.from ? typed.query.from : parsed.query.from,
      to: typed.explicit.to ? typed.query.to : parsed.query.to,
    }
  }

  const results = search.results.value
  const sorted = useMemo(() => {
    if (sort === 'relevance') {
      return results
    }
    const factor = sort === 'newest' ? -1 : 1
    return [...results].sort(
      (a, b) => factor * (a.event.origin_ts - b.event.origin_ts),
    )
  }, [results, sort])

  const terms = useMemo(() => queryTerms(parsed.query.text), [parsed])
  const labels = useMemo(
    () =>
      accountLabels(
        accounts.accounts.value.map((account) => [
          account.account_id,
          account.user_id,
        ]),
      ),
    [accounts.accounts.value],
  )
  // The account column only earns its width when hits can span accounts.
  const showAccount =
    parsed.query.scope.kind === 'all' && accounts.accounts.value.length > 1

  const setSort = (next: ResultSort) => {
    const base = withSearchParam(location.url, tokens)
    const [pathPart, queryString = ''] = base.split('?', 2)
    const params = new URLSearchParams(queryString)
    if (next === 'relevance') {
      params.delete('ssort')
    } else {
      params.set('ssort', next)
    }
    location.route(`${pathPart}?${params.toString()}`, true)
  }

  const total = search.total.value
  const searched = tokens !== '' && runnable

  return (
    <div
      ref={containerRef}
      class="overlay"
      role="dialog"
      aria-modal="true"
      aria-label="Search messages"
    >
      <div class="overlay-panel search-panel">
        <form
          class="search-form"
          onSubmit={(event) => {
            event.preventDefault()
            const next = merged()
            if (next !== null) {
              apply(next)
            }
          }}
        >
          <input
            type="search"
            class="search-input"
            placeholder="Search messages…"
            aria-label="Search query"
            value={input}
            onInput={(event) => setInput(event.currentTarget.value)}
          />
          <button type="submit">Search</button>
          <button type="button" class="ghost" onClick={close}>
            Close
          </button>
        </form>

        <div class="search-chips">
          <ScopeChip
            query={parsed.query}
            ctx={ctx}
            titles={rooms.titles.value}
            labels={labels}
            onChange={(scope) => {
              const next = merged()
              if (next !== null) {
                apply({ ...next, scope })
              }
            }}
          />
          {parsed.query.sender !== null && (
            <Chip
              label={`sender: ${parsed.query.sender}`}
              onRemove={() => {
                const next = merged()
                if (next !== null) {
                  apply({ ...next, sender: null })
                }
              }}
            />
          )}
          {parsed.query.from !== null && (
            <Chip
              label={`after ${new Date(parsed.query.from).toLocaleDateString()}`}
              onRemove={() => {
                const next = merged()
                if (next !== null) {
                  apply({ ...next, from: null })
                }
              }}
            />
          )}
          {parsed.query.to !== null && (
            <Chip
              label={`before ${new Date(parsed.query.to).toLocaleDateString()}`}
              onRemove={() => {
                const next = merged()
                if (next !== null) {
                  apply({ ...next, to: null })
                }
              }}
            />
          )}
          {searched && total !== null && (
            <label class="search-sort">
              sort
              <select
                value={sort}
                onChange={(event) =>
                  setSort(event.currentTarget.value as ResultSort)
                }
              >
                <option value="relevance">relevance</option>
                <option value="newest">newest</option>
                <option value="oldest">oldest</option>
              </select>
            </label>
          )}
        </div>

        {(inputErrors.length > 0 || parsed.errors.length > 0) && (
          <div class="banner error" role="alert">
            {[...inputErrors, ...parsed.errors].join('; ')}
          </div>
        )}
        {search.error.value !== null && (
          <div class="banner error" role="alert">
            {search.error.value}
          </div>
        )}

        {search.unavailable.value ? (
          <p class="search-status">Search is not enabled on this server.</p>
        ) : !searched ? (
          <p class="search-status">
            Type to search{ctx.current !== null ? ' this room' : ''}. Narrow
            with <code>room:</code>, <code>account:</code>, <code>sender:</code>
            , <code>after:</code>, <code>before:</code>, <code>date:</code>, or
            quote a phrase; <code>*</code> or <code>all:true</code> searches
            everywhere.
          </p>
        ) : search.loading.value ? (
          <p class="search-status">Searching…</p>
        ) : total === null ? null : total === 0 ? (
          <p class="search-status">No matches.</p>
        ) : (
          <>
            <p class="search-status">
              {total} {total === 1 ? 'result' : 'results'}
              {sort !== 'relevance' && !search.exhausted.value
                ? ' — sorted among loaded results'
                : ''}
            </p>
            <ol class="search-results">
              {sorted.map((result) => (
                <SearchHit
                  key={result.event.event_id}
                  result={result}
                  terms={terms}
                  ctx={ctx}
                  titles={rooms.titles.value}
                  accountLabel={
                    showAccount
                      ? (labels.get(result.event.account_id) ??
                        result.event.account_id)
                      : null
                  }
                />
              ))}
            </ol>
            {!search.exhausted.value && (
              <LoadMore
                loading={search.loadingMore.value}
                onLoad={() => void search.loadMore()}
              />
            )}
          </>
        )}
      </div>
    </div>
  )
}

function Chip({ label, onRemove }: { label: string; onRemove: () => void }) {
  return (
    <span class="search-chip">
      {label}
      <button
        type="button"
        class="chip-remove"
        aria-label={`Remove filter: ${label}`}
        onClick={onRemove}
      >
        ×
      </button>
    </span>
  )
}

/**
 * The scope control: a select over the scopes reachable by pointer — the open
 * room, each account, everywhere — plus, when the current scope is a room
 * arrived at by token (`room:ops`), that room as a labelled entry so the
 * select never lies about what is active.
 */
function ScopeChip({
  query,
  ctx,
  titles,
  labels,
  onChange,
}: {
  query: SearchQuery
  ctx: SearchTokenContext
  titles: ReadonlyMap<string, string>
  labels: Map<string, string>
  onChange: (scope: SearchQuery['scope']) => void
}) {
  const scope = query.scope
  const scopedRoom =
    scope.kind === 'room'
      ? (ctx.rooms.find(
          (room) =>
            room.account_id === scope.accountId &&
            room.room_id === scope.roomId,
        ) ?? null)
      : null
  const isCurrent =
    scope.kind === 'room' &&
    ctx.current !== null &&
    scope.accountId === ctx.current.accountId &&
    scope.roomId === ctx.current.roomId

  const value =
    scope.kind === 'all'
      ? 'all'
      : scope.kind === 'account'
        ? `account:${scope.accountId}`
        : isCurrent
          ? 'current'
          : 'room'

  return (
    <label class="search-scope">
      in
      <select
        value={value}
        onChange={(event) => {
          const next = event.currentTarget.value
          if (next === 'all') {
            onChange({ kind: 'all' })
          } else if (next === 'current' && ctx.current !== null) {
            onChange({ kind: 'room', ...ctx.current })
          } else if (next.startsWith('account:')) {
            onChange({ kind: 'account', accountId: next.slice(8) })
          }
        }}
      >
        {ctx.current !== null && <option value="current">this room</option>}
        {!isCurrent && scope.kind === 'room' && (
          <option value="room">
            {scopedRoom !== null ? roomTitle(scopedRoom, titles) : scope.roomId}
          </option>
        )}
        {ctx.accounts.map((account) => (
          <option
            key={account.account_id}
            value={`account:${account.account_id}`}
          >
            account: {labels.get(account.account_id) ?? account.user_id}
          </option>
        ))}
        <option value="all">everywhere</option>
      </select>
    </label>
  )
}

function SearchHit({
  result,
  terms,
  ctx,
  titles,
  accountLabel,
}: {
  result: SearchResult
  terms: string[]
  ctx: SearchTokenContext
  titles: ReadonlyMap<string, string>
  accountLabel: string | null
}) {
  const { search } = useServices()
  const { event } = result
  const room = ctx.rooms.find(
    (candidate) =>
      candidate.account_id === event.account_id &&
      candidate.room_id === event.room_id,
  )
  const segments = event.redacted
    ? [{ text: 'message deleted', hit: false }]
    : highlightSnippet(event.body ?? '', terms)
  const rootId = threadRootId(event)
  const href =
    rootId === null
      ? localRoomHref(event.account_id, event.room_id, event.event_id)
      : localThreadEventHref(
          event.account_id,
          event.room_id,
          rootId,
          event.event_id,
        )
  return (
    <li>
      <a
        class="search-hit"
        href={href}
        onClick={(click) => {
          if (
            click.button === 0 &&
            !click.metaKey &&
            !click.ctrlKey &&
            !click.altKey &&
            !click.shiftKey &&
            (ctx.current === null ||
              ctx.current.accountId !== event.account_id ||
              ctx.current.roomId !== event.room_id)
          ) {
            search.preserveForResultJump()
          }
        }}
      >
        <span class="search-hit-meta">
          <span class="search-hit-room">
            {room !== undefined ? roomTitle(room, titles) : event.room_id}
          </span>
          {accountLabel !== null && (
            <span class="search-hit-account">{accountLabel}</span>
          )}
          <span class="search-hit-sender">{localpart(event.sender)}</span>
          <time class="search-hit-time">
            {new Date(event.origin_ts).toLocaleString(undefined, {
              dateStyle: 'medium',
              timeStyle: 'short',
            })}
          </time>
        </span>
        <span class={`search-hit-body${event.redacted ? ' redacted' : ''}`}>
          {segments.map((segment, index) =>
            segment.hit ? (
              <mark key={index}>{segment.text}</mark>
            ) : (
              segment.text
            ),
          )}
        </span>
      </a>
    </li>
  )
}

/**
 * Pagination, the Timeline's pattern: the button is the real (and jsdom-
 * testable) control; the sentinel auto-clicks it where IntersectionObserver
 * exists.
 */
function LoadMore({
  loading,
  onLoad,
}: {
  loading: boolean
  onLoad: () => void
}) {
  const sentinel = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (typeof IntersectionObserver === 'undefined') {
      return
    }
    const el = sentinel.current
    if (el === null) {
      return
    }
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        onLoad()
      }
    })
    observer.observe(el)
    return () => observer.disconnect()
    // eslint-disable-next-line react-hooks/exhaustive-deps -- observe once per mount
  }, [])
  return (
    <div class="search-load-more">
      <div ref={sentinel} aria-hidden="true" />
      <button type="button" class="ghost" disabled={loading} onClick={onLoad}>
        {loading ? 'Loading…' : 'Load more results'}
      </button>
    </div>
  )
}
