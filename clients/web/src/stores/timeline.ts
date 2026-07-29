import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { apiErrorMessage, inBackground, type ApiClient } from '../api/client'
import type { components } from '../api/schema'
import { markdownToHtmlIfFormatted } from '../markdown/markdown'
import {
  uploadFailureMessage,
  uploadKind,
  type MediaService,
  type UploadKind,
} from '../media/media-service'

export type EventDto = components['schemas']['EventDto']

export type LocalEchoStatus = 'pending' | 'failed'

type SendOptions = {
  replyTo?: string
  threadRoot?: string
  /** The local account's Matrix user id, for the local echo's sender. */
  senderId?: string
  /** Already-rendered Matrix HTML from the composer context. */
  formattedBody?: string | null
}

/**
 * Client-only extension of EventDto for a send this client made but the
 * server hasn't (yet, or ever) confirmed — modeled explicitly rather than
 * widened onto the generated schema, same pattern as `sync_state` in
 * `stores/accounts.ts`. Server-confirmed events never carry this field.
 */
export type TimelineEvent = EventDto & {
  localEcho?: {
    status: LocalEchoStatus
    body: string
    options: SendOptions
    /**
     * Present on a media send (ADR 0065). The `File` is retained so a failed
     * upload can be retried with the same bytes — losing the user's chosen
     * file to a network blip is the attachment-shaped version of losing their
     * typed text. `previewUrl` is an object url (images only) that **must** be
     * revoked when the echo leaves the slice; `dropEchoes` is the one place
     * that happens.
     */
    media?: { file: File; kind: UploadKind; previewUrl: string | null }
    /**
     * Display-only position within a multi-image send (ADR 0081). `retrySend`
     * drops it: a lone retry claiming "2 of 5" would be a lie about a batch
     * that no longer exists.
     */
    batch?: { index: number; total: number }
  }
}

/** Page size for both the initial page and each scroll-back page. */
const PAGE_LIMIT = 50

/**
 * How many events a scroll-back keeps loaded before it starts dropping the
 * newest end. The timeline is not DOM-windowed — a row mounts per event and
 * lives until the room closes (issue #26) — so an unbounded scroll-back grows
 * the DOM, the memory, and above all the teardown cost that ADR 0071 measured
 * on the phone "back to room list" transition. Twelve pages is well past a
 * screenful on any device, so the cap is invisible in normal reading.
 */
const RETAINED_EVENT_LIMIT = PAGE_LIMIT * 12

export interface TimelineStore {
  /** Loaded events, oldest first (display order). */
  events: ReadonlySignal<TimelineEvent[]>
  /** True until the first page settles. */
  loading: ReadonlySignal<boolean>
  /** True while an older page is being fetched. */
  loadingOlder: ReadonlySignal<boolean>
  /** True while a newer page is being fetched (after a jump). */
  loadingNewer: ReadonlySignal<boolean>
  /** True when the room start has been reached (no older page). */
  atStart: ReadonlySignal<boolean>
  /**
   * True when the slice ends at the room's live tail. `jumpTo` clears it —
   * the loaded page ends somewhere in history — and `loadNewer` catching up
   * (or a head re-read) restores it. While false, live frames do not append
   * (splicing "now" directly after "then" would fake contiguity) and the view
   * offers "Load newer messages" instead.
   */
  atEnd: ReadonlySignal<boolean>
  error: Signal<string | null>
  /**
   * Resolved reply-context events (ADR 0033 display), keyed by event id.
   * Populated for every `m.in_reply_to` target after each page load — from
   * the loaded slice when possible, otherwise fetched by id.
   */
  replyTargets: ReadonlySignal<ReadonlyMap<string, EventDto>>

  /** Load the newest page, replacing any loaded slice. */
  loadLatest(): Promise<void>
  /** Prepend the next older page (infinite scroll-back). */
  /**
   * Resolves to whether the page *landed* — the cursor moved on. `false` means
   * nothing was fetched at all: no cursor left, a sibling load already in
   * flight, a failed request, or a page dropped as stale. That is the caller's
   * signal to stop paging.
   *
   * A page that lands but contributes nothing the view renders still returns
   * `true`. Runs of state events, redactions, or thread replies are ordinary
   * history, and treating them as failure is what strands an automatic
   * scroll-back on the message before them.
   */
  loadOlder(): Promise<boolean>
  /**
   * Append the next newer run after a jump (forward scroll toward the
   * present). The read API pages backwards only, so this probes `at_ts`
   * above the loaded tail until a page overlaps it — the TUI's
   * `load_newer_history` ported (clients/tui/src/app/rooms.rs). A probe that
   * finds nothing newer flips `atEnd`.
   */
  loadNewer(): Promise<void>
  /**
   * Date jump (`at_ts`): replace the slice with the page at or before the
   * timestamp. Scroll-back continues from there; `loadNewer` walks forward.
   */
  jumpTo(atTs: number): Promise<void>
  /**
   * Calendar-day jump: find the page closest to the start of the selected day,
   * starting from that day's end and walking older pages only as far as needed.
   */
  jumpToDate(
    startTs: number,
    endTs: number,
    anchor?: (event: EventDto) => boolean,
  ): Promise<void>

  // ---- mutations (ADR 0046, M-W7). Each returns success; failures land in
  // `error`. Sends render a local echo synchronously, then reconcile it —
  // re-fetching the confirmed event and patching it in place on success, or
  // marking the echo `failed` (retryable/discardable) on error — the same
  // "refetch and patch in place" shape edit/redact/react already use.

  /** Send a message; `replyTo`/`threadRoot` set the ADR 0032 relations. */
  send(body: string, options?: SendOptions): Promise<boolean>
  /**
   * Stage a file's bytes and send them into the room (ADR 0065): upload, then
   * `send-media` claiming the staged id. The composer's text, if any, is the
   * `caption`; without one the server uses the filename as the event body.
   * Relations compose exactly as they do for a text send.
   */
  /** Send several files as one gesture, sequentially (ADR 0081). */
  sendMediaBatch(
    files: readonly File[],
    options?: {
      caption?: string
      replyTo?: string
      threadRoot?: string
      senderId?: string
    },
  ): Promise<boolean>
  sendMedia(
    file: File,
    options?: {
      caption?: string
      replyTo?: string
      threadRoot?: string
      senderId?: string
    },
  ): Promise<boolean>
  /** Retry a failed local echo using its retained body/options (and file). */
  retrySend(localId: string): Promise<boolean>
  /** Discard a failed local echo without retrying. */
  discardSend(localId: string): void
  /** Replace an event's text (`m.replace`). */
  edit(
    eventId: string,
    body: string,
    options?: { formattedBody?: string | null },
  ): Promise<boolean>
  /** Redact an event. */
  redact(eventId: string): Promise<boolean>
  /**
   * Toggle my reaction `key` on an event: reacts, or redacts my existing
   * reaction event (`my_event_ids`) when already reacted.
   */
  toggleReaction(event: EventDto, key: string): Promise<boolean>

  /**
   * Merge a live event pushed over `/v1/ws` into the loaded slice (M-W6): an
   * unseen event is appended (live frames arrive newest-last); a known id — a
   * reconnect refetch, a live edit/redact, or this client's own send echoing
   * back — replaces in place. A pending local echo the event confirms is
   * dropped so a send never shows twice. Events for another room/account (or
   * thread, when this is a thread timeline) are ignored.
   */
  ingestLive(event: EventDto): void
}

/** The `m.in_reply_to` target id, if this event is a reply. */
export function inReplyToId(event: EventDto): string | null {
  const relates = event.relates_to as {
    rel_type?: unknown
    is_falling_back?: unknown
    'm.in_reply_to'?: { event_id?: unknown }
  } | null
  const reply = relates?.['m.in_reply_to']
  if (relates?.rel_type === 'm.thread' && relates.is_falling_back === true) {
    return null
  }
  const id = reply?.event_id
  return typeof id === 'string' ? id : null
}

/**
 * The top-level Matrix relation target, if this raw live event is a relation
 * that the collapsed timeline folds into another row.
 */
function collapsedRelationTargetId(event: EventDto): string | null {
  const relates = event.relates_to as {
    rel_type?: unknown
    event_id?: unknown
  } | null
  if (
    (relates?.rel_type === 'm.annotation' && event.type === 'm.reaction') ||
    relates?.rel_type === 'm.replace'
  ) {
    return typeof relates.event_id === 'string' ? relates.event_id : null
  }
  return null
}

/** A room event that should create a room-level unread indicator. */
export function isRoomUnreadEvent(event: EventDto): boolean {
  const relates = event.relates_to as { rel_type?: unknown } | null
  return (
    event.type === 'm.room.message' &&
    event.state_key == null &&
    !event.redacted &&
    relates?.rel_type !== 'm.replace' &&
    typeof event.body === 'string' &&
    event.body.trim() !== ''
  )
}

/** The event id a raw redaction event redacts, when the live payload exposes it. */
function redactsEventId(event: EventDto): string | null {
  if (event.type !== 'm.room.redaction') {
    return null
  }
  const content = event.content as { redacts?: unknown } | null
  return typeof content?.redacts === 'string' ? content.redacts : null
}

/**
 * One room's timeline (ADR 0046, M-W5 read + M-W7 write) over the
 * cursor-paginated read API (ADRs 0019/0020): the server pages newest-first;
 * the store keeps events in ascending display order and prepends older
 * pages. Edits and reactions arrive already collapsed onto their target rows
 * (ADR 0033) — no client-side aggregation happens here.
 *
 * With `threadRoot` set, the store reads the thread-scoped timeline
 * (`…/threads/{root}/timeline`) and its sends target the thread (M-W7).
 */
export function createTimelineStore(
  api: ApiClient,
  media: MediaService,
  accountId: string,
  roomId: string,
  threadRoot?: string,
): TimelineStore {
  const events = signal<TimelineEvent[]>([])
  const loading = signal(true)
  const loadingOlder = signal(false)
  const loadingNewer = signal(false)
  const nextCursor = signal<string | null>(null)
  const reachedStart = signal(false)
  const reachedEnd = signal(true)
  const error = signal<string | null>(null)
  const replyTargets = signal<ReadonlyMap<string, EventDto>>(new Map())
  /** Reply-target ids already resolved or in flight (misses included). */
  const requestedTargets = new Set<string>()
  /**
   * Bumped by every slice replacement (`loadLatest`/`jumpTo`). A paged or
   * replacing request captures the value when it starts and discards its
   * result if it changed — otherwise a `loadOlder` in flight when a reconnect
   * gap-fill replaces the slice would prepend a page fetched against the old
   * cursor onto the new slice, leaving a silent gap and a stale cursor; and
   * of two racing replacements the *older* response could win (WCR-03).
   */
  let sliceGeneration = 0
  const collapsedRelationTargets = new Map<string, string>()

  /**
   * Retire a media echo's local preview (ADR 0065). An echo that leaves the
   * slice — discarded, retried, or replaced by the confirmed event — takes its
   * object url with it; nothing points at the local blob afterwards, and an
   * echo dropped without this leaks it.
   */
  function revokePreview(event: TimelineEvent): void {
    const previewUrl = event.localEcho?.media?.previewUrl
    if (previewUrl !== undefined && previewUrl !== null) {
      URL.revokeObjectURL(previewUrl)
    }
  }

  /** Remove events from a slice, revoking the preview of any echo among them —
   *  the one path by which an echo may leave `events`. */
  function dropEchoes(
    list: TimelineEvent[],
    doomed: (event: TimelineEvent, index: number) => boolean,
  ): TimelineEvent[] {
    return list.filter((event, index) => {
      if (!doomed(event, index)) {
        return true
      }
      revokePreview(event)
      return false
    })
  }

  /**
   * Bound a scrolled-back slice by dropping its newest end, and report whether
   * anything went. Only the newest end can go: `nextCursor` is opaque and
   * describes the page *below* the current oldest event, so trimming the head
   * would leave the cursor pointing past the events it dropped and the next
   * scroll-back would splice a silent gap into history (the WCR-03 hazard).
   *
   * Local echoes are never dropped — they are the user's unsent work, and the
   * cut stops short of the oldest one rather than taking it.
   */
  function trimRetainedTail(slice: TimelineEvent[]): TimelineEvent[] {
    if (slice.length <= RETAINED_EVENT_LIMIT) {
      return slice
    }
    // The same history/echo split `loadNewer` and `refreshHead` use: echoes
    // are not history, they ride at the tail wherever the history ends.
    const history = slice.filter((event) => event.localEcho === undefined)
    if (history.length <= RETAINED_EVENT_LIMIT) {
      return slice
    }
    const echoes = slice.filter((event) => event.localEcho !== undefined)
    return [...history.slice(0, RETAINED_EVENT_LIMIT), ...echoes]
  }

  async function fetchPage(query: {
    cursor?: string
    at_ts?: number
  }): Promise<{ events: EventDto[]; next: string | null } | null> {
    try {
      const pageQuery = { limit: PAGE_LIMIT, ...query }
      const { data, error: apiError } =
        threadRoot === undefined
          ? await api.GET(
              '/v1/accounts/{account_id}/rooms/{room_id}/timeline',
              {
                params: {
                  path: { account_id: accountId, room_id: roomId },
                  query: pageQuery,
                },
              },
            )
          : await api.GET(
              '/v1/accounts/{account_id}/rooms/{room_id}/threads/{root_id}/timeline',
              {
                params: {
                  path: {
                    account_id: accountId,
                    room_id: roomId,
                    root_id: threadRoot,
                  },
                  query: pageQuery,
                },
              },
            )
      if (apiError !== undefined) {
        error.value = apiErrorMessage(apiError)
        return null
      }
      error.value = null
      return {
        // Server order is newest-first; display order is oldest-first.
        events: [...data.data.events].reverse(),
        next: data.data.next_cursor ?? null,
      }
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : String(cause)
      return null
    }
  }

  function resolveReplyTargets(slice: EventDto[]): void {
    const wanted = new Set<string>()
    for (const event of slice) {
      const target = inReplyToId(event)
      if (target !== null && !requestedTargets.has(target)) {
        wanted.add(target)
      }
    }
    if (wanted.size === 0) {
      return
    }
    // Resolve from the loaded slice first — the common case for replies to
    // nearby messages — then fetch the rest by id.
    const next = new Map(replyTargets.value)
    for (const event of events.value) {
      if (wanted.has(event.event_id)) {
        next.set(event.event_id, event)
        requestedTargets.add(event.event_id)
        wanted.delete(event.event_id)
      }
    }
    replyTargets.value = next
    for (const id of wanted) {
      requestedTargets.add(id)
      inBackground(
        api
          .GET('/v1/accounts/{account_id}/events/{event_id}', {
            params: { path: { account_id: accountId, event_id: id } },
          })
          .then(({ data }) => {
            if (data !== undefined) {
              replyTargets.value = new Map(replyTargets.value).set(
                id,
                data.data,
              )
            }
            // A miss (e.g. target outside stored history) renders as a stub;
            // `requestedTargets` prevents refetch loops either way.
          }),
      )
    }
  }

  async function replaceSlice(query: { at_ts?: number }): Promise<void> {
    // `loading` swaps the whole timeline for a placeholder, which is right
    // for both callers — the first load has nothing to show and a jump is
    // *leaving* what is shown (the remount also re-anchors the view on the
    // new page's tail, the jump target). The reconnect gap-fill deliberately
    // does not come through here: raising `loading` there blanked the
    // timeline on every reconnect (see `refreshHead`).
    loading.value = true
    const generation = ++sliceGeneration
    const page = await fetchPage(query)
    if (page !== null && generation === sliceGeneration) {
      events.value = page.events
      nextCursor.value = page.next
      reachedStart.value = page.next === null
      // Only a *settled* head load may declare the slice live — flipping the
      // flag before the fetch resolves let a failed load leave `atEnd` true
      // while the slice was still parked in history, and live frames then
      // spliced "now" onto "then". A jump's page ends at `at_ts`, in history.
      reachedEnd.value = query.at_ts === undefined
      resolveReplyTargets(page.events)
    }
    // A superseding replacement owns the flag now; racing it here would
    // unveil the stale slice while the newer page is still in flight.
    if (generation === sliceGeneration) {
      loading.value = false
    }
  }

  async function replaceSliceForDate(
    startTs: number,
    endTs: number,
    anchor: ((event: EventDto) => boolean) | undefined,
  ): Promise<void> {
    loading.value = true
    const generation = ++sliceGeneration
    let page = await fetchPage({ at_ts: endTs })
    let anchors = page === null ? [] : dateJumpAnchors(page.events, anchor)
    while (
      page !== null &&
      generation === sliceGeneration &&
      page.next !== null &&
      page.events.length > 0 &&
      (anchors.length === 0 || anchors[0].origin_ts > startTs)
    ) {
      page = await fetchPage({ cursor: page.next })
      anchors = page === null ? [] : dateJumpAnchors(page.events, anchor)
    }
    if (page !== null && generation === sliceGeneration) {
      events.value = page.events
      nextCursor.value = page.next
      reachedStart.value = page.next === null
      reachedEnd.value = false
      resolveReplyTargets(page.events)
    }
    if (generation === sliceGeneration) {
      loading.value = false
    }
  }

  function dateJumpAnchors(
    slice: readonly EventDto[],
    anchor: ((event: EventDto) => boolean) | undefined,
  ): readonly EventDto[] {
    if (anchor !== undefined) {
      return slice.filter(anchor)
    }
    return threadRoot === undefined
      ? slice.filter((event) => liveThreadRoot(event) === null)
      : slice
  }

  /**
   * The newest page, folded into an already-loaded slice (ADR 0061 gap-fill,
   * WCR-05). Replacing the slice wholesale teleported a scrolled-back user to
   * the newest page on every reconnect; instead, when the fresh head overlaps
   * what is loaded, the head's rows win by id (picking up edits/redactions
   * missed while offline), older history and the existing cursor chain are
   * kept, and pending local echoes stay at the end (dropping any the head
   * confirms, same rule as `ingestLive`). Only a head that shares nothing
   * with the loaded slice replaces it — the gap is real, the old cursor
   * chain no longer describes what would sit below the head.
   */
  async function refreshHead(): Promise<void> {
    const generation = sliceGeneration
    const page = await fetchPage({})
    if (page === null || generation !== sliceGeneration) {
      return
    }
    const loaded = events.value
    const headIds = new Set(page.events.map((e) => e.event_id))
    const overlaps = loaded.some((e) => headIds.has(e.event_id))
    if (!overlaps && !reachedEnd.value) {
      // A slice deliberately parked in history — a date jump, or a scroll-back
      // that trimmed its tail — shares nothing with the head *by design*, and
      // the cursor chain below it is still sound. Replacing it here would
      // teleport a reader to the newest page on every reconnect, which is the
      // very thing WCR-05 set out to stop; `loadNewer` is what walks forward.
      return
    }
    if (!overlaps) {
      sliceGeneration += 1
      events.value = page.events
      nextCursor.value = page.next
      reachedStart.value = page.next === null
      // The slice *is* the head now, wherever it was parked before.
      reachedEnd.value = true
      resolveReplyTargets(page.events)
      return
    }
    // Overlap: merge. No generation bump — an in-flight `loadOlder` prepend
    // still applies, since the old history and cursor survive.
    let rest = loaded.filter((e) => !headIds.has(e.event_id))
    for (const fresh of page.events) {
      const echoIndex = rest.findIndex(
        (e) =>
          e.localEcho?.status === 'pending' &&
          e.sender === fresh.sender &&
          e.localEcho.body === (fresh.body ?? ''),
      )
      if (echoIndex !== -1) {
        rest = dropEchoes(rest, (_, i) => i === echoIndex)
      }
    }
    const history = rest.filter((e) => e.localEcho === undefined)
    const echoes = rest.filter((e) => e.localEcho !== undefined)
    events.value = [...history, ...page.events, ...echoes]
    // The merged slice provably ends at the head just fetched.
    reachedEnd.value = true
    resolveReplyTargets(page.events)
  }

  /**
   * A page that *straddles* the loaded tail, found by probing `at_ts` above
   * it. The read API has no forward cursor — `at_ts` returns the page at or
   * before a timestamp — so forward progress means guessing a timestamp far
   * enough ahead to contain new events but near enough that the returned
   * page still overlaps what is loaded; overlap is what proves the appended
   * run is contiguous. The window doubles over quiet stretches (an empty
   * probe), and — one honesty improvement over the TUI original — *halves*
   * when a full page floats entirely above the tail, instead of accepting a
   * page that may hide a gap beneath itself.
   */
  /**
   * Returning `null` means *inconclusive* (a fetch failed or the probe
   * budget ran out with no forward progress): the caller must leave `atEnd`
   * alone and let a later attempt retry. Only a definitive result — some
   * events, or a probe at the present that found nothing — comes back as an
   * array, because an empty array is read as "caught up".
   */
  async function fetchStraddlingRun(pivot: number): Promise<EventDto[] | null> {
    const DAY_MS = 86_400_000
    let window = DAY_MS
    /** Best overlapping page so far (ascending), and its events above pivot. */
    let chosen: EventDto[] | null = null
    let chosenAfter = 0
    /**
     * The range `(pivot, knownUpTo]` is fully covered by `chosen`: a page is
     * the newest events at-or-before its `at_ts`, so one that reaches down to
     * the pivot has shown *everything* up to where it was aimed.
     */
    let knownUpTo = pivot
    for (let probe = 0; probe < 32; probe += 1) {
      const now = Date.now()
      const atTs = Math.min(pivot + window, now)
      const page = await fetchPage({ at_ts: atTs })
      if (page === null) {
        return null
      }
      const slice = page.events // ascending, per fetchPage
      if (slice.length === 0) {
        if (atTs >= now) {
          return chosen ?? []
        }
        window *= 2
        continue
      }
      if (slice[0].origin_ts <= pivot) {
        // Overlaps the tail: contiguous by construction.
        chosen = slice
        chosenAfter = slice.filter((e) => e.origin_ts > pivot).length
        knownUpTo = Math.max(knownUpTo, atTs)
        if (atTs >= now || chosenAfter >= PAGE_LIMIT / 2) {
          return chosen
        }
        window *= 2
        continue
      }
      // The page floats above the tail, which may hide a gap beneath it —
      // *unless* its bottom meets the range an overlap page already proved
      // covered. That union is how a burst wider than one page stays
      // reachable: no single window both overlaps the tail and bites into
      // the burst, so neither page alone can ever be accepted.
      const oldest = slice[0].origin_ts
      if (oldest <= knownUpTo + 1) {
        return [...(chosen ?? []), ...slice].sort(
          (a, b) => a.origin_ts - b.origin_ts,
        )
      }
      if (chosenAfter > 0) {
        // Contiguous progress in hand beats aiming for more (the TUI's rule,
        // and the loop's termination guarantee).
        return chosen
      }
      if (window <= 1) {
        // Cannot aim closer: >= PAGE_LIMIT events share the millisecond
        // above the tail. Take the page; the alternative is never advancing.
        return slice
      }
      // Aim at the floating page's own bottom — a blind halving can hop over
      // it forever when a quiet stretch sits under the burst.
      window = Math.max(1, Math.min(Math.floor(window / 2), oldest - pivot))
    }
    return chosenAfter > 0 ? chosen : null
  }

  async function loadNewer(): Promise<void> {
    if (reachedEnd.value || loadingNewer.value) {
      return
    }
    // Local echoes sit at the tail but are not history; the pivot is the
    // newest *confirmed* event.
    const pivotEvent = events.value.findLast((e) => e.localEcho === undefined)
    if (pivotEvent === undefined) {
      return
    }
    const pivot = pivotEvent.origin_ts
    loadingNewer.value = true
    const generation = sliceGeneration
    try {
      const run = await fetchStraddlingRun(pivot)
      if (generation !== sliceGeneration) {
        return
      }
      if (run === null) {
        return // fetch error; `error` is already set, the button stays
      }
      const current = events.value
      const have = new Set(current.map((e) => e.event_id))
      // `>=`, not `>`: origin_ts ties at the pivot millisecond are routine in
      // a burst, and a strict bound would read an unseen tied event as
      // "nothing newer" — flipping atEnd with messages still unloaded. The id
      // set is what actually separates new from already-loaded; adding as we
      // go also drops the duplicate a stitched run carries at its seam.
      const fresh = run.filter((e) => {
        if (e.origin_ts < pivot || have.has(e.event_id)) {
          return false
        }
        have.add(e.event_id)
        return true
      })
      if (fresh.length === 0) {
        // Nothing above the tail: caught up. Live appends resume.
        reachedEnd.value = true
        return
      }
      const history = current.filter((e) => e.localEcho === undefined)
      const echoes = current.filter((e) => e.localEcho !== undefined)
      events.value = [...history, ...fresh, ...echoes]
      resolveReplyTargets(fresh)
    } finally {
      // Unconditional: the `loadingNewer` guard means no sibling is in
      // flight, and a slice replacement that superseded this run must not
      // leave the flag latched.
      loadingNewer.value = false
    }
  }

  /**
   * Re-fetch one event and replace it in the loaded slice — how edits,
   * redactions, and reaction toggles become visible without resetting the
   * user's scroll-back position.
   */
  async function refreshEvent(eventId: string): Promise<void> {
    // Best-effort by contract: the mutation already succeeded, so a refetch
    // that fails (miss *or* network drop) leaves the stale row for the next
    // live frame or page read to heal — it must never reject the mutation's
    // own promise (WCR-02).
    let data
    try {
      ;({ data } = await api.GET(
        '/v1/accounts/{account_id}/events/{event_id}',
        { params: { path: { account_id: accountId, event_id: eventId } } },
      ))
    } catch {
      return
    }
    if (data === undefined) {
      return
    }
    const fresh = data.data
    events.value = events.value.map((event) =>
      event.event_id === eventId ? fresh : event,
    )
  }

  /** This event's thread root, or `null` — mirrors `threadRootId` locally to
   *  keep the timeline module below the threads module in the import graph. */
  function liveThreadRoot(event: EventDto): string | null {
    const relates = event.relates_to as {
      rel_type?: unknown
      event_id?: unknown
    } | null
    return relates?.rel_type === 'm.thread' &&
      typeof relates.event_id === 'string'
      ? relates.event_id
      : null
  }

  function ingestLive(event: EventDto): void {
    if (event.account_id !== accountId || event.room_id !== roomId) {
      return
    }
    const relationTargetId = collapsedRelationTargetId(event)
    if (relationTargetId !== null) {
      collapsedRelationTargets.set(event.event_id, relationTargetId)
      const targetLoaded = events.value.some(
        (candidate) => candidate.event_id === relationTargetId,
      )
      if (targetLoaded) {
        inBackground(refreshEvent(relationTargetId))
      }
      return
    }
    const redactedEventId = redactsEventId(event)
    if (redactedEventId !== null) {
      const relationTarget =
        collapsedRelationTargets.get(redactedEventId) ??
        events.value.find((candidate) =>
          Object.values(candidate.reactions ?? {}).some((reaction) =>
            reaction.my_event_ids?.includes(redactedEventId),
          ),
        )?.event_id
      const targetId = relationTarget ?? redactedEventId
      const targetLoaded = events.value.some(
        (candidate) => candidate.event_id === targetId,
      )
      if (targetLoaded) {
        inBackground(refreshEvent(targetId))
      }
      return
    }
    // A thread timeline only takes its own thread's replies; the main timeline
    // takes everything for the room (it hides thread members at the view, as it
    // already does for the paged read).
    if (threadRoot !== undefined && liveThreadRoot(event) !== threadRoot) {
      return
    }
    const list = events.value
    const idx = list.findIndex((e) => e.event_id === event.event_id)
    if (idx !== -1) {
      // Known id: replace in place (live edit/redact, or own send reconciled).
      events.value = list.map((e, i) => (i === idx ? event : e))
      return
    }
    if (!reachedEnd.value) {
      // Jumped into history: the slice does not end at the room's tail, so
      // appending a live event would splice the present onto the past. It is
      // not lost — `loadNewer` (or the reconnect head re-read) walks forward
      // to it. Known-id replacements above still apply to loaded rows.
      return
    }
    // New event: drop a pending local echo it confirms (own-send live race,
    // where the broadcast beats this client's own reconcile), then append.
    // Only the *first* match: one frame confirms one send, and two quick
    // sends of the same body must keep their second echo for its own frame
    // (or reconcile) to confirm — the bus is lossy (WCR-07).
    const confirmsEcho = (e: TimelineEvent) =>
      e.localEcho?.status === 'pending' &&
      e.sender === event.sender &&
      e.localEcho.body === (event.body ?? '')
    const echoIndex = list.findIndex(confirmsEcho)
    if (echoIndex !== -1) {
      // Replace the echo *where it stands*, rather than dropping it and
      // appending. Appending is right for a genuinely new event, but this one
      // already had a place: the echo the user is watching. Moving it to the
      // tail reorders a multi-image send — a frame confirming image 1 while
      // 2..N are still pending renders the batch as 2, 3, …, 1, permanently,
      // because the server assigns room order and Matrix carries no album
      // marker (ADR 0081, #130).
      revokePreview(list[echoIndex])
      events.value = list.map((existing, i) =>
        i === echoIndex ? event : existing,
      )
    } else {
      events.value = [...list, event]
    }
    resolveReplyTargets([event])
  }

  /** Shared mutation wrapper: run, surface the envelope on failure. */
  async function mutate(
    call: () => Promise<{ error?: unknown }>,
  ): Promise<boolean> {
    try {
      const { error: apiError } = await call()
      if (apiError !== undefined) {
        error.value = apiErrorMessage(apiError)
        return false
      }
      error.value = null
      return true
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : String(cause)
      return false
    }
  }

  /** Like `mutate`, but also returns the success payload's `event_id`. */
  async function mutateWithResult(
    call: () => Promise<{
      data?: { data: { event_id: string } }
      error?: unknown
    }>,
  ): Promise<{ ok: true; eventId: string } | { ok: false }> {
    try {
      const { data, error: apiError } = await call()
      if (apiError !== undefined || data === undefined) {
        error.value = apiErrorMessage(apiError)
        return { ok: false }
      }
      error.value = null
      return { ok: true, eventId: data.data.event_id }
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : String(cause)
      return { ok: false }
    }
  }

  /** `body` + the markdown-derived formatted pair, when formatting exists. */
  function formattedParts(
    body: string,
    formattedBody?: string | null,
  ): {
    body: string
    format?: 'org.matrix.custom.html'
    formatted_body?: string
  } {
    if (formattedBody !== undefined) {
      return formattedBody === null
        ? { body }
        : {
            body,
            format: 'org.matrix.custom.html',
            formatted_body: formattedBody,
          }
    }
    const html = markdownToHtmlIfFormatted(body)
    return html === null
      ? { body }
      : { body, format: 'org.matrix.custom.html', formatted_body: html }
  }

  /** The `m.relates_to` shape a local echo needs for display helpers
   *  (`inReplyToId`, `threadRootId`) to treat it like a real event. */
  function localRelatesTo(
    replyTo: string | undefined,
    targetThread: string | undefined,
  ): Record<string, unknown> | null {
    if (targetThread === undefined && replyTo === undefined) {
      return null
    }
    return {
      ...(targetThread !== undefined && {
        rel_type: 'm.thread',
        event_id: targetThread,
      }),
      ...(replyTo !== undefined && { 'm.in_reply_to': { event_id: replyTo } }),
    }
  }

  function buildLocalEcho(
    localId: string,
    body: string,
    options: SendOptions,
    senderId: string | undefined,
  ): TimelineEvent {
    const targetThread = options.threadRoot ?? threadRoot
    return {
      account_id: accountId,
      event_id: localId,
      room_id: roomId,
      sender: senderId ?? '',
      origin_ts: Date.now(),
      type: 'm.room.message',
      body,
      content: {
        msgtype: 'm.text',
        ...formattedParts(body, options.formattedBody),
      },
      relates_to: localRelatesTo(options.replyTo, targetThread),
      redacted: false,
      edited: false,
      edit_count: 0,
      state_key: null,
      reactions: null,
      localEcho: { status: 'pending', body, options },
    } as unknown as TimelineEvent
  }

  /** Mark a pending echo failed. Its body/options — and, for media, its `File`
   *  — are retained, so Retry has everything it needs. */
  function failEcho(localId: string): void {
    events.value = events.value.map((event) =>
      event.event_id === localId
        ? {
            ...event,
            localEcho: { ...event.localEcho!, status: 'failed' as const },
          }
        : event,
    )
  }

  /**
   * A send landed: fetch the confirmed event and patch it over its echo, same
   * as `refreshEvent` (a fetch miss — or a connection dropping right after the
   * send landed — silently no-ops, leaving the echo pending for the live frame
   * or the next page read to confirm).
   */
  async function reconcileEcho(
    localId: string,
    eventId: string,
  ): Promise<void> {
    let data
    try {
      ;({ data } = await api.GET(
        '/v1/accounts/{account_id}/events/{event_id}',
        { params: { path: { account_id: accountId, event_id: eventId } } },
      ))
    } catch {
      return
    }
    if (data === undefined) {
      return
    }
    // If the live frame already appended the confirmed event (its `confirmsEcho`
    // match can miss — e.g. the echo's sender was '' because rooms hadn't loaded
    // when the send was made), replacing the echo would put the same event_id in
    // the slice twice. Drop the echo instead (WCR-07).
    const confirmed = data.data
    const alreadyAppended = events.value.some(
      (event) => event.event_id === confirmed.event_id,
    )
    if (alreadyAppended) {
      events.value = dropEchoes(
        events.value,
        (event) => event.event_id === localId,
      )
      return
    }
    events.value = events.value.map((event) => {
      if (event.event_id !== localId) {
        return event
      }
      // The confirmed event renders from the media proxy, so the echo's local
      // blob is retired as it is replaced.
      revokePreview(event)
      return confirmed
    })
  }

  async function send(
    body: string,
    options: SendOptions = {},
  ): Promise<boolean> {
    const targetThread = options.threadRoot ?? threadRoot
    const localId = `local:${crypto.randomUUID()}`
    events.value = [
      ...events.value,
      buildLocalEcho(localId, body, options, options.senderId),
    ]

    const result = await mutateWithResult(() =>
      api.POST('/v1/accounts/{account_id}/rooms/{room_id}/send', {
        params: { path: { account_id: accountId, room_id: roomId } },
        body: {
          ...formattedParts(body, options.formattedBody),
          reply_to: options.replyTo ?? null,
          thread_root: targetThread ?? null,
        },
      }),
    )

    if (!result.ok) {
      failEcho(localId)
      return false
    }
    await reconcileEcho(localId, result.eventId)
    return true
  }

  type MediaSendOptions = {
    caption?: string
    replyTo?: string
    threadRoot?: string
    senderId?: string
  }

  /**
   * Push the local echo for a media send and return its id, without starting
   * the upload.
   *
   * Split out so a batch can push *every* echo synchronously before any bytes
   * move: the gallery then appears complete the instant the user hits send,
   * rather than growing a row at a time as uploads finish (ADR 0081).
   */
  function pushMediaEcho(
    file: File,
    options: MediaSendOptions,
    batch?: { index: number; total: number },
  ): string {
    const kind = uploadKind(file)
    // The server sets the event body to the caption, or to the filename when
    // there is none — so an echo built with the same rule is what the confirming
    // live frame's `confirmsEcho` body match recognizes, for free.
    const body = options.caption ?? file.name
    const localId = `local:${crypto.randomUUID()}`
    const previewUrl = kind === 'image' ? URL.createObjectURL(file) : null

    const echo = buildLocalEcho(localId, body, options, options.senderId)
    events.value = [
      ...events.value,
      // Cast for the same reason `buildLocalEcho` does: the generated schema
      // types `content` as an opaque `Record<string, never>`.
      {
        ...echo,
        content: {
          msgtype: kind === 'image' ? 'm.image' : 'm.file',
          body,
          filename: file.name,
          // No `url`: no mxc exists until the upload lands. `EventBody` renders
          // this echo from `localEcho.media`, not through `parseMedia` (ADR
          // 0065) — an image with a non-mxc url is deliberately *not* media.
          info: { mimetype: file.type, size: file.size },
        },
        localEcho: {
          ...echo.localEcho!,
          media: { file, kind, previewUrl },
          ...(batch === undefined ? {} : { batch }),
        },
      } as unknown as TimelineEvent,
    ]
    return localId
  }

  /** Upload the bytes for an echo already in the slice, and send the event. */
  async function completeMediaSend(
    localId: string,
    file: File,
    options: MediaSendOptions,
  ): Promise<boolean> {
    const targetThread = options.threadRoot ?? threadRoot
    const staged = await media.upload(accountId, file)
    if (!staged.ok) {
      error.value = uploadFailureMessage(staged.error)
      failEcho(localId)
      return false
    }

    const result = await mutateWithResult(() =>
      api.POST('/v1/accounts/{account_id}/rooms/{room_id}/send-media', {
        params: { path: { account_id: accountId, room_id: roomId } },
        body: {
          upload_id: staged.uploadId,
          caption: options.caption ?? null,
          reply_to: options.replyTo ?? null,
          thread_root: targetThread ?? null,
        },
      }),
    )

    if (!result.ok) {
      failEcho(localId)
      return false
    }
    await reconcileEcho(localId, result.eventId)
    return true
  }

  async function sendMedia(
    file: File,
    options: MediaSendOptions = {},
  ): Promise<boolean> {
    return sendMediaBatch([file], options)
  }

  /**
   * Send several files as one gesture (ADR 0081).
   *
   * Every echo is pushed first and synchronously, so the run appears as one
   * gallery immediately. The uploads then run **sequentially** — not a
   * simplification but the only thing that preserves order, because the server
   * assigns it at send time and Matrix carries no album marker (#130).
   *
   * Each file fails independently: a failed echo keeps its retry affordance
   * and, being failed, drops out of its gallery into a full row where that
   * affordance is visible.
   */
  async function sendMediaBatch(
    files: readonly File[],
    options: MediaSendOptions = {},
  ): Promise<boolean> {
    if (files.length === 0) {
      return true
    }
    const jobs = files.map((file, index) => ({
      file,
      // Caption and `replyTo` belong to the first image only; `threadRoot`
      // applies to all, because thread membership is a destination rather
      // than a per-event decoration (ADR 0081).
      options: {
        ...options,
        caption: index === 0 ? options.caption : undefined,
        replyTo: index === 0 ? options.replyTo : undefined,
      },
      localId: '',
    }))
    for (const [index, job] of jobs.entries()) {
      job.localId = pushMediaEcho(
        job.file,
        job.options,
        files.length > 1 ? { index, total: files.length } : undefined,
      )
    }

    let failures = 0
    for (const job of jobs) {
      const ok = await completeMediaSend(job.localId, job.file, job.options)
      if (!ok) {
        failures += 1
      }
    }
    if (failures > 0 && files.length > 1) {
      // One error slot, so say how much of the batch it covers.
      error.value = `${failures} of ${files.length} files failed to send`
    }
    return failures === 0
  }

  return {
    events: computed(() => events.value),
    loading: computed(() => loading.value),
    loadingOlder: computed(() => loadingOlder.value),
    loadingNewer: computed(() => loadingNewer.value),
    atStart: computed(() => reachedStart.value),
    atEnd: computed(() => reachedEnd.value),
    error,
    replyTargets: computed(() => replyTargets.value),

    // First load replaces (nothing to merge); a reload over an existing
    // slice — the reconnect gap-fill — merges instead (WCR-05).
    // `atEnd` flips inside the load's success path, never eagerly — a failed
    // head fetch must leave a jumped-back slice marked as history.
    loadLatest: () =>
      events.value.length === 0 ? replaceSlice({}) : refreshHead(),
    jumpTo: (atTs: number) => {
      // The page at `atTs` ends somewhere in history. Pessimistic on purpose:
      // a jump near the present costs one `loadNewer` that comes back empty
      // and flips this back, which is cheaper than ever faking contiguity.
      reachedEnd.value = false
      return replaceSlice({ at_ts: atTs })
    },
    jumpToDate: (
      startTs: number,
      endTs: number,
      anchor?: (event: EventDto) => boolean,
    ) => {
      reachedEnd.value = false
      return replaceSliceForDate(startTs, endTs, anchor)
    },
    loadNewer,

    ingestLive,

    send,
    sendMedia,
    sendMediaBatch,

    async retrySend(localId) {
      const failed = events.value.find((event) => event.event_id === localId)
      if (failed?.localEcho === undefined) {
        return false
      }
      const { body, options, media: failedMedia } = failed.localEcho
      // Drops the old echo (and its preview url); the retry builds a fresh one.
      events.value = dropEchoes(
        events.value,
        (event) => event.event_id === localId,
      )
      const senderId = failed.sender
      return failedMedia === undefined
        ? send(body, { ...options, senderId })
        : // The retained `File` is re-uploaded — a failed send never costs the
          // user the file they picked.
          sendMedia(failedMedia.file, {
            ...options,
            // Invert the echo's `caption ?? filename` body rule. A caption that
            // *was* the filename round-trips as "no caption", which the server
            // renders identically — so the outcome is the same either way.
            caption: body === failedMedia.file.name ? undefined : body,
            senderId,
          })
    },

    discardSend(localId) {
      events.value = dropEchoes(
        events.value,
        (event) => event.event_id === localId,
      )
    },

    async edit(eventId, body, options = {}) {
      const ok = await mutate(() =>
        api.PUT('/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}', {
          params: {
            path: {
              account_id: accountId,
              room_id: roomId,
              event_id: eventId,
            },
          },
          body: formattedParts(body, options.formattedBody),
        }),
      )
      if (ok) {
        await refreshEvent(eventId)
      }
      return ok
    },

    async redact(eventId) {
      const ok = await mutate(() =>
        api.DELETE(
          '/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}',
          {
            params: {
              path: {
                account_id: accountId,
                room_id: roomId,
                event_id: eventId,
              },
            },
          },
        ),
      )
      if (ok) {
        await refreshEvent(eventId)
      }
      return ok
    },

    async toggleReaction(event, key) {
      const mine = event.reactions?.[key]
      const myReactionId =
        mine?.me === true ? mine.my_event_ids?.[0] : undefined
      const ok = await mutate(() =>
        myReactionId !== undefined
          ? // Withdraw: redact my reaction event (the raw m.reaction rows are
            // collapsed out of the timeline, so this is the only handle).
            api.DELETE(
              '/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}',
              {
                params: {
                  path: {
                    account_id: accountId,
                    room_id: roomId,
                    event_id: myReactionId,
                  },
                },
              },
            )
          : api.POST(
              '/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}/reactions',
              {
                params: {
                  path: {
                    account_id: accountId,
                    room_id: roomId,
                    event_id: event.event_id,
                  },
                },
                body: { key },
              },
            ),
      )
      if (ok) {
        await refreshEvent(event.event_id)
      }
      return ok
    },

    async loadOlder() {
      const cursor = nextCursor.value
      if (cursor === null || loadingOlder.value) {
        return false
      }
      loadingOlder.value = true
      const generation = sliceGeneration
      try {
        const page = await fetchPage({ cursor })
        // A slice replacement while this page was in flight means the cursor
        // it was fetched against no longer describes the loaded slice —
        // prepending would splice unrelated history in. Drop it (WCR-03).
        if (page === null || generation !== sliceGeneration) {
          return false
        }
        const merged = [...page.events, ...events.value]
        const retained = trimRetainedTail(merged)
        if (retained.length !== merged.length) {
          // The slice no longer runs to the room's tail. That is exactly the
          // state a date jump creates, so the machinery for it already
          // exists: `ingestLive` stops appending the present onto the past,
          // the bottom sentinel appears, and `loadNewer` walks forward when
          // the reader comes back down.
          reachedEnd.value = false
        }
        events.value = retained
        nextCursor.value = page.next
        reachedStart.value = page.next === null
        resolveReplyTargets(page.events)
        // The cursor moved, whether or not the page carried anything this
        // view will draw.
        return true
      } finally {
        // `finally`, so a throw between here and the fetch cannot strand the
        // flag: a latched `loadingOlder` makes every later page a no-op.
        loadingOlder.value = false
      }
    },
  }
}
