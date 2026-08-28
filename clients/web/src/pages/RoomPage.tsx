import { Fragment, type JSX } from 'preact'
import { useLocation, useRoute } from 'preact-iso'
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'preact/hooks'
import { apiErrorMessage, inBackground } from '../api/client'
import { timelineEvent } from '../api/frames'
import { parseCalendarDay, sameLocalDay } from '../calendar-day'
import { DaySeparator } from '../components/DaySeparator'
import { Composer } from '../components/Composer'
import type { ComposerAutocompleteOption } from '../components/Composer'
import { ErrorBanner } from '../components/ErrorBanner'
import {
  canonicalReactionKey,
  isEditable,
  isReactable,
  isStateEvent,
  isUnsupportedBodylessEvent,
  MessageEventRow,
} from '../components/MessageEventRow'
import { RoomInfoPanel } from '../components/RoomInfoPanel'
import { ThreadPanel } from '../components/ThreadPanel'
import {
  literalMessage,
  rainbowMessage,
  rawHtmlMessage,
  spoilerMessage,
  type FormattedMessage,
} from '../html/send-format'
import {
  useMessageComposer,
  type ComposerAction,
} from '../components/use-message-composer'
import {
  localRoomHref,
  localThreadEventHref,
  parseMatrixRoomReference,
  serverNameFromRoomReference,
} from '../matrix-to'
import { normalizeUserId, parseUserIdList } from '../matrix-user'
import { stateEventNotice, stateEventTier } from '../state-event-notice'
import { inviteErrorMessage } from '../invite'
import { useModalFocus } from '../components/use-modal-focus'
import { MediaViewerProvider } from '../media/media-viewer'
import { MediaGalleryRow } from '../components/MediaGalleryRow'
import {
  groupMediaRuns,
  rowTs,
  runPosition,
  type TimelineRow,
} from '../timeline/group-media-runs'
import { viewMayClaimReadState } from '../timeline/arrival-order'
import { hiddenByRedaction } from '../timeline/visibility'
import { computeRoomReceipt } from '../timeline/room-receipt'
import {
  NATIVE_BACK_EDGE_PX,
  SWIPE_AXIS_RATIO,
  SWIPE_DECISION_THRESHOLD,
  SWIPE_MAX_Y,
  SWIPE_MIN_X,
} from '../gestures'
import { resolveEmojiShortcode, type EmojiEntry } from '../emoji'
import { SINGLE_PANE_QUERY } from '../layout'
import { perfMark, perfMarkFrames } from '../perf'
import { withSearchParam, withoutQueryParam } from '../search-tokens'
import { useServices } from '../services'
import { useShellActions } from '../shell-actions'
import { SHOW_HELP_EVENT, useShortcuts } from '../shortcuts'
import {
  SLASH_COMMAND,
  canonicalSlashCommandName,
  slashCommandUsage,
  type SlashCommandName,
} from '../slash-commands'
import { createMembersStore, type MembersStore } from '../stores/members'
import { roomKey, roomTitle, type MemberDto } from '../stores/room-list'
import {
  resolveRoomTarget,
  roomCommandSuggestions,
} from '../stores/room-command'
import {
  ROOM_SORTS,
  type RoomSort,
  type SettingsStore,
} from '../stores/settings'
import type { EphemeralStore } from '../stores/ephemeral'
import {
  createThreadsStore,
  threadRootId,
  type ThreadsStore,
} from '../stores/threads'
import type { ThreadUnreadStore } from '../stores/thread-unread'
import {
  READ_MARKERS_NAMESPACE,
  THREAD_READ_MARKERS_NAMESPACE,
} from '../stores/device-state'
import {
  type EventDto,
  type HeadLoadOutcome,
  type TimelineEvent,
  type TimelineStore,
} from '../stores/timeline'

const REACTION_COMMAND_ALIASES = new Map([
  ['+1', '👍'],
  ['thumbsup', '👍'],
  ['thumbs_up', '👍'],
  ['heart', '❤️'],
  ['joy', '😂'],
  ['tada', '🎉'],
  ['open_mouth', '😮'],
  ['cry', '😢'],
])

const LAST_JOINED_MEMBER_LEAVE_CONFIRM =
  'You are the only joined member in this room. If you leave, there may be no one left to invite you back. Leave this room?'

// Coupled to the current M19 server error message. Prefer a typed room-entry
// timeout code here if the `/rooms/{join,knock}` error envelope grows one.
const ROOM_ENTRY_TIMEOUT = /^(join|knock) timed out after \d+s$/i

// Swipe thresholds live in `src/gestures.ts` so the room and the media
// viewer's swipe paging (ADR 0081) cannot drift apart. Kept as local aliases
// so the call sites below read unchanged.
const SWIPE_RIGHT_MIN_X = SWIPE_MIN_X
const SWIPE_RIGHT_MAX_Y = SWIPE_MAX_Y
const SWIPE_RIGHT_AXIS_RATIO = SWIPE_AXIS_RATIO
// How far above the scroller a page starts loading, so it arrives during the
// scroll rather than after the reader has stopped at the top edge.
const SCROLL_BACK_PREFETCH_PX = 400
// How long a composer focus holds `keyboardPin` open. The keyboard's own show
// animation is under half this on every platform tested; the margin is for
// slow devices, not correctness at the edge.
const KEYBOARD_PIN_MS = 600
// Pages one arrival at the top may pull automatically. The chain exists to
// cross runs of filtered-out events, not to walk history unattended.
const AUTO_SCROLL_BACK_PAGES = 5
// How far the scroll anchor may drift from the viewport, in viewport heights,
// before a fresh one is taken. Growth between the anchor and the visible rows
// is not counted, so it has to stay nearby — but retaking it costs a missed
// correction, so not too eagerly either.
const ANCHOR_REACH = 1.5

type SwipeStart = {
  x: number
  y: number
}

function isUnableToDecrypt(event: EventDto): boolean {
  return event.type === 'm.room.encrypted' && event.content === null
}

function timelineContainsEvent(
  events: readonly TimelineEvent[],
  eventId: string | null,
): boolean {
  return events.some((event) => event.event_id === eventId)
}

function isGestureControl(target: EventTarget | null): boolean {
  return (
    target instanceof Element &&
    target.closest(
      'a, button, input, textarea, select, summary, [contenteditable="true"], [role="button"], [role="textbox"], emoji-picker',
    ) !== null
  )
}

/**
 * True if `target` sits inside an element that scrolls horizontally on its
 * own (a wide code block or table, ADR 0046's message rendering) — those
 * need real touch panning, so the swipe-to-room-list gesture must not claim
 * touches that start inside them. Stops walking at `.room-body`, the swipe
 * region's own boundary.
 */
function isHorizontallyScrollable(target: EventTarget | null): boolean {
  let el = target instanceof Element ? target : null
  while (el !== null && !el.classList.contains('room-body')) {
    if (
      el instanceof HTMLElement &&
      el.scrollWidth > el.clientWidth &&
      /(auto|scroll)/.test(getComputedStyle(el).overflowX)
    ) {
      return true
    }
    el = el.parentElement
  }
  return false
}

function roomEntryPendingMessage(
  kind: 'join' | 'knock',
  roomIdOrAlias: string,
): string {
  return `${kind === 'join' ? 'Joining' : 'Knocking on'} ${roomIdOrAlias}…`
}

function roomEntryFailureMessage(message: string): string {
  if (!ROOM_ENTRY_TIMEOUT.test(message)) {
    return message
  }
  return `${message}. The homeserver did not finish before Axon's room-entry timeout. The room may still appear after sync catches up; for large federated rooms, try a Matrix.to link with via hints or increase sync.room_entry_timeout_secs.`
}

function serverNameFromUserId(userId: string | null): string | null {
  if (userId === null) {
    return null
  }
  const colon = userId.indexOf(':')
  if (colon === -1 || colon === userId.length - 1) {
    return null
  }
  return userId.slice(colon + 1)
}

/**
 * Room view (ADR 0046): the M-W5 read-only timeline plus M-W7 messaging —
 * send with markdown-on-send, reply (ADR 0032), edit, redact-with-confirm,
 * reaction toggle, and threads (badges on roots, members hidden from the
 * main timeline like the TUI, panel via `?thread=` per the deep-link
 * contract).
 */
export function RoomPage() {
  const { params, query } = useRoute()
  const location = useLocation()
  const accountId = params.accountId
  const roomId = params.roomId
  perfMark('room-page:render', {
    accountId,
    roomId,
    thread: typeof query.thread === 'string',
    event: typeof query.event === 'string',
  })
  const {
    api,
    rooms,
    activeRoom,
    activeThread,
    live,
    ephemeral,
    ephemeralSender,
    deviceState,
    threadUnread,
    composerFocus,
    settings,
    search,
    timelines,
    attachments: staging,
  } = useServices()
  // Warm across room switches rather than rebuilt per mount (ADR 0085 phase
  // 1). The store may therefore arrive already populated — and stale, since
  // live frames only reach the mounted room — so the load effect below
  // gap-fills it instead of assuming a cold start.
  const timeline = useMemo(
    () => timelines.acquire(accountId, roomId),
    [timelines, accountId, roomId],
  )
  const redecryptAttempted = useRef(false)
  // RoomPage does not remount on a route change (preact-iso reuses the
  // instance), so the one-shot redecrypt guard must re-arm per room — without
  // this, the first room with a UTD consumed the kick for every room after it.
  useEffect(() => {
    redecryptAttempted.current = false
  }, [accountId, roomId])
  const threads = useMemo(
    () => createThreadsStore(api, accountId, roomId),
    [api, accountId, roomId],
  )
  const members = useMemo(
    () => createMembersStore(api, accountId, roomId),
    [api, accountId, roomId],
  )
  // A persisted preference, not per-room view state (Settings → Timeline).
  const stateEvents = settings.stateEvents.value
  const hideRedacted = settings.hideRedactedEvents.value
  const [reactionPickerEventId, setReactionPickerEventId] = useState<
    string | null
  >(null)
  const [roomInfoOpen, setRoomInfoOpen] = useState(false)
  const [jumpOpen, setJumpOpen] = useState(false)
  const [dateJumpStart, setDateJumpStart] = useState<number | null>(null)
  const [roomEntryStatus, setRoomEntryStatus] = useState<string | null>(null)
  const { openUnreadThreads, setJumpAction, setRoomChrome } = useShellActions()
  const heading = useRef<HTMLHeadingElement>(null)
  /** Scopes the media viewer's focus-restore lookup to this room's rows. */
  const roomStream = useRef<HTMLDivElement>(null)
  const swipeStart = useRef<SwipeStart | null>(null)
  const swipeLocked = useRef(false)
  const highlighted = typeof query.event === 'string' ? query.event : null
  // Whether this view may claim read state at all — the invariant the three
  // effects below share (see `clients/web/AGENTS.md`). Derived once rather than
  // re-stated per effect, so another condition is added in one place instead of
  // three (PR review).
  /** Not parked on a permalink or a search hit. */
  const unanchored = highlighted === null
  /** …and the loaded slice reaches the live end, so the newest events are shown. */
  const showingNewestEvents = viewMayClaimReadState({
    anchoredTo: highlighted,
    atEnd: timeline.atEnd.value,
    // The room stream's own loading term stays with each claim site rather than
    // here: the summary-derived marker deliberately fires for a slice that has
    // loaded but is missing the newest event, which is not the same question.
    loading: false,
  })
  const openThread = typeof query.thread === 'string' ? query.thread : null
  /**
   * Whether every thread in this room is known read — the room-list badge waits
   * on it. `threads`/`read_markers`/`thread_read_markers` must all have arrived
   * first: an unfetched summary map looks like a room with no threads, and an
   * unhydrated namespace makes every thread look unjudged.
   */
  /**
   * The two read-state gates, sharing their loading/hydration terms so a future
   * namespace cannot be added to one and missed by the other, and diverging
   * only where they must.
   *
   * `Loaded` is for the **receipt**: it demands the data actually arrived, since
   * a claim made on missing evidence is sent to the homeserver and cannot be
   * withdrawn. `Settled` is for the **room-list badge**: it only demands that
   * the fetches finished, because a failure has no retry until the socket drops
   * and reconnects, and a badge frozen forever is worse than one cleared early —
   * the badge self-corrects on the next load, and this is the second finding of
   * that exact shape (review: `threads.error`, then device-state hydration).
   */
  /**
   * One scalar standing in for the thread markers this page reads.
   * `threadReadMarker` reaches them behind a method call, so a memo depending on
   * them has nothing else to name — and listing the call directly reads to the
   * dependency rule as a redundant property of `deviceState`.
   *
   * Scoped to the marker namespace, not the whole store: a global counter meant
   * every draft keystroke re-ran the receipt scan (review).
   */
  const threadMarkerRevision = deviceState.revision(
    accountId,
    THREAD_READ_MARKERS_NAMESPACE,
  )
  const threadStoresLoaded =
    !threads.loading.value &&
    threads.error.value === null &&
    deviceState.hydrated(accountId, READ_MARKERS_NAMESPACE) &&
    deviceState.hydrated(accountId, THREAD_READ_MARKERS_NAMESPACE)
  const threadReadStateSettled =
    !threads.loading.value &&
    deviceState.hydrateSettled(accountId, READ_MARKERS_NAMESPACE) &&
    deviceState.hydrateSettled(accountId, THREAD_READ_MARKERS_NAMESPACE)
  const roomHasNoKnownUnreadThreads =
    threadReadStateSettled &&
    !threadUnread.entries.value.some(
      (entry) => entry.accountId === accountId && entry.roomId === roomId,
    )
  const unreadThreadCutoff = threadUnread.entries.value
    .filter((entry) => entry.accountId === accountId && entry.roomId === roomId)
    .reduce<number | null>(
      (oldest, entry) =>
        oldest === null ? entry.latestTs : Math.min(oldest, entry.latestTs),
      null,
    )

  const openJump = useCallback(() => setJumpOpen(true), [])
  useEffect(() => {
    setJumpAction(openJump)
    return () => setJumpAction(null)
  }, [openJump, setJumpAction])

  useEffect(() => {
    perfMark('room-page:initial-load-effect', {
      accountId,
      roomId,
      highlighted: highlighted !== null,
      // How often room entry finds a warm store (ADR 0085 phase 1) — the
      // relative value of phase 1 against the persisted phases is the one
      // input the ADR says differs sharply between phone and desktop, and it
      // rides on the mark that is already here rather than a new one.
      warm: timeline.events.peek().length > 0,
    })
    // The room list store also feeds this page's title; populate it on a
    // hard load straight into the room URL. `ensureLoaded` rather than a
    // length test: cached rows (ADR 0085 phase 2) make the list non-empty
    // while still unconfirmed, and a deep link may be the only thing asking.
    void rooms.ensureLoaded()
    void threads.refresh()
    void members.refresh()
    // With a deep-linked event the jump effect below owns the initial load,
    // with two exceptions. A thread jump is resolved by ThreadPanel and must
    // not leave the desktop room stream empty. And a warm store (ADR 0085
    // phase 1) has nobody else to refresh it: the jump effect fetches nothing
    // when its target is already in the loaded slice, and a warm slice stopped
    // receiving live frames when the user left the room. `loadLatest` over a
    // populated slice *is* `refreshHead`'s gap-fill merge, and a jump racing
    // it wins by slice generation.
    if (
      highlighted === null ||
      openThread !== null ||
      timeline.events.peek().length > 0
    ) {
      void timeline.loadLatest()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once per room instance
  }, [timeline])

  // The deep-link view this page is showing *now*, readable from an async
  // continuation created several renders ago. Bump its generation during
  // render rather than in an effect, because effect cleanup lags the render
  // that invalidates an old continuation. The value tuple avoids URL-string
  // comparison: the browser keeps a literal `:` in a room id reached by a cold
  // navigation while `localRoomHref` percent-encodes it. Account and thread are
  // both part of the identity because changing either invalidates an old route.
  const deepLinkIdentity = JSON.stringify([
    accountId,
    roomId,
    highlighted,
    openThread,
  ])
  const currentDeepLink = useRef({ identity: deepLinkIdentity, generation: 0 })
  if (currentDeepLink.current.identity !== deepLinkIdentity) {
    currentDeepLink.current = {
      identity: deepLinkIdentity,
      generation: currentDeepLink.current.generation + 1,
    }
  }
  const deepLinkGeneration = currentDeepLink.current.generation

  // Remembers the deep-link target already resolved. Routing into a thread
  // changes `openThread`, which re-runs the effect below with the same
  // `highlighted`; without this it fetches the same event a second time and
  // then jumps the main stream toward a reply that has no row in it. Once the
  // thread panel is open it owns the reveal.
  const resolvedDeepLink = useRef<{
    eventId: string
    rootId: string | null
  } | null>(null)

  // A `?event=` anchor this view cannot satisfy must not keep blocking read
  // state: while the URL names it, `unanchored` stays false for the room's whole
  // visit and every claim below is suppressed — no badge clear, no marker, no
  // receipt — even though the user is reading the newest messages (PR review).
  //
  // Dropping it is guarded four ways, because each of these has a failure mode
  // worse than leaving a stale anchor in place:
  //
  // 1. **Verified absent, not merely unverified.** A failed head load leaves the
  //    slice unchanged (empty on a cold open), which looks exactly like a slice
  //    that genuinely lacks the target. Concluding "unsatisfiable" there would
  //    destroy a live permalink over a transient blip, so a load error bails.
  // 2. **Absent from the loaded slice, not from the by-id lookup.** A 404 there
  //    means only that the server would not serve that one event by id; the
  //    event may still be in the room, and the e2e mock does exactly this for
  //    seeded history. Dropping on the lookup alone discards the highlight the
  //    deep link exists for.
  // 3. **Still this closure's anchor.** The continuation can outlive its own
  //    anchor: a second search hit (`?event=B`) or a room switch re-runs the
  //    effect with a fresh closure while this one is in flight, and the page does
  //    not remount across either (ADR 0085). Stripping `event` off whatever the
  //    URL says *now* would delete an anchor belonging to a later navigation.
  // 4. **Still mounted.** No later render updates the identity ref after the
  //    page unmounts, so the invoking effect also supplies a cleanup-backed
  //    liveness check before this helper can mutate browser history.
  //
  // Only `event` is removed — `?thread=` and the rest are none of this effect's
  // business, and a deep link that resolves *into* a thread keeps its anchor and
  // stays correctly parked.
  const dropUnsatisfiableAnchor = useCallback(
    async (isCurrent: () => boolean) => {
      if (!isCurrent()) {
        return
      }
      // Already loaded? Then the anchor is satisfiable and there is nothing to
      // drop — and the refresh below must not run, because `refreshHead`'s
      // wholesale-replace branch discards the outgoing slice entirely and would
      // evict the very event being checked for, turning a live permalink into a
      // "verified absent" one (PR review).
      // `superseded` is not a failure: a sibling load — the mount effect's own, or
      // a reconnect gap-fill — won the generation race, and it may have applied a
      // perfectly good head. Try at most twice, checking the winner's slice
      // before either refresh; a second supersession means "couldn't verify"
      // and keeps the anchor, which is the safe direction (PR review).
      let outcome: HeadLoadOutcome = 'superseded'
      for (let attempt = 0; attempt < 2; attempt += 1) {
        if (timelineContainsEvent(timeline.events.peek(), highlighted)) {
          return
        }
        outcome = await timeline.loadLatest()
        if (!isCurrent()) {
          return
        }
        if (outcome !== 'superseded') {
          break
        }
      }
      // Two remaining questions, both required: did a head actually get applied
      // (a decline and a success leave identical state behind), and does the slice
      // still reach the live end (a jump can move it out from under this
      // continuation between the load and the check)?
      if (outcome !== 'applied' || !timeline.atEnd.peek()) {
        return
      }
      if (timelineContainsEvent(timeline.events.peek(), highlighted)) {
        return
      }
      // Still this continuation's anchor, in this room, under this account? A slow
      // lookup outlives its own anchor when the user follows a second deep link,
      // switches rooms, or switches accounts, and the page remounts across none of
      // them (ADR 0085).
      if (!isCurrent()) {
        return
      }
      location.route(
        withoutQueryParam(
          `${window.location.pathname}${window.location.search}`,
          'event',
        ),
        true,
      )
    },
    [highlighted, location, timeline],
  )

  // Jump to the `?event=` target — on a cold deep link *and* whenever the
  // query changes while the room is already open (WCR-09; M-W10's search
  // results navigate this way). An event already in the loaded slice needs
  // no page change — the Timeline reveal effect scrolls it into view. A
  // network-level failure falls back to the newest page, whose own error
  // handling owns the surface (WCR-02).
  useEffect(() => {
    if (highlighted === null) {
      return
    }
    if (timelineContainsEvent(timeline.events.value, highlighted)) {
      return
    }
    const resolved = resolvedDeepLink.current
    if (
      resolved !== null &&
      resolved.eventId === highlighted &&
      resolved.rootId !== null &&
      resolved.rootId === openThread
    ) {
      return
    }
    // The render-bumped generation closes the render-before-cleanup window; the
    // effect-local flag closes unmount, when no later render exists to bump it.
    // Every continuation that can route checks both.
    let active = true
    const isCurrent = () =>
      active && currentDeepLink.current.generation === deepLinkGeneration
    inBackground(
      api
        .GET('/v1/accounts/{account_id}/events/{event_id}', {
          params: { path: { account_id: accountId, event_id: highlighted } },
        })
        .then(
          async ({ data, response }) => {
            if (!isCurrent()) {
              return
            }
            if (data === undefined) {
              // Only a 404 is the server saying it has no such event. A 5xx or a
              // rate limit says nothing about the anchor and must not cost the
              // user their permalink, so it falls back exactly like a rejection.
              if (response.status === 404) {
                await dropUnsatisfiableAnchor(isCurrent)
              } else {
                await timeline.loadLatest()
              }
              return
            }
            const rootId = threadRootId(data.data)
            resolvedDeepLink.current = { eventId: highlighted, rootId }
            // A thread reply has no row in the main room stream to jump to —
            // it only ever renders inside its thread's own panel. Route
            // straight there instead of jumping the main timeline toward the
            // reply's timestamp, which would land near the thread root but
            // never open the thread (or reveal the reply itself).
            if (rootId !== null && rootId !== openThread) {
              location.route(
                localThreadEventHref(accountId, roomId, rootId, highlighted),
                true,
              )
              return
            }
            await timeline.jumpTo(data.data.origin_ts)
          },
          // A rejected lookup — offline, timeout, DNS — proves nothing about the
          // anchor either. Load the tail and leave the URL alone so a retry or a
          // reload can still resolve the permalink.
          async () => {
            if (isCurrent()) {
              await timeline.loadLatest()
            }
          },
        ),
    )
    return () => {
      active = false
    }
  }, [
    api,
    accountId,
    roomId,
    timeline,
    highlighted,
    openThread,
    location,
    dropUnsatisfiableAnchor,
    deepLinkGeneration,
  ])

  // Mark this room active while it is open so shared chrome can mark the row.
  useEffect(() => {
    perfMark('room-page:active-room-effect', { accountId, roomId })
    activeRoom.value = roomKey({ account_id: accountId, room_id: roomId })
    return () => {
      activeRoom.value = null
    }
  }, [activeRoom, accountId, roomId])

  // Fetch this account's drafts + read markers so the composer can prefill and
  // the read marker can advance forward-only (M-W6 steps 5b/5c, ADR 0048);
  // idempotent per account.
  useEffect(() => {
    deviceState.hydrateDrafts(accountId)
    deviceState.hydrateReadMarkers(accountId)
    deviceState.hydrateThreadReadMarkers(accountId)
  }, [deviceState, accountId])

  // Clear summary-derived room-list unread as soon as the room opens. The
  // timeline may not have loaded the newest summary event yet, especially on
  // quick mobile switches — which is why this reads the *summary* rather than
  // waiting for the timeline effect below.
  //
  // Anchored loads (`?event=`) and slices parked short of the live end (for
  // example after jump-to-date) are excluded: this marker is cross-device (ADR
  // 0048), and a sibling device turns it straight into a zeroed badge
  // (`connectReadMarkers`). Advancing it from either view would mark a room read
  // *everywhere* from a view that never showed the summary's newest event.
  useEffect(() => {
    // The slice must have *loaded*, not merely be at its live end. The room list
    // is restored from IndexedDB (ADR 0085 phase 2) and is on screen before the
    // first timeline page exists, so this effect otherwise runs against an empty
    // slice — where the thread-member check below cannot see anything, seeds the
    // marker from the reply, and (being forward-only on `origin_ts`) can never
    // be walked back. Measured on a live instance as `loadedEvents: 0,
    // timelineLoading: true` with the summary already present.
    //
    // Gating on `loading` alone, not on emptiness: this effect exists for a slice
    // that has loaded but is *missing the newest event* — a quick mobile switch,
    // a gap-filled head, a room whose page came back empty — and those must
    // still advance. `loading` starts `true` on a cold store, so it is precisely
    // the pre-first-page window that closes.
    if (!showingNewestEvents || timeline.loading.value) {
      return
    }
    const room = rooms.rooms.value.find(
      (candidate) =>
        candidate.account_id === accountId && candidate.room_id === roomId,
    )
    // `last_event_id` is `MAX(origin_ts)` over *every* event in the room, thread
    // replies included, and this marker is a main-timeline position — where to
    // draw the "new messages" line (ADR 0048, ADR 0096 § 1). In a room whose
    // newest event is a reply the two disagree, and seeding from the summary
    // parks the marker on an event the main timeline never renders. That is not
    // just untidy: the marker is what `reconcileSummary` falls back to for a
    // thread with no marker of its own, so the fallback ends up answering "read"
    // for the very thread it came from, and the room reports no unread thread
    // while badging for one (#209).
    //
    // The summary event must therefore be *classifiable*: present in the loaded
    // slice and not a thread member. Treating "absent" as "not a thread reply"
    // was the same inference under a different race — `rooms` and `timeline` are
    // independently live-updated, so a reply can reach the summary before the
    // slice and get seeded exactly as it did on a cold start (review, blocking
    // 4). When it cannot be classified this effect stands down and the timeline
    // effect below owns the marker; that costs the marker a beat in the case
    // this effect was written for (a slice missing the newest event), which is
    // cheap next to seeding a read position from an event nobody can identify.
    const summaryEvent = timeline.events.value.find(
      (event) => event.event_id === room?.last_event_id,
    )
    if (
      summaryEvent !== undefined &&
      threadRootId(summaryEvent) === null &&
      room?.last_event_id !== null &&
      room?.last_event_id !== undefined &&
      (unreadThreadCutoff === null ||
        room.last_activity_ts < unreadThreadCutoff)
    ) {
      deviceState.advanceReadMarker(
        accountId,
        roomId,
        room.last_event_id,
        room.last_activity_ts,
      )
    }
  }, [
    accountId,
    roomId,
    rooms.rooms.value,
    timeline.events.value,
    timeline.loading.value,
    deviceState,
    unreadThreadCutoff,
    showingNewestEvents,
  ])

  useEffect(() => {
    // Clear the badge optimistically, since opening a room normally does read
    // it and the receipt makes that true a moment later — but *only* when this
    // page is not anchored to a specific event. With `?event=` the view opens
    // parked in history (a search hit, a permalink), the server correctly
    // refuses to advance the read receipt past what was displayed (ADR 0089),
    // and zeroing here would claim messages below the anchor as read: the
    // badge disappears for the session and returns on the next load, which is
    // worse than never clearing. The timeline effect below picks the room up
    // once the view actually reaches the live end.
    //
    // It also waits on this room's threads being known read. Opening a room does
    // not read its threads, so clearing here left the user looking at a room
    // with no badge in the list and an unread count on the Threads button —
    // two indicators disagreeing about the same room. The wait is bounded by the
    // thread-summary and marker reads, and it re-fires as soon as the last
    // unread thread is opened, which is when the two agree again.
    if (unanchored && roomHasNoKnownUnreadThreads) {
      rooms.noteUnreadCounts(accountId, roomId, 0, 0)
    }
  }, [unanchored, roomHasNoKnownUnreadThreads, rooms, accountId, roomId])

  // Thread members live in the panel, not the main timeline (TUI parity);
  // state events are tiered behind the visibility setting, and bodyless
  // unsupported events are developer diagnostics rather than ordinary timeline
  // content.
  const isVisibleTimelineEvent = useCallback(
    (event: TimelineEvent): boolean => {
      if (threadRootId(event) !== null) {
        return false
      }
      if (!settings.developerMode.value && isUnsupportedBodylessEvent(event)) {
        return false
      }
      if (hideRedacted && event.redacted) {
        return false
      }
      if (!isStateEvent(event)) {
        return true
      }
      if (stateEvents === 'all') {
        return true
      }
      // The `important` tier is membership only, and only when there is
      // something to say: a member event whose profile fields are unchanged is
      // routine re-sync traffic with no notice to render (ADR 0083).
      return (
        stateEvents === 'important' &&
        stateEventTier(event) === 'important' &&
        stateEventNotice(event) !== null
      )
    },
    [hideRedacted, settings.developerMode.value, stateEvents],
  )

  /**
   * Memoised on the values the filter actually reads, not on render.
   *
   * A fresh `.filter()` per render is cheap on its own, but it is the input to
   * `groupMediaRuns` below and to the media viewer's `imageSequence` — both of
   * which walk every event through `parseMedia`. Handing them a new array
   * identity on every render made both re-run on every `RoomPage` render, over
   * up to `RETAINED_EVENT_LIMIT` events, including renders caused by things
   * with no bearing on the timeline's contents at all: opening the reaction
   * picker, the jump dialog, the room-info panel.
   */
  const visible = useMemo(
    () => timeline.events.value.filter(isVisibleTimelineEvent),
    [timeline.events.value, isVisibleTimelineEvent],
  )

  /**
   * The events this view has put on screen, in display order — the candidate
   * set both read positions are picked from (ADR 0089). Memoised on what the
   * filter actually reads: the effect below runs on it, and so does the thread
   * receipt ceiling, and a fresh array per render would re-run both on renders
   * that cannot change either answer.
   */
  const displayed = useMemo(
    () =>
      // Derived from `visible` rather than re-filtering the slice: the two ran
      // `isVisibleTimelineEvent` over every loaded event independently, which is
      // the pass this memo exists to avoid doing twice (review).
      visible.filter(
        (event) =>
          // A local echo has not been ingested, so it has no arrival position
          // and is not a receipt candidate; it is not the room's read position
          // either.
          !event.event_id.startsWith('local:') &&
          (unreadThreadCutoff === null || event.origin_ts < unreadThreadCutoff),
      ),
    [visible, unreadThreadCutoff],
  )

  /**
   * How far past its own target a thread view may push the room's receipt
   * (ADR 0096 § 2) — exclusive, `null` for "no bound".
   *
   * A receipt has no thread scope: naming an event acknowledges everything at or
   * before it in *arrival* order. The room view has already named the
   * arrival-max event it displayed, so everything at or below that is
   * acknowledged either way, and extending to a thread member only adds the
   * window above it. The question is therefore not "has the user read every
   * thread in this room" — the first implementation asked that, and in the very
   * room from #207 the answer is permanently no, because a room full of threads
   * nobody has opened this session can never satisfy it. It is the much narrower
   * "does this window contain a reply from a thread the panel is not showing".
   *
   * So the bound is the first such reply above the room's own target. A thread
   * view may name any member below it, and stops there.
   */
  /**
   * What this view may acknowledge in the room, in arrival order.
   *
   * `blocker` is the first reply *above the main timeline's own target* that
   * belongs to another thread and is not known read — the exclusive bound a
   * thread panel may name up to. A reply covered by a thread marker is not an
   * obstruction; one with no marker is, which is the never-opened case.
   *
   * `target` is the arrival-max event the *room view* may name: main-timeline
   * rows as before, extended over thread replies already known read, up to the
   * blocker. That extension is what closes the room out. Without it the receipt
   * depends on which panel happened to be open when a thread became eligible:
   * read the newest thread first and its panel names nothing (an older thread
   * still holds the bound down), read the older one next and its panel names
   * only its own reply — the newest thread is eligible by then, but its panel is
   * closed and nothing revisits it, so the room stays one event short forever.
   * Observed exactly that way on a live account (#207).
   *
   * Claiming a read thread's reply keeps ADR 0089's rule: a thread marker exists
   * because some client of this user displayed those replies — this panel
   * earlier, or Element, whose threaded receipts arrive through
   * `connectThreadReceipts`.
   *
   * Not memoised: it reads per-thread markers, and a `useMemo` would have to
   * list a device-state snapshot as a dependency to notice one changing. The
   * passes are one skip-test per loaded event, nearly all exiting on the first
   * comparison.
   */
  const markersHydrated = deviceState.hydrated(
    accountId,
    THREAD_READ_MARKERS_NAMESPACE,
  )
  const roomReadMarker = deviceState.readMarker(accountId, roomId)
  /**
   * The room marker, but only where it can speak for a *thread's* read position.
   * It cannot when it points at a thread member, and this is not hypothetical
   * history: `advanceReadMarker` is forward-only on `origin_ts`, and every
   * session before the guard above seeded the marker from `last_event_id` —
   * `MAX(origin_ts)` over every event, replies included. So a real account
   * carries a marker parked on a reply, durably, and `reconcileSummary`'s
   * fallback would answer "read" for the very thread it came from. Preventing
   * new poisoning does not heal that; withholding the marker does, on the next
   * load, with no migration.
   *
   * A marker whose event is not in the loaded slice stays admissible — it is
   * usually one that scrolled out of a long room, and withholding it there would
   * flag every old thread in rooms that have nothing wrong with them.
   *
   * Withholding alone is not enough, because "no read position" is not "unread":
   * `reconcileSummary` deliberately records nothing when it has nothing to
   * compare against, so a withheld marker leaves the thread unflagged — silent
   * in a different way. What replaces it is the position the marker should have
   * held: the display-last event the main timeline actually rendered. That reads
   * as "you are caught up on the room stream to here", which is true, and makes
   * a reply newer than it unread, which is also true.
   */
  const mainTimelineRead = displayed.at(-1)
  // Memoised on the marker's id rather than left in the render body: it is a
  // full scan of the slice, and the page re-renders for typing, reaction
  // pickers and unrelated signal churn (review).
  const markerIsThreadMember = useMemo(
    () =>
      roomReadMarker !== null &&
      timeline.events.value.some(
        (event) =>
          event.event_id === roomReadMarker.eventId &&
          threadRootId(event) !== null,
      ),
    [timeline.events.value, roomReadMarker],
  )
  // Memoised on the values rather than the objects: `readMarker()` parses a
  // fresh object on every call, so an unmemoised result re-runs the reconcile
  // effect below on every render of the page.
  const roomMarkerForThreads = useMemo(
    () =>
      markerIsThreadMember
        ? mainTimelineRead === undefined
          ? null
          : {
              eventId: mainTimelineRead.event_id,
              originTs: mainTimelineRead.origin_ts,
            }
        : roomReadMarker,
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [
      markerIsThreadMember,
      mainTimelineRead?.event_id,
      mainTimelineRead?.origin_ts,
      roomReadMarker?.eventId,
      roomReadMarker?.originTs,
    ],
  )

  /**
   * Memoised on `deviceState.revision` rather than left to rebuild per render:
   * it is a dependency of the reconcile effect, and a fresh array of fresh
   * objects re-fired that effect on every render of the page — reaction picker,
   * typing indicator, anything (review, non-blocking).
   */
  const threadSummaryStates = useMemo(() => {
    // Subscribes the memo to marker writes; `threadReadMarker` reads them
    // behind a method call, which the dependency rule cannot see (same idiom as
    // `ephemeral.revision` below).
    void threadMarkerRevision
    return [...threads.summaries.value.values()].map((summary) => ({
      summary,
      threadMarker: deviceState.threadReadMarker(
        accountId,
        roomId,
        summary.root_event_id,
      ),
      rootPreview: eventPreview(threads.roots.value.get(summary.root_event_id)),
    }))
  }, [
    threads.summaries.value,
    threads.roots.value,
    deviceState,
    threadMarkerRevision,
    accountId,
    roomId,
  ])

  /**
   * The receipt bound and target for this room (ADR 0096). The rule itself lives
   * in `timeline/room-receipt.ts` so it can be tested without a page tree; this
   * only adapts the page's stores to its inputs.
   */
  const roomReceipt = useMemo(() => {
    void threadMarkerRevision
    const candidate = (event: TimelineEvent) => ({
      ...event,
      threadRoot: threadRootId(event),
    })
    return computeRoomReceipt({
      displayed: displayed.map(candidate),
      loaded: timeline.events.value
        .filter((event) => !hiddenByRedaction(event, hideRedacted))
        .map(candidate),
      openThread,
      threads: threadSummaryStates.map(({ summary, threadMarker }) => ({
        summary,
        marker: threadMarker,
      })),
      roomMarker: roomMarkerForThreads,
      storesLoaded: threadStoresLoaded,
      knownUnreadCutoff: unreadThreadCutoff,
    })
  }, [
    displayed,
    timeline.events.value,
    threadStoresLoaded,
    threadSummaryStates,
    roomMarkerForThreads,
    unreadThreadCutoff,
    openThread,
    hideRedacted,
    threadMarkerRevision,
  ])
  const threadReceiptCeiling = roomReceipt.blocker

  // Advance this room's read marker to the newest event while it is open, so
  // sibling devices see it as read (M-W6 step 5c, ADR 0048). Hidden unread
  // thread replies are a hard stop: the user has opened the room, not that
  // thread panel, so a room marker must not silently consume them.
  //
  // An anchored view (`?event=`) and a slice that stops short of the present are
  // both hard stops: the newest *loaded* event is then not the room's newest,
  // and naming it would claim everything before it as read — including messages
  // this view never showed.
  //
  // Both conditions are needed. `atEnd` alone is not enough: once a room summary
  // is known, the head gets gap-filled, so a view parked at a five-day-old
  // search hit can have the newest events loaded and `atEnd` true while the user
  // is still looking at history. `highlighted` alone is not enough either — it
  // says nothing about a plain scroll-back that has paged away from the live end.
  //
  // The cost is that an anchored view never marks the room read, even after
  // scrolling to the bottom; reopening the room normally does. That matches how
  // other Matrix clients treat permalinks, and erring the other way silently
  // consumes unread messages.
  //
  // The gate above answers "may this view claim anything?". Which *event* is
  // named is a second, independent question, and the two consumers below answer
  // it differently — one candidate set, two picks (ADR 0089):
  //
  //   - the cross-device marker is a display-order artifact (where to draw the
  //     "new messages" line), so it names the display-last event, on `origin_ts`;
  //   - a Matrix receipt is interpreted in *arrival* order, so it names the
  //     greatest `arrival_order` among the same events.
  //
  // They disagree whenever a homeserver delivers an event stamped earlier than
  // events already held — routinely, for a bridge backfilling a conversation
  // into a freshly created portal, where the room's only message is oldest by
  // `origin_ts` and newest by arrival order. Naming the display-last event there
  // receipts a portal state event that does not cover the message, and the room
  // shows unread forever. Do not collapse these back into one pick: handing the
  // arrival-newest event to `advanceReadMarker` would feed it an older
  // `origin_ts` than the marker already holds, and the marker — forward-only on
  // `origin_ts` — would simply stop advancing.
  useEffect(() => {
    // The loading term is here for the same reason its siblings carry it: an
    // `atEnd` slice that has not loaded is vacuously at the end. Not reachable
    // on today's load path — `events.value` is only populated once `loading`
    // flips — but the two are independent signals, and a future path that fills
    // one before the other would make this claim read state it never showed
    // (review).
    if (!showingNewestEvents || timeline.loading.value) {
      return
    }
    // Display order → cross-device marker (M-W6 step 5c, ADR 0048).
    const last = displayed.at(-1)
    if (last !== undefined) {
      deviceState.advanceReadMarker(
        accountId,
        roomId,
        last.event_id,
        last.origin_ts,
      )
    }
    // Arrival order → the Matrix receipt. Second, fire-and-forget action
    // alongside the cross-device marker (ADR 0067): tell the homeserver too, so
    // third-party Matrix clients see the room as read. Forward-only (on
    // `arrival_order`) + debounced inside the sender.
    // `roomReceipt.target`, not the display-last row: the room view may also
    // claim thread replies it knows are read, which is the only thing that ever
    // closes out a room whose newest events are all in threads (#207).
    const target = roomReceipt.target
    if (target !== null) {
      ephemeralSender.noteRead(
        accountId,
        roomId,
        target.event_id,
        target.arrival_order,
      )
    }
  }, [
    displayed,
    roomReceipt.target,
    showingNewestEvents,
    timeline.loading.value,
    accountId,
    roomId,
    deviceState,
    ephemeralSender,
  ])

  // Clear any live typing notice when leaving this room (RoomPage does not
  // remount on a route change, so the cleanup runs with the room being left).
  useEffect(() => {
    return () => {
      ephemeralSender.stopTyping(accountId, roomId)
    }
  }, [ephemeralSender, accountId, roomId])

  useEffect(() => {
    activeThread.value =
      openThread === null
        ? null
        : { accountId, roomId, rootEventId: openThread }
    return () => {
      const current = activeThread.value
      if (
        current !== null &&
        current.accountId === accountId &&
        current.roomId === roomId &&
        current.rootEventId === openThread
      ) {
        activeThread.value = null
      }
    }
  }, [activeThread, accountId, roomId, openThread])

  // Live: fold incoming events for this room into the timeline as they arrive.
  // Membership state changes can update display names and avatar URLs; re-read
  // the roster so sender labels and avatar images stay current without a room
  // reload.
  useEffect(() => {
    return live.subscribe((frame) => {
      const event = timelineEvent(frame)
      if (
        event === null ||
        event.account_id !== accountId ||
        event.room_id !== roomId
      ) {
        return
      }
      timeline.ingestLive(event)
      if (event.type === 'm.room.member') {
        inBackground(members.refresh())
      }
      if (threadRootId(event) !== null) {
        inBackground(threads.refresh())
      }
    })
  }, [live, timeline, threads, members, accountId, roomId])

  // Gap-fill: the bus is lossy and cursor-less, so on reconnect refetch the
  // room's head and reconcile by event id (ADR 0061). `reconnects` starts at 0
  // and only bumps on an actual reconnect, so the initial load isn't doubled.
  useEffect(() => {
    if (live.reconnects.value === 0) {
      return
    }
    void timeline.loadLatest()
    inBackground(members.refresh())
  }, [live.reconnects.value, timeline, members])

  useEffect(() => {
    if (
      redecryptAttempted.current ||
      timeline.loading.value ||
      !timeline.events.value.some(isUnableToDecrypt)
    ) {
      return
    }
    redecryptAttempted.current = true
    inBackground(
      api
        .POST('/v1/accounts/{account_id}/utds/redecrypt', {
          params: { path: { account_id: accountId } },
        })
        .then(({ data, error }) => {
          if (error !== undefined) {
            timeline.error.value = apiErrorMessage(error)
            return
          }
          if ((data?.data.decrypted ?? 0) > 0) {
            void timeline.loadLatest()
          }
        }),
    )
  }, [api, accountId, timeline, timeline.events.value, timeline.loading.value])

  const room = rooms.rooms.value.find(
    (candidate) =>
      candidate.account_id === accountId && candidate.room_id === roomId,
  )
  const cachedTitle = rooms.titles.value.get(
    roomKey({ account_id: accountId, room_id: roomId }),
  )
  const title =
    room !== undefined
      ? roomTitle(room, rooms.titles.value)
      : (cachedTitle ?? roomId)
  const composerLabelTitle = title !== roomId ? title : null
  const ownUserId = room?.account_user_id ?? null
  // Completions and reference links resolve against this account's rooms,
  // matching what the thread panel is handed.
  const accountRooms = rooms.rooms.value.filter(
    (candidate) => candidate.account_id === accountId,
  )
  const {
    action,
    setAction,
    banner: composerBanner,
    attachable,
    attachments,
    stage,
    removeAttachment,
    dragging,
    dropHandlers,
    emojiEntries,
    formatComposerBody,
    mentionCompletions,
    roomReferenceCompletions,
    emojiCompletions,
    submitMessage,
  } = useMessageComposer({
    timeline,
    accountId,
    members,
    rooms: accountRooms,
    roomTitles: rooms.titles.value,
    ownUserId,
    // Account *and* room: a room joined by two accounts is two rows
    // (`roomKey`), and a file staged under one must never be sent from the
    // other. Staging is now retained per scope (issue #89), so this key
    // decides what the composer shows as well as what a send picks up —
    // joined on `'\0'` like every other composite key, since a printable
    // separator is a collision waiting to happen.
    attachmentScope: `${accountId}\0${roomId}`,
    onMutation: search.clear,
    staging,
  })
  void ephemeral.revision.value
  const typingText = formatTypingIndicator(
    ephemeral.typingUsers(accountId, roomId, ownUserId),
    members,
  )
  /**
   * Whether a thread view in this room may claim read state at all (ADR 0096
   * § 2). This is the room half: the stream is showing the room's newest events,
   * so no main-timeline message sits unrendered below the thread's tail, *and*
   * that stream has actually loaded.
   *
   * The second half is not implied by the first. `atEnd` starts `true` on a cold
   * store — it means "nothing newer is known to exist", which is vacuously so
   * before the first page lands — and `threadReceiptCeiling` is derived from the
   * slice, so an empty one reports no obstruction rather than no knowledge. The
   * panel's own endpoint can easily answer first, and a receipt sent then would
   * name a thread member with the room's own events still in flight.
   *
   * The panel adds the half only it knows, that its own slice reaches the
   * thread's newest reply, and `threadReceiptCeiling` bounds how far it reaches.
   */
  const mayNameRoomReceiptFromThread =
    showingNewestEvents &&
    !timeline.loading.value &&
    timeline.events.value.length > 0

  useEffect(() => {
    // Not before the per-thread markers have arrived. Thread summaries come back
    // first on a fresh load, and judging them against the *room* marker in the
    // meantime flags every thread whose replies are newer than the main
    // timeline — a badge on the Threads button that corrects itself a moment
    // later, seen as a flash on every reload. An unhydrated namespace is "not
    // known yet", not "no marker", the same distinction the receipt gate makes.
    if (!deviceState.hydrated(accountId, THREAD_READ_MARKERS_NAMESPACE)) {
      return
    }
    for (const { summary, threadMarker, rootPreview } of threadSummaryStates) {
      threadUnread.reconcileSummary(summary, {
        accountId,
        roomId,
        roomTitle: title,
        rootPreview,
        roomMarker: roomMarkerForThreads,
        threadMarker,
      })
    }
  }, [
    threadUnread,
    threadSummaryStates,
    accountId,
    roomId,
    title,
    roomMarkerForThreads,
    deviceState,
    markersHydrated,
  ])

  /**
   * Adjacent images from one sender collapse into a gallery row (ADR 0081).
   * Computed here rather than inside `Timeline` so the media viewer can share
   * the same pass — its counter names the position within a run as well as
   * within the room, and a second grouping pass for that would be waste.
   *
   * A jump target is deliberately *not* excluded. Forcing it back to an
   * ordinary row was the first approach, and it made a deep link tear the
   * gallery it pointed into apart — up to three rows where there had been
   * one. Instead each cell carries its own `data-event-id`, so
   * `centerHighlightedRow` finds and centres the cell itself. Anchoring is
   * unaffected: `captureAnchor` searches `li.event-row`, and a cell is an
   * `li.gallery-cell`.
   */
  const rows = useMemo(() => groupMediaRuns(visible), [visible])

  // The user-visible moment of a room open: when messages replace "Loading
  // messages…". `hasRows` is the gate — the pane also renders with nothing in
  // it, and counting that as a paint would flatter every cold open, which is
  // the same trap `room-list:render` records for the room list.
  //
  // Emitted on *transitions only*, not on every render. The summary wants the
  // first render that put rows on screen; composer keystrokes, reactions and
  // receipt churn re-render this component constantly, and each one was
  // calling native `performance.mark()`, whose entries nothing ever clears.
  // Marking what did not change is the instrumentation perturbing what it
  // measures.
  const hasTimelineRows = !timeline.loading.value && visible.length > 0
  const renderState = roomId + timeline.loading.value + hasTimelineRows
  const lastRenderState = useRef<string | null>(null)
  if (lastRenderState.current !== renderState) {
    lastRenderState.current = renderState
    perfMark('room-page:timeline-render', {
      roomId,
      loading: timeline.loading.value,
      visible: visible.length,
      hasRows: hasTimelineRows,
    })
  }

  const handleComposerCommandFor = ({
    body,
    timeline: commandTimeline,
    visible: commandVisible,
    isVisible: isCommandVisible,
    action: commandAction,
    setAction: setCommandAction,
    setReactionPickerEventId: setCommandReactionPickerEventId,
    formatComposerBody: commandFormatComposerBody,
    allowThreadCommand,
    onMutation,
  }: {
    body: string
    timeline: TimelineStore
    visible: readonly TimelineEvent[]
    isVisible: (event: TimelineEvent) => boolean
    action: ComposerAction | null
    setAction: (action: ComposerAction | null) => void
    setReactionPickerEventId: (eventId: string | null) => void
    formatComposerBody: typeof formatComposerBody
    allowThreadCommand: boolean
    onMutation: () => void
  }): boolean | Promise<boolean> => {
    const latestTarget = (): TimelineEvent | null => {
      for (let i = commandVisible.length - 1; i >= 0; i -= 1) {
        const event = commandVisible[i]
        if (isReactable(event)) {
          return event
        }
      }
      return null
    }
    const command = parseComposerCommand(body)
    if (command === null) {
      commandTimeline.error.value = `unknown command: ${body.split(/\s+/, 1)[0]}`
      return false
    }
    if (command.kind === 'help' || command.kind === 'shortcuts') {
      window.dispatchEvent(new Event(SHOW_HELP_EVENT))
      return true
    }
    if (command.kind === 'usage') {
      commandTimeline.error.value = `usage: ${slashCommandUsage(command.name)}`
      return false
    }
    if (command.kind === 'formatted-message') {
      const current = commandAction
      setCommandAction(null)
      void commandTimeline
        .send(command.message.body, {
          replyTo:
            current?.kind === 'reply' ? current.event.event_id : undefined,
          senderId: ownUserId ?? undefined,
          formattedBody: command.message.formattedBody,
        })
        .then((ok) => {
          if (ok) onMutation()
        })
      return true
    }
    if (command.kind === 'refresh') {
      inBackground(rooms.refresh())
      return true
    }
    if (command.kind === 'unreadthreads') {
      openUnreadThreads()
      return true
    }
    if (command.kind === 'sort') {
      if (command.value === null) {
        commandTimeline.error.value = `usage: ${slashCommandUsage(SLASH_COMMAND.sort)}`
        return false
      }
      settings.roomSort.value = command.value
      return true
    }
    if (command.kind === 'search') {
      // The overlay parses the args itself; invoked from a room, its scope
      // defaults to this room (ADR 0066) — no prefill needed here.
      location.route(withSearchParam(location.url, command.args))
      return true
    }
    if (command.kind === 'jump') {
      if (command.date === null) {
        setJumpOpen(true)
        return true
      }
      const day = parseCalendarDay(command.date)
      if (day === null) {
        commandTimeline.error.value = `usage: ${slashCommandUsage(SLASH_COMMAND.jump)}`
        return false
      }
      setDateJumpStart(day.start)
      void commandTimeline.jumpToDate(day.start, day.end, isCommandVisible)
      return true
    }
    if (command.kind === 'whereami') {
      openRoomInfo()
      return true
    }
    if (command.kind === 'room') {
      if (command.target === '') {
        commandTimeline.error.value = `usage: ${slashCommandUsage(SLASH_COMMAND.room)}`
        return false
      }
      const visibleRooms = rooms.rooms.value.filter(
        (candidate) => candidate.account_id === accountId,
      )
      const resolution = resolveRoomTarget(visibleRooms, command.target)
      if (resolution.kind === 'missing') {
        commandTimeline.error.value = `room not found: ${command.target}`
        return false
      }
      if (resolution.kind === 'ambiguous') {
        commandTimeline.error.value = `room name is ambiguous: ${resolution.options.join(', ')}`
        return false
      }
      location.route(
        `/${resolution.room.account_id}/rooms/${encodeURIComponent(resolution.room.room_id)}`,
      )
      // The composer is keyed by room id, so the new room mounts a fresh one —
      // hand it the keyboard so the user can keep typing. Deferred past the
      // mount, the same reason RoomList's `stepRoom` defers: a bump before the
      // new composer mounts starts as its focus baseline and is swallowed.
      setTimeout(() => {
        composerFocus.value += 1
      })
      return true
    }
    if (command.kind === 'leave' || command.kind === 'forget') {
      return handleMembershipCommand(command.kind)
    }
    if (command.kind === 'join') {
      const target = command.target
      if (target === null) {
        location.route('/rooms/discover')
        return true
      }
      return handleRoomEntryCommand({ kind: 'join', target })
    }
    if (command.kind === 'dm') {
      const target = command.target
      if (target === null) {
        location.route('/rooms/dm')
        return true
      }
      return handleDmCommand(target)
    }
    if (command.kind === 'create') {
      location.route('/rooms/create#create')
      return true
    }
    if (command.kind === 'find') {
      location.route('/rooms/discover#find')
      return true
    }
    if (command.kind === 'invite') {
      return handleInviteCommand(command.users)
    }
    if (command.kind === 'cancel-invite') {
      return handleCancelInviteCommand(command.user)
    }
    if (command.kind === 'knock') {
      return handleRoomEntryCommand(command)
    }
    if (command.kind === 'pin' || command.kind === 'unpin') {
      if (command.target === null) {
        const key = roomKey({ account_id: accountId, room_id: roomId })
        if (command.kind === 'pin') {
          settings.pinRoom(key)
        } else {
          settings.unpinRoom(key)
        }
        return true
      }
      const targetRoom = resolveCommandRoom(accountRooms, command.target)
      if (targetRoom === null) {
        commandTimeline.error.value = `room not found: ${command.target}`
        return false
      }
      if (typeof targetRoom === 'string') {
        commandTimeline.error.value = targetRoom
        return false
      }
      const key = roomKey(targetRoom)
      if (command.kind === 'pin') {
        settings.pinRoom(key)
      } else {
        settings.unpinRoom(key)
      }
      return true
    }
    if (command.kind === 'thread' && !allowThreadCommand) {
      return true
    }
    const target = latestTarget()
    if (target === null) {
      commandTimeline.error.value = 'no message available for command'
      return false
    }
    if (command.kind === 'reply') {
      if (command.body === null) {
        setCommandAction({ kind: 'reply', event: target })
      } else {
        setCommandAction(null)
        void (async () => {
          const formatted = await commandFormatComposerBody(command.body!)
          void commandTimeline
            .send(formatted.body, {
              replyTo: target.event_id,
              senderId: ownUserId ?? undefined,
              formattedBody: formatted.formatted_body ?? null,
            })
            .then((ok) => {
              if (ok) onMutation()
            })
        })()
      }
      return true
    }
    if (command.kind === 'thread') {
      setThreadParam(target.event_id)
      return true
    }
    if (command.reaction === null) {
      setCommandReactionPickerEventId(target.event_id)
      return true
    }
    const key = resolveReactionCommandKey(command.reaction, emojiEntries)
    if (key === null) {
      commandTimeline.error.value = `unknown reaction shortcode: ${command.reaction}`
      return false
    }
    settings.recordRecentReaction(key)
    void commandTimeline.toggleReaction(target, key).then((ok) => {
      if (ok) onMutation()
    })
    return true
  }

  const handleComposerCommand = (body: string): boolean | Promise<boolean> =>
    handleComposerCommandFor({
      body,
      timeline,
      visible,
      isVisible: isVisibleTimelineEvent,
      action,
      setAction,
      setReactionPickerEventId,
      formatComposerBody,
      allowThreadCommand: true,
      onMutation: search.clear,
    })

  const handleThreadComposerCommand = (
    body: string,
    commandTimeline: TimelineStore,
    commandVisible: readonly TimelineEvent[],
    isCommandVisible: (event: TimelineEvent) => boolean,
    commandAction: ComposerAction | null,
    setCommandAction: (action: ComposerAction | null) => void,
    setCommandReactionPickerEventId: (eventId: string | null) => void,
    commandFormatComposerBody: typeof formatComposerBody,
  ): boolean | Promise<boolean> =>
    handleComposerCommandFor({
      body,
      timeline: commandTimeline,
      visible: commandVisible,
      isVisible: isCommandVisible,
      action: commandAction,
      setAction: setCommandAction,
      setReactionPickerEventId: setCommandReactionPickerEventId,
      formatComposerBody: commandFormatComposerBody,
      allowThreadCommand: false,
      onMutation: search.clear,
    })

  const handleMembershipCommand = async (
    action: 'leave' | 'forget',
  ): Promise<boolean> => {
    timeline.error.value = null
    if (action === 'leave' && !(await confirmLeaveIfOnlyJoinedMember())) {
      return false
    }
    const result =
      action === 'leave'
        ? await rooms.leaveRoom(accountId, roomId)
        : await rooms.forgetRoom(accountId, roomId)
    if (!result.ok) {
      timeline.error.value = result.message
      return false
    }
    location.route('/', true)
    return true
  }

  const handleRoomEntryCommand = async (
    command:
      | { kind: 'join'; target: string }
      | { kind: 'knock'; target: string; reason: string | null },
  ): Promise<boolean> => {
    timeline.error.value = null
    const reference = parseMatrixRoomReference(command.target, {
      allowAliasShorthand: true,
      defaultAliasServerName:
        serverNameFromUserId(ownUserId) ?? serverNameFromRoomReference(roomId),
    })
    if (reference === null) {
      timeline.error.value = `usage: ${slashCommandUsage(
        command.kind === 'join' ? SLASH_COMMAND.join : SLASH_COMMAND.knock,
      )}`
      return false
    }
    setRoomEntryStatus(
      roomEntryPendingMessage(command.kind, reference.roomIdOrAlias),
    )
    let result: Awaited<ReturnType<typeof rooms.joinRoom>>
    try {
      result = await (command.kind === 'join'
        ? rooms.joinRoom(
            accountId,
            reference.roomIdOrAlias,
            reference.serverNames,
          )
        : rooms.knockRoom(
            accountId,
            reference.roomIdOrAlias,
            command.reason,
            reference.serverNames,
          ))
    } finally {
      setRoomEntryStatus(null)
    }
    if (!result.ok) {
      timeline.error.value = roomEntryFailureMessage(result.message)
      return false
    }
    if (command.kind === 'join') {
      location.route(localRoomHref(accountId, result.roomId, reference.eventId))
    }
    return true
  }

  const handleDmCommand = async (target: string): Promise<boolean> => {
    timeline.error.value = null
    const { currentOwnUserId, defaultServerName } = currentUserCommandContext()
    const userId = normalizeUserId(target, defaultServerName)
    if (userId === null) {
      timeline.error.value = `usage: ${slashCommandUsage(SLASH_COMMAND.dm)}`
      return false
    }
    if (userId === currentOwnUserId) {
      timeline.error.value = 'Select another user to start a direct message.'
      return false
    }
    setRoomEntryStatus(`Opening DM with ${userId}…`)
    let result: Awaited<ReturnType<typeof rooms.createDm>>
    try {
      result = await rooms.createDm(accountId, userId)
    } finally {
      setRoomEntryStatus(null)
    }
    if (!result.ok) {
      timeline.error.value = result.message
      return false
    }
    location.route(localRoomHref(accountId, result.roomId, null))
    return true
  }

  const handleInviteCommand = async (input: string): Promise<boolean> => {
    timeline.error.value = null
    const { currentOwnUserId, defaultServerName } = currentUserCommandContext()
    const invite = parseUserIdList(input, defaultServerName)
    if (!invite.ok) {
      timeline.error.value = invite.message
      return false
    }
    if (invite.userIds.length === 0) {
      timeline.error.value = `usage: ${slashCommandUsage(SLASH_COMMAND.invite)}`
      return false
    }
    if (
      currentOwnUserId !== null &&
      invite.userIds.some((userId) => userId === currentOwnUserId)
    ) {
      timeline.error.value = 'Invite another user, not yourself.'
      return false
    }
    setRoomEntryStatus(`Inviting ${invite.userIds.join(', ')}…`)
    let result: Awaited<ReturnType<typeof members.inviteUsers>>
    try {
      result = await members.inviteUsers(invite.userIds)
    } finally {
      setRoomEntryStatus(null)
    }
    if (!result.ok) {
      timeline.error.value = inviteErrorMessage(result)
      return false
    }
    return true
  }

  const handleCancelInviteCommand = async (input: string): Promise<boolean> => {
    timeline.error.value = null
    const { currentOwnUserId, defaultServerName } = currentUserCommandContext()
    const userId = normalizeUserId(input, defaultServerName)
    if (userId === null) {
      timeline.error.value = `usage: ${slashCommandUsage(SLASH_COMMAND.cancel)}`
      return false
    }
    if (userId === currentOwnUserId) {
      timeline.error.value = 'Select another user to cancel an invite.'
      return false
    }
    await members.refresh()
    if (members.error.value !== null) {
      timeline.error.value = `could not refresh room members: ${members.error.value}`
      return false
    }
    const member = members.members.value.get(userId)
    if (member?.membership !== 'invite') {
      timeline.error.value = `No pending invite for ${userId}.`
      return false
    }
    setRoomEntryStatus(`Canceling invite for ${userId}…`)
    let result: Awaited<ReturnType<typeof members.cancelInvite>>
    try {
      result = await members.cancelInvite(userId)
    } finally {
      setRoomEntryStatus(null)
    }
    if (!result.ok) {
      timeline.error.value = `Could not cancel invite for ${userId}: ${result.message}`
      return false
    }
    return true
  }

  const currentRoomOwnUserId = (): string | null =>
    rooms.rooms.value.find(
      (candidate) =>
        candidate.account_id === accountId && candidate.room_id === roomId,
    )?.account_user_id ??
    ownUserId ??
    null

  const currentUserCommandContext = (): {
    currentOwnUserId: string | null
    defaultServerName: string | null
  } => {
    const currentOwnUserId = currentRoomOwnUserId()
    return {
      currentOwnUserId,
      defaultServerName:
        serverNameFromUserId(currentOwnUserId) ??
        serverNameFromRoomReference(roomId),
    }
  }

  async function confirmLeaveIfOnlyJoinedMember(): Promise<boolean> {
    if (ownUserId === null) {
      timeline.error.value = 'room membership is still loading; try again'
      return false
    }
    await members.refresh()
    if (members.error.value !== null) {
      timeline.error.value = `could not refresh room members: ${members.error.value}`
      return false
    }
    if (!isOnlyJoinedMember(members.members.value.values(), ownUserId)) {
      return true
    }
    return window.confirm(LAST_JOINED_MEMBER_LEAVE_CONFIRM)
  }

  const setThreadParam = useCallback(
    (rootId: string | null) => {
      if (rootId !== null) {
        setRoomInfoOpen(false)
      }
      const base = location.path
      location.route(
        rootId === null ? base : `${base}?thread=${encodeURIComponent(rootId)}`,
        true,
      )
    },
    [location],
  )

  const openRoomInfo = useCallback(() => {
    setThreadParam(null)
    setRoomInfoOpen(true)
  }, [setThreadParam])

  useEffect(() => {
    setRoomChrome(title, openRoomInfo)
    return () => setRoomChrome(null, null)
  }, [openRoomInfo, setRoomChrome, title])

  const navigateBackOneMobilePane = () => {
    perfMark('room-page:mobile-back', {
      target: openThread !== null ? 'thread-close' : 'room-list',
    })
    if (openThread !== null) {
      setThreadParam(null)
    } else {
      perfMarkFrames('room-page:before-route-room-list')
      location.route('/')
    }
  }

  const handleTouchStart = (event: JSX.TargetedTouchEvent<HTMLDivElement>) => {
    swipeLocked.current = false
    const touch = event.touches[0]
    if (
      !window.matchMedia(SINGLE_PANE_QUERY).matches ||
      event.touches.length !== 1 ||
      // Guarded by the length check above, so `touch` is present here.
      touch.clientX < NATIVE_BACK_EDGE_PX ||
      isGestureControl(event.target) ||
      isHorizontallyScrollable(event.target)
    ) {
      swipeStart.current = null
      return
    }
    swipeStart.current = { x: touch.clientX, y: touch.clientY }
  }

  /**
   * Claims the gesture as soon as it looks like a rightward swipe, well
   * before the touch travels far enough to satisfy the touchend thresholds.
   * `preventDefault` here suppresses the scrolling and text selection that
   * would otherwise fight our pan; CSS `touch-action` can't do that without
   * also disabling touch-scrolling for the code blocks/tables nested inside
   * the timeline (both need real horizontal panning), so this has to be a
   * targeted, per-gesture opt-out instead.
   *
   * What it does *not* do is stop the browser's native edge-swipe-back —
   * that recognizer ignores cancelled touch events, which is why
   * `handleTouchStart` declines the edge band outright (ADR 0075).
   */
  const handleTouchMove = (event: JSX.TargetedTouchEvent<HTMLDivElement>) => {
    const start = swipeStart.current
    if (start === null || event.touches.length !== 1) {
      return
    }
    if (swipeLocked.current) {
      event.preventDefault()
      return
    }
    const touch = event.touches[0]
    const dx = touch.clientX - start.x
    const dy = touch.clientY - start.y
    const absX = Math.abs(dx)
    const absY = Math.abs(dy)
    if (absX < SWIPE_DECISION_THRESHOLD && absY < SWIPE_DECISION_THRESHOLD) {
      return
    }
    if (dx > 0 && absX > absY * SWIPE_RIGHT_AXIS_RATIO) {
      swipeLocked.current = true
      event.preventDefault()
    } else {
      // Vertical scroll or a leftward drag: not ours, leave it to the browser.
      swipeStart.current = null
    }
  }

  const handleTouchEnd = (event: JSX.TargetedTouchEvent<HTMLDivElement>) => {
    const start = swipeStart.current
    swipeStart.current = null
    swipeLocked.current = false
    if (
      start === null ||
      !window.matchMedia(SINGLE_PANE_QUERY).matches ||
      event.changedTouches.length === 0
    ) {
      return
    }
    const touch = event.changedTouches[0]
    const dx = touch.clientX - start.x
    const dy = touch.clientY - start.y
    const absY = Math.abs(dy)
    if (
      dx < SWIPE_RIGHT_MIN_X ||
      absY > SWIPE_RIGHT_MAX_Y ||
      dx < absY * SWIPE_RIGHT_AXIS_RATIO
    ) {
      return
    }
    perfMark('room-page:swipe-right-accepted', { dx, dy })
    navigateBackOneMobilePane()
  }

  const roomCompletions = (query: string): ComposerAutocompleteOption[] => {
    const visibleRooms = rooms.rooms.value.filter(
      (candidate) => candidate.account_id === accountId,
    )
    return roomCommandSuggestions(visibleRooms, query)
      .slice(0, 8)
      .map((suggestion) => ({
        value: suggestion.completion,
        label: suggestion.completion,
        description:
          suggestion.detail === suggestion.completion
            ? suggestion.matched
            : suggestion.detail,
      }))
  }

  const pinLiveTimelineAfterComposerFocus = () => {
    if (highlighted !== null || !timeline.atEnd.value) {
      return
    }
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        const scroller = document.querySelector<HTMLElement>(
          '.room-stream > .timeline',
        )
        if (scroller !== null) {
          scroller.scrollTop = scroller.scrollHeight
        }
      })
    })
  }

  /**
   * The staged Escape (ADR 0078). The edit-history modal claims it first in the
   * capture phase, and the focused composer claims it to cancel its own banner.
   * What is left lands here: cancel a reply/edit, else close the thread panel,
   * and either way hand the keyboard back to the composer.
   *
   * The banner is cancelled here as well as in the composer because focus is
   * briefly on `<body>` while the composer remounts into edit mode — an Escape
   * in that window would otherwise skip past the banner and close the thread.
   */
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        if (action !== null) {
          setAction(null)
        } else if (roomInfoOpen) {
          setRoomInfoOpen(false)
        } else if (openThread !== null) {
          setThreadParam(null)
        }
        composerFocus.value += 1
      },
    },
    // Escape has to reach us from inside the composer too — that is where the
    // TUI closes the thread panel from. A banner-cancelling composer, and the
    // room list, both claim the event first by preventing its default.
    { whileTyping: true },
  )

  /** `ArrowUp` on an empty composer edits the newest message we can edit. */
  const editLast = () => {
    for (let i = timeline.events.value.length - 1; i >= 0; i -= 1) {
      const event = timeline.events.value[i]
      if (isEditable(event, ownUserId)) {
        setAction({ kind: 'edit', event })
        return
      }
    }
  }

  return (
    <div class="page room-page">
      <header class="room-head">
        <h1 ref={heading} tabIndex={-1}>
          <button
            type="button"
            class="room-title-button"
            title={`Room information (${SLASH_COMMAND.whereami})`}
            aria-label="Open room information"
            aria-expanded={roomInfoOpen}
            aria-controls="room-info-panel"
            onClick={openRoomInfo}
          >
            <span>{title}</span>
          </button>
        </h1>
      </header>

      <ErrorBanner error={timeline.error} />

      {/* Row, so the thread panel sits beside the stream and shrinks it
          rather than covering it (ADR 0062). Below the three-pane breakpoint
          CSS reverts the panel to an overlay drawer. */}
      <div
        class="room-body"
        onTouchStart={handleTouchStart}
        onTouchMove={handleTouchMove}
        onTouchEnd={handleTouchEnd}
        onTouchCancel={() => {
          swipeStart.current = null
          swipeLocked.current = false
        }}
      >
        {/* Drop scoped to this pane, not the page: a file dropped on the thread
            panel beside it must stage there, not here (ADR 0065). */}
        <div
          ref={roomStream}
          class="room-stream"
          {...(attachable ? dropHandlers : {})}
        >
          {dragging && (
            <div class="drop-overlay" aria-hidden="true">
              <p>Drop to attach</p>
            </div>
          )}
          {timeline.loading.value ? (
            <p>Loading messages…</p>
          ) : (
            <MediaViewerProvider
              accountId={accountId}
              events={visible}
              onLoadOlder={() => timeline.loadOlder()}
              atStart={timeline.atStart.value}
              findRow={(eventId) => findEventRow(roomStream.current, eventId)}
              runOf={(eventId) => runPosition(rows, eventId)}
              actions={{
                ownUserId,
                onReply: (event) => setAction({ kind: 'reply', event }),
                onReact: (event) => setReactionPickerEventId(event.event_id),
                onOpenThread: (event) => setThreadParam(event.event_id),
                onDelete: async (event) => {
                  const deleted = await timeline.redact(event.event_id)
                  if (deleted) search.clear()
                  return deleted
                },
              }}
            >
              <Timeline
                rows={rows}
                timeline={timeline}
                threads={threads}
                threadUnread={threadUnread}
                members={members}
                visible={visible}
                accountId={accountId}
                ownUserId={ownUserId}
                ephemeral={ephemeral}
                highlighted={highlighted}
                dateJumpStart={dateJumpStart}
                onDateJumpAnchored={() => setDateJumpStart(null)}
                settings={settings}
                reactionPickerEventId={reactionPickerEventId}
                onSetReactionPicker={setReactionPickerEventId}
                onReply={(event) => setAction({ kind: 'reply', event })}
                onEdit={(event) => setAction({ kind: 'edit', event })}
                onOpenThread={setThreadParam}
                onMutation={search.clear}
              />
            </MediaViewerProvider>
          )}

          {typingText !== null && (
            <p class="typing-indicator" aria-live="polite">
              {typingText}
            </p>
          )}

          <Composer
            // Scoped to the account *and* room: this page is not remounted
            // when the route changes, so an unkeyed composer would carry room
            // A's typing into room B (and send it there on Enter) — including
            // the same room viewed through a different account.
            key={
              action?.kind === 'edit'
                ? action.event.event_id
                : `send:${accountId}:${roomId}`
            }
            placeholder={
              composerLabelTitle === null
                ? 'Message'
                : `Message ${composerLabelTitle}`
            }
            ariaLabel={
              composerLabelTitle === null
                ? 'Message'
                : `Message ${composerLabelTitle}`
            }
            status={roomEntryStatus ?? undefined}
            banner={composerBanner}
            onEditLast={editLast}
            onFocus={pinLiveTimelineAfterComposerFocus}
            focusRequest={composerFocus.value}
            height={settings.messageComposerHeight.value}
            onHeightChange={(height) =>
              (settings.messageComposerHeight.value = height)
            }
            initialValue={
              action?.kind === 'edit'
                ? (action.draft ?? action.event.body ?? '')
                : deviceState.draft(accountId, roomId)
            }
            onDraftChange={
              action?.kind === 'edit'
                ? undefined
                : (text) => {
                    deviceState.setDraft(accountId, roomId, text)
                    // `onDraftChange` is not called for slash-command drafts
                    // (Composer suppresses them), so a non-empty text here is
                    // genuine composition worth a typing notice. An empty text
                    // clears it — this is also the send path, since
                    // `Composer.submit` fires `onDraftChange('')` before
                    // `onSubmit`.
                    if (text === '') {
                      ephemeralSender.stopTyping(accountId, roomId)
                    } else {
                      ephemeralSender.noteTyping(accountId, roomId)
                    }
                  }
            }
            onCommand={handleComposerCommand}
            roomCompletions={roomCompletions}
            mentionCompletions={mentionCompletions}
            roomReferenceCompletions={roomReferenceCompletions}
            emojiCompletions={emojiCompletions}
            onAttach={attachable ? stage : undefined}
            attachments={{ ...attachments, onRemove: removeAttachment }}
            onSubmit={submitMessage}
          />
        </div>

        {openThread !== null && !roomInfoOpen && (
          <ThreadPanel
            key={`${accountId}:${roomId}:${openThread}`}
            accountId={accountId}
            roomId={roomId}
            members={members}
            rooms={rooms.rooms.value.filter(
              (candidate) => candidate.account_id === accountId,
            )}
            roomTitles={rooms.titles.value}
            rootId={openThread}
            rootEvent={
              threads.roots.value.get(openThread) ??
              timeline.events.value.find((e) => e.event_id === openThread)
            }
            ownUserId={ownUserId}
            threadUnread={threadUnread}
            mayNameRoomReceipt={mayNameRoomReceiptFromThread}
            receiptCeiling={threadReceiptCeiling}
            onCommand={handleThreadComposerCommand}
            roomCompletions={roomCompletions}
            onClose={() => setThreadParam(null)}
          />
        )}
        {roomInfoOpen && (
          <RoomInfoPanel
            accountId={accountId}
            roomId={roomId}
            room={room}
            roomTitles={rooms.titles.value}
            members={members}
            onClose={() => setRoomInfoOpen(false)}
          />
        )}
      </div>
      {jumpOpen && (
        <JumpDialog
          onClose={() => setJumpOpen(false)}
          onJump={(startTs, endTs) => {
            setDateJumpStart(startTs)
            void timeline.jumpToDate(startTs, endTs, isVisibleTimelineEvent)
          }}
        />
      )}
    </div>
  )
}

function Timeline({
  rows,
  timeline,
  threads,
  threadUnread,
  members,
  visible,
  accountId,
  ownUserId,
  ephemeral,
  highlighted,
  dateJumpStart,
  onDateJumpAnchored,
  settings,
  reactionPickerEventId,
  onSetReactionPicker,
  onReply,
  onEdit,
  onOpenThread,
  onMutation,
}: {
  timeline: TimelineStore
  threads: ThreadsStore
  threadUnread: ThreadUnreadStore
  members: MembersStore
  /** Grouped by `groupMediaRuns` in the parent, so the media viewer and the
   *  timeline share one grouping pass (ADR 0081). */
  rows: TimelineRow[]
  visible: TimelineEvent[]
  accountId: string
  ownUserId: string | null
  ephemeral: EphemeralStore
  highlighted: string | null
  dateJumpStart: number | null
  onDateJumpAnchored: () => void
  settings: SettingsStore
  reactionPickerEventId: string | null
  onSetReactionPicker: (eventId: string | null) => void
  onReply: (event: EventDto) => void
  onEdit: (event: EventDto) => void
  onOpenThread: (rootId: string) => void
  onMutation: () => void
}) {
  const topSentinel = useRef<HTMLDivElement>(null)
  const bottomSentinel = useRef<HTMLDivElement>(null)
  const scroller = useRef<HTMLDivElement>(null)
  const eventList = useRef<HTMLOListElement>(null)
  const lastOwnEventId = useRef<string | null>(null)
  const stickToBottom = useRef(true)
  /**
   * Whether the reader was at the live end the instant they focused the
   * composer — captured before the keyboard moves anything, so it survives
   * the race below. Bringing up the keyboard forces a scroll (the composer's
   * own focus handler re-pins immediately, and iOS may nudge the scroller
   * trying to keep the focused control in view); if either lands while the
   * keyboard's own resize is still settling, `onScroll` reads a transient
   * `clientHeight` and can flip `stickToBottom` false for a container that
   * never actually left the bottom. This ref is the fix: the keyboard-resize
   * observer below trusts it over a `stickToBottom` that just got clobbered.
   *
   * Left set (not consumed by the first resize) for `KEYBOARD_PIN_MS` after
   * focus, not cleared by the next `scroller-resize` callback: the keyboard's
   * show animation fires that observer more than once — an unrelated content
   * reflow, then the real shrink — and clearing on the first occurrence burned
   * the override before the real one arrived (caught live on a phone: a
   * `keyboardPin=true` mark followed by a `keyboardPin=false` one on the very
   * next resize, before the keyboard had actually finished shrinking anything).
   */
  const keyboardPin = useRef(false)
  const keyboardPinTimer = useRef<number | null>(null)
  const resizePinFrame = useRef<number | null>(null)
  const highlightedCentering = useRef<{
    eventId: string | null
    active: boolean
  }>({ eventId: null, active: false })
  const [actionsOpenEventId, setActionsOpenEventId] = useState<string | null>(
    null,
  )

  /**
   * The row the reader's eye is on: where it sat relative to the scroller's
   * top edge, and what the scroll offset was at the time. Holding *both* is
   * what makes the measurement independent of the reader's own scrolling —
   * see the anchoring effect below.
   */
  const scrollAnchor = useRef<{
    row: HTMLElement
    top: number
    scrollTop: number
  } | null>(null)

  /**
   * Take a fresh anchor: the topmost fully visible row, its offset, and the
   * scroll position it was measured at.
   *
   * Deliberately *not* called from the scroll handler. Reading geometry forces
   * layout, and layout is where `content-visibility` decides to render the
   * rows coming into view — so a capture during a scroll triggers the very
   * growth it is meant to measure and records the position after it. That is
   * what left the correction silent through an entire scroll-back while
   * working perfectly on a still timeline. The baseline survives the reader's
   * own scrolling instead (see the effect below), so it only has to be
   * retaken when it stops describing anything useful.
   *
   * Rows come in document order, so the predicates below are monotonic across
   * them: a binary search settles it in ~10 rect reads rather than walking a
   * slice that holds hundreds.
   */
  const captureAnchor = useCallback(() => {
    const el = scroller.current
    const list = eventList.current
    if (el === null || list === null) {
      scrollAnchor.current = null
      return
    }
    const rows = list.querySelectorAll<HTMLElement>('li.event-row')
    const top = el.getBoundingClientRect().top
    /** First row satisfying a predicate that is monotonic in document order. */
    const search = (matches: (row: HTMLElement) => boolean) => {
      let low = 0
      let high = rows.length - 1
      let found = -1
      while (low <= high) {
        const mid = (low + high) >> 1
        if (matches(rows[mid])) {
          found = mid
          high = mid - 1
        } else {
          low = mid + 1
        }
      }
      return found
    }
    // The first row entirely at or below the top edge — *not* the one
    // straddling it. During a scroll-back the straddling row is the one being
    // revealed and rendered for the first time, so it is the likeliest to
    // correct its own height; anchoring to it would measure its top, which a
    // downward growth never moves, and miss the shift entirely. Anchoring
    // below it puts that growth above the anchor, where it is compensated
    // like any other. Only a row taller than the viewport leaves nothing
    // fully visible, and then the straddling row is all there is.
    const fullyVisible = search((row) => row.getBoundingClientRect().top >= top)
    const found =
      fullyVisible === -1
        ? search((row) => row.getBoundingClientRect().bottom > top)
        : fullyVisible
    scrollAnchor.current =
      found === -1
        ? null
        : {
            row: rows[found],
            top: rows[found].getBoundingClientRect().top - top,
            scrollTop: el.scrollTop,
          }
  }, [])

  const scheduleResizePin = useCallback(() => {
    if (resizePinFrame.current !== null) {
      return
    }
    resizePinFrame.current = requestAnimationFrame(() => {
      resizePinFrame.current = null
      const el = scroller.current
      if (el !== null && stickToBottom.current) {
        scrollTimelineToBottom(el, stickToBottom)
      }
    })
  }, [])

  useEffect(
    () => () => {
      if (resizePinFrame.current !== null) {
        cancelAnimationFrame(resizePinFrame.current)
      }
    },
    [],
  )

  useLayoutEffect(() => {
    if (highlightedCentering.current.eventId !== highlighted) {
      highlightedCentering.current = {
        eventId: highlighted,
        active: highlighted !== null,
      }
    }
  }, [highlighted])

  const stopHighlightedCentering = useCallback(() => {
    highlightedCentering.current.active = false
  }, [])

  // Infinite scroll-back: fetch the next older page when the top sentinel
  // becomes visible. jsdom has no IntersectionObserver; the button below is
  // both the fallback and the testable path.
  useEffect(() => {
    if (typeof IntersectionObserver === 'undefined') {
      return
    }
    const sentinel = topSentinel.current
    const root = scroller.current
    if (sentinel === null) {
      return
    }
    let cancelled = false
    let pumping = false
    let requested = false
    let frame = 0

    // One trigger is not one page: a page of state events, redactions, or
    // thread replies adds no height, so the sentinel never leaves the band
    // and nothing would ask again. The chain keeps paging while the sentinel
    // is in the band, up to a cap per chain — past that an idle reader stops
    // pulling history, but any further scroll starts a new chain.
    //
    // The cap therefore bounds an *unattended* chain, not a session: a reader
    // who keeps scrolling walks as far back as they keep asking for, which is
    // the intent. What bounds memory is the store's retained-slice trim.
    const pump = async () => {
      if (timeline.atStart.value) {
        // Nothing left to page. Bailing before the chain starts, rather than
        // inside it, keeps a reader parked at the beginning of a room from
        // opening (and marking) a chain on every scroll frame.
        return
      }
      if (pumping) {
        // Remember the trigger rather than dropping it. A scroll that lands
        // mid-page used to be lost, and if it was the last one the reader
        // made, the chain ended with nothing left to wake it.
        requested = true
        return
      }
      pumping = true
      perfMark('timeline:auto-page:chain-start', {
        loaded: timeline.events.value.length,
      })
      try {
        do {
          requested = false
          for (let page = 0; page < AUTO_SCROLL_BACK_PAGES; page += 1) {
            const stop = (reason: string) =>
              perfMark('timeline:auto-page:stop', {
                reason,
                page,
                loaded: timeline.events.value.length,
                atStart: timeline.atStart.value,
                atEnd: timeline.atEnd.value,
              })
            if (cancelled) {
              return
            }
            if (timeline.atStart.value) {
              stop('at-start')
              return
            }
            if (!withinPrefetchBand(sentinel, root)) {
              stop('out-of-band')
              return
            }
            perfMark('timeline:auto-page:fetch', {
              page,
              loaded: timeline.events.value.length,
            })
            const advanced = await timeline.loadOlder()
            perfMark('timeline:auto-page:fetched', {
              page,
              advanced,
              loaded: timeline.events.value.length,
            })
            if (!advanced) {
              // The cursor did not move: end of history, a failed request, or
              // a page dropped as stale. Retrying now would spin.
              stop('no-progress')
              return
            }
            if (page + 1 >= AUTO_SCROLL_BACK_PAGES) {
              stop('cap')
              break
            }
            // Let layout — and the browser's scroll anchoring — settle before
            // asking where the sentinel ended up.
            await nextFrame()
          }
          // A trigger that arrived mid-chain gets its answer here, so the
          // chain can never end while the reader is still asking for more.
        } while (requested && !cancelled)
      } finally {
        pumping = false
        perfMark('timeline:auto-page:chain-end', {
          loaded: timeline.events.value.length,
        })
      }
    }

    // Two triggers, deliberately. The observer catches the band being entered
    // without a scroll (content shrinking above, a first paint already at the
    // top); the scroll listener is what makes the chain recoverable, since it
    // reports the reader's intent every frame rather than only on an
    // intersection *change* the browser may never have cause to report again.
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          void pump()
        }
      },
      // The root must be the scroller, not the viewport: `rootMargin` grows
      // the *root's* rect, while an intermediate scroll container clips the
      // target before the root is ever consulted — margin against the default
      // root would do nothing here. With it, a page starts loading before the
      // reader reaches the top, so it lands during the scroll, not after it.
      { root, rootMargin: `${SCROLL_BACK_PREFETCH_PX}px 0px 0px 0px` },
    )
    observer.observe(sentinel)
    const onScroll = () => {
      // One measurement per frame, however many scroll events arrive.
      if (frame !== 0) {
        return
      }
      frame = requestAnimationFrame(() => {
        frame = 0
        if (withinPrefetchBand(sentinel, root)) {
          void pump()
        }
      })
    }
    root?.addEventListener('scroll', onScroll, { passive: true })
    return () => {
      cancelled = true
      observer.disconnect()
      root?.removeEventListener('scroll', onScroll)
      if (frame !== 0) {
        cancelAnimationFrame(frame)
      }
    }
  }, [timeline])

  // The forward mirror, live only after a jump: the bottom sentinel exists
  // only while the slice is parked in history (`!atEnd`), so the effect
  // re-runs on that flip — a mount-only observer would miss the ref.
  const atEnd = timeline.atEnd.value
  useEffect(() => {
    if (typeof IntersectionObserver === 'undefined' || atEnd) {
      return
    }
    const sentinel = bottomSentinel.current
    if (sentinel === null) {
      return
    }
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        void timeline.loadNewer()
      }
    })
    observer.observe(sentinel)
    return () => observer.disconnect()
  }, [timeline, atEnd])

  // Each fresh page (initial load, date jump, post-send reload) remounts
  // this component (RoomPage swaps it for the "Loading…" text meanwhile),
  // so a mount-time scroll-to-bottom lands on the newest/most-relevant end
  // of whatever page just loaded without disturbing scroll-back position.
  useEffect(() => {
    const el = scroller.current
    if (el !== null && dateJumpStart === null) {
      scrollTimelineToBottom(el, stickToBottom)
    }
    // A jumped page also anchors on its tail (the jump target), but must not
    // *pin* there: the scroller's bottom is history, not the room's tail, and
    // a latched pin would yank the view on the next atEnd flip.
    if (!timeline.atEnd.value) {
      stickToBottom.current = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- once per mount
  }, [])

  useLayoutEffect(() => {
    if (dateJumpStart === null) {
      return
    }
    const target =
      visible.find((event) => event.origin_ts >= dateJumpStart) ??
      visible.at(-1)
    if (target === undefined) {
      onDateJumpAnchored()
      return
    }
    if (centerHighlightedRow(scroller.current, target.event_id)) {
      stickToBottom.current = false
      onDateJumpAnchored()
    }
  }, [dateJumpStart, visible, onDateJumpAnchored])

  // A reaction changes an existing row's height, so the event count does not
  // change and the "new own message" scroll path below never runs. Preserve
  // bottom pinning across every visible-event update. The "was pinned" state is
  // maintained by scroll events below; measuring it in a layout-effect cleanup
  // is too late in Chromium for a row-height change, because the new reaction
  // may already have increased `scrollHeight`.
  useLayoutEffect(() => {
    // `atEnd` gates every pin: after a jump the scroller's bottom is not the
    // room's tail, and re-pinning on each `loadNewer` append would chase the
    // view back to the present one page at a time.
    const el = scroller.current
    if (el !== null && stickToBottom.current && highlighted === null && atEnd) {
      scrollTimelineToBottom(el, stickToBottom)
    }
  }, [visible, highlighted, atEnd])

  // Thread summaries arrive after the room timeline and add "N replies"
  // buttons to existing rows. That height change can happen after the initial
  // bottom pin, so preserve the live-end pin when summaries hydrate.
  useLayoutEffect(() => {
    const el = scroller.current
    if (el !== null && stickToBottom.current && highlighted === null && atEnd) {
      scrollTimelineToBottom(el, stickToBottom)
    }
  }, [threads.summaries.value, highlighted, atEnd])

  // Media, formatted HTML, and other room-specific content can change row
  // heights after the initial page render. Keep a newly opened room pinned to
  // its newest visible event while layout settles, but stop as soon as the user
  // scrolls away from the bottom.
  useLayoutEffect(() => {
    if (
      highlighted !== null ||
      !atEnd ||
      typeof ResizeObserver === 'undefined'
    ) {
      return
    }
    const list = eventList.current
    if (list === null) {
      return
    }
    const observer = new ResizeObserver(() => {
      if (stickToBottom.current) {
        scheduleResizePin()
      }
    })
    observer.observe(list)
    return () => observer.disconnect()
  }, [highlighted, atEnd, scheduleResizePin])

  // Snapshot "was the reader at the live end" the instant the composer takes
  // focus — before the keyboard's own resize (and the composer's separate
  // focus-triggered pin) have a chance to fire an in-between scroll event
  // that miscomputes `stickToBottom` (see `keyboardPin` above).
  useEffect(() => {
    const clearPinTimer = () => {
      if (keyboardPinTimer.current !== null) {
        window.clearTimeout(keyboardPinTimer.current)
        keyboardPinTimer.current = null
      }
    }
    const onFocusIn = (event: FocusEvent) => {
      // Only the *room* composer arms the pin. The listener has to sit on
      // `document` (focus lands before the composer's own handlers run), but
      // `RoomPage` does not remount on in-room navigation, so matching any text
      // control would let unrelated overlays arm it. `JumpDialog`'s date inputs
      // are the damaging case: a resize inside the pin window then force-pins
      // to the live end, which is the exact opposite of what "jump to date" is
      // for.
      const target = event.target
      if (!(target instanceof HTMLElement)) {
        return
      }
      const composer = target.closest('.composer')
      if (composer === null) {
        return
      }
      // ...and only the composer belonging to *this* timeline's pane, so the
      // room timeline is never re-pinned by focus in the thread panel's
      // composer, or vice versa. `ThreadPanel` doesn't render its own
      // `Timeline`/keyboardPin instance today, so `.thread-panel` has no live
      // counterpart to guard against yet — this is forward-looking, for
      // whenever it does.
      const pane = (node: Element | null | undefined) =>
        node?.closest('.room-stream, .thread-panel') ?? null
      const ownPane = pane(scroller.current)
      if (ownPane === null || pane(composer) !== ownPane) {
        return
      }
      keyboardPin.current = stickToBottom.current
      clearPinTimer()
      keyboardPinTimer.current = window.setTimeout(() => {
        keyboardPin.current = false
      }, KEYBOARD_PIN_MS)
    }
    const onFocusOut = () => {
      keyboardPin.current = false
      clearPinTimer()
    }
    document.addEventListener('focusin', onFocusIn)
    document.addEventListener('focusout', onFocusOut)
    return () => {
      document.removeEventListener('focusin', onFocusIn)
      document.removeEventListener('focusout', onFocusOut)
      clearPinTimer()
    }
  }, [])

  // The soft keyboard shrinks the scroller's *own* box — its flex parent
  // loses height to the shrunk visual viewport — without touching its
  // content, so the content-resize observer above never fires for it. Left
  // alone, a reader already pinned to the newest message has their scrollTop
  // unchanged while the box's bottom edge moves up to meet the keyboard,
  // burying exactly the row they were reading. Re-pin on the scroller's own
  // resize too, both for the keyboard's appearance and its dismissal (which
  // grows the box back and would otherwise leave a gap of blank space below
  // the last row). A mid-scroll reader is untouched, same as every other pin
  // here: the `stickToBottom` guard only fires this while already at bottom.
  useLayoutEffect(() => {
    if (
      highlighted !== null ||
      !atEnd ||
      typeof ResizeObserver === 'undefined'
    ) {
      return
    }
    const el = scroller.current
    if (el === null) {
      return
    }
    const observer = new ResizeObserver(() => {
      perfMark('timeline:keyboard:scroller-resize', {
        clientHeight: el.clientHeight,
        scrollHeight: el.scrollHeight,
        scrollTop: el.scrollTop,
        stickToBottom: stickToBottom.current,
        keyboardPin: keyboardPin.current,
      })
      if (keyboardPin.current) {
        stickToBottom.current = true
      }
      if (stickToBottom.current) {
        scheduleResizePin()
      }
    })
    observer.observe(el)
    return () => observer.disconnect()
  }, [highlighted, atEnd, scheduleResizePin])

  // Scroll anchoring, done by hand (`overflow-anchor: none` in index.css).
  //
  // A row's real height is only known once it has been rendered — the timeline
  // reserves an estimate for the rest (`content-visibility`), and the estimate
  // is necessarily wrong for a wrapped body, a reaction row, or an image. When
  // a row *above* the reader corrects itself, everything below it moves, and a
  // scroll-back turns into a series of lurches (measured at 50–90px a time on
  // a phone).
  //
  // The browser's own anchoring is supposed to absorb this, but it is uneven
  // across engines — WebKit only shipped it in Safari 27 — and it has no view
  // on which row the reader actually cares about. So we hold the topmost
  // visible row ourselves and put back whatever moved it. `ResizeObserver`
  // runs before paint, so the correction lands in the same frame and is never
  // seen. The bottom pin owns the other direction, hence the `stickToBottom`
  // guard; a deep-link centring pass owns its own target, hence the other.
  useLayoutEffect(() => {
    if (typeof ResizeObserver === 'undefined') {
      return
    }
    const el = scroller.current
    const list = eventList.current
    if (el === null || list === null) {
      return
    }
    captureAnchor()
    const observer = new ResizeObserver(() => {
      const held = scrollAnchor.current
      if (
        held === null ||
        stickToBottom.current ||
        highlightedCentering.current.active ||
        // The anchor left the DOM — a slice replacement, or the retained-slice
        // trim. Nothing to hold on to; take a fresh one below.
        !held.row.isConnected
      ) {
        perfMark('timeline:anchor:skip', {
          reason:
            held === null
              ? 'no-anchor'
              : stickToBottom.current
                ? 'stick-to-bottom'
                : highlightedCentering.current.active
                  ? 'centering'
                  : 'detached',
          // What the correction *would* have been. Carried even when nothing
          // is applied: it is the only way to see how big the shifts during a
          // scroll actually are, rather than inferring them from a video.
          moved:
            held === null || !held.row.isConnected
              ? null
              : Math.round(
                  held.row.getBoundingClientRect().top -
                    el.getBoundingClientRect().top -
                    held.top +
                    (el.scrollTop - held.scrollTop),
                ),
        })
        captureAnchor()
        return
      }
      // A row's viewport-relative top is `itsOffsetInContent - scrollTop`. So
      // between two observations:
      //
      //   topNow - topThen = grownAbove - (scrollTopNow - scrollTopThen)
      //
      // Rearranged, the growth above the anchor is the change in its position
      // *plus* whatever the reader scrolled in between — which is what lets
      // the baseline outlive their scrolling instead of being retaken every
      // frame (and retaken, as it turned out, too late to see anything).
      const moved =
        held.row.getBoundingClientRect().top -
        el.getBoundingClientRect().top -
        held.top +
        (el.scrollTop - held.scrollTop)
      // Sub-pixel drift is layout noise, not a shift worth chasing.
      if (Math.abs(moved) >= 0.5) {
        const before = el.scrollTop
        el.scrollTop += moved
        // `applied` is read back deliberately: if it does not equal what was
        // asked for, the scroller refused the write — which is what an
        // inertial fling owning the scroll position would look like.
        perfMark('timeline:anchor:correct', {
          moved: Math.round(moved),
          requested: Math.round(before + moved),
          applied: Math.round(el.scrollTop),
        })
      }
      captureAnchor()
    })
    observer.observe(list)
    return () => observer.disconnect()
  }, [captureAnchor])

  // Bring the highlighted (`?event=`) row into view once it exists — after
  // the mount effect above, so a deep link lands on its target rather than
  // the bottom of the page (WCR-09). Once per target id: the user scrolling
  // away must not be yanked back by an unrelated re-render.
  const revealed = useRef<string | null>(null)
  const highlightedResizeFrame = useRef<number | null>(null)
  useLayoutEffect(() => {
    if (highlighted === null || revealed.current === highlighted) {
      return
    }
    if (centerHighlightedRow(scroller.current, highlighted)) {
      revealed.current = highlighted
    }
  }, [highlighted, visible])

  // The jump page *ends* at the target, so the first centering had nothing
  // below to scroll against and the row sat at the viewport bottom. Once the
  // forward auto-load puts rows beneath it, center it for real — once per
  // target id, same anti-yank rule as the reveal above.
  const recentered = useRef<string | null>(null)
  useEffect(() => {
    if (
      highlighted === null ||
      revealed.current !== highlighted ||
      recentered.current === highlighted ||
      !highlightedCentering.current.active
    ) {
      return
    }
    const index = visible.findIndex((e) => e.event_id === highlighted)
    if (index === -1 || index === visible.length - 1) {
      return
    }
    recentered.current = highlighted
    centerHighlightedRow(scroller.current, highlighted)
  }, [highlighted, visible])

  // A highlighted row can be centered correctly and then drift out of view
  // when row heights settle: thread buttons hydrate, images decode, or
  // formatted bodies resize. While a search/deep-link target is active, keep
  // that target centered on actual timeline layout changes. Stop after the
  // user starts scrolling so the deep-link target does not become a sticky
  // anchor that fights movement toward newer messages.
  useLayoutEffect(() => {
    if (highlighted === null || typeof ResizeObserver === 'undefined') {
      return
    }
    const list = eventList.current
    if (list === null) {
      return
    }
    const scheduleCenter = () => {
      if (highlightedResizeFrame.current !== null) {
        return
      }
      highlightedResizeFrame.current = requestAnimationFrame(() => {
        highlightedResizeFrame.current = null
        if (
          highlightedCentering.current.eventId === highlighted &&
          highlightedCentering.current.active
        ) {
          centerHighlightedRow(scroller.current, highlighted)
        }
      })
    }
    const observer = new ResizeObserver(scheduleCenter)
    observer.observe(list)
    const row = findEventRow(scroller.current, highlighted)
    if (row !== null) {
      observer.observe(row)
    }
    return () => {
      observer.disconnect()
      if (highlightedResizeFrame.current !== null) {
        cancelAnimationFrame(highlightedResizeFrame.current)
        highlightedResizeFrame.current = null
      }
    }
  }, [highlighted, visible])

  // Sends no longer remount this component (no more full-page reload), so a
  // new message of my own — pending local echo or otherwise — needs its own
  // scroll-to-bottom rather than relying on the mount effect above.
  useEffect(() => {
    const last = visible[visible.length - 1]
    if (
      last !== undefined &&
      ownUserId !== null &&
      last.sender === ownUserId &&
      last.event_id !== lastOwnEventId.current
    ) {
      lastOwnEventId.current = last.event_id
      const el = scroller.current
      if (el !== null) {
        scrollTimelineToBottom(el, stickToBottom)
      }
    }
  }, [visible, ownUserId])

  /**
   * Receipts for a gallery, taken from its last event: the furthest point a
   * reader reached. Only own-message receipts are shown, matching the rule
   * `renderRow` applies to ordinary rows.
   */
  const galleryReceipts = (events: readonly TimelineEvent[]) => {
    const last = events[events.length - 1]
    return last.sender === ownUserId
      ? ephemeral.readReceipts(
          accountId,
          last.room_id,
          last.event_id,
          ownUserId,
        )
      : []
  }

  /**
   * One ordinary message row. Extracted so a gallery's expander can render
   * exactly the same thing — that is what keeps reply, edit, delete, react,
   * thread, inspect and retry reachable without the grid reimplementing any
   * of them.
   */
  const renderRow = (event: TimelineEvent) => (
    <MessageEventRow
      event={event}
      timeline={timeline}
      threads={threads}
      threadUnread={threadUnread.isUnread(
        accountId,
        event.room_id,
        event.event_id,
      )}
      members={members}
      accountId={accountId}
      ownUserId={ownUserId}
      readReceipts={
        event.sender === ownUserId
          ? ephemeral.readReceipts(
              accountId,
              event.room_id,
              event.event_id,
              ownUserId,
            )
          : []
      }
      highlighted={event.event_id === highlighted}
      settings={settings}
      reactionPickerOpen={reactionPickerEventId === event.event_id}
      onSetReactionPicker={onSetReactionPicker}
      actionsOpen={actionsOpenEventId === event.event_id}
      onOpenActions={() => setActionsOpenEventId(event.event_id)}
      onReply={onReply}
      onEdit={onEdit}
      onOpenThread={onOpenThread}
      onMutation={onMutation}
    />
  )

  return (
    <div
      class="timeline"
      ref={scroller}
      onPointerDown={stopHighlightedCentering}
      onScroll={(event) => {
        const el = event.currentTarget
        stickToBottom.current = isScrolledToTimelineBottom(el)
        // The anchor survives scrolling, so it is *not* retaken here — doing
        // that is what blinded the correction. It only has to stay near the
        // viewport, since a change between it and the visible rows would go
        // uncounted. Where it would be without any growth is arithmetic on
        // `scrollTop`, so the common case costs no geometry read at all.
        const held = scrollAnchor.current
        if (
          held !== null &&
          Math.abs(held.top - (el.scrollTop - held.scrollTop)) >
            el.clientHeight * ANCHOR_REACH
        ) {
          captureAnchor()
        }
      }}
      onTouchStart={stopHighlightedCentering}
      onWheel={stopHighlightedCentering}
    >
      <div ref={topSentinel} />
      {timeline.atStart.value ? (
        <p class="muted timeline-edge">Beginning of room history.</p>
      ) : (
        <button
          type="button"
          class="timeline-edge"
          disabled={timeline.loadingOlder.value}
          onClick={() => void timeline.loadOlder()}
        >
          {timeline.loadingOlder.value ? 'Loading…' : 'Load older messages'}
        </button>
      )}
      {visible.length === 0 && (
        <EmptyTimelineMessage events={timeline.events.value} />
      )}
      <div class="timeline-list-shell">
        <ol class="event-list" ref={eventList}>
          {/* The key must sit on the fragment itself — the list child — not on
            the elements inside it. Preact reconciles unkeyed fragments by
            index, so a `loadOlder` prepend would re-pair every row by
            position and attach per-row state (an open confirm, a picker) to
            a different message (WCR-01; RoomList learned this once too). */}
          {rows.map((row, index) => (
            <Fragment key={row.key}>
              {(index === 0 ||
                !sameLocalDay(rowTs(rows[index - 1]), rowTs(row))) && (
                <DaySeparator ts={rowTs(row)} />
              )}
              {row.kind === 'gallery' ? (
                <MediaGalleryRow
                  events={row.events}
                  accountId={accountId}
                  members={members}
                  readReceipts={galleryReceipts(row.events)}
                  highlighted={highlighted}
                  renderEvent={renderRow}
                />
              ) : (
                renderRow(row.event)
              )}
            </Fragment>
          ))}
        </ol>
      </div>
      {!atEnd && (
        <>
          {/* Sentinel above the button, not below: anything that reveals the
              button must also reveal the sentinel, or the *last* forward
              probe — the one that discovers there is nothing newer and
              retires this affordance — can sit one row out of the viewport
              and never fire. */}
          <div ref={bottomSentinel} />
          <button
            type="button"
            class="timeline-edge"
            disabled={timeline.loadingNewer.value}
            onClick={() => void timeline.loadNewer()}
          >
            {timeline.loadingNewer.value ? 'Loading…' : 'Load newer messages'}
          </button>
        </>
      )}
    </div>
  )
}

function EmptyTimelineMessage({
  events,
}: {
  events: readonly TimelineEvent[]
}): JSX.Element {
  if (events.length === 0) {
    return (
      <p class="muted">
        No messages loaded for this room yet. Newly joined large rooms can take
        a little while to sync history.
      </p>
    )
  }
  return (
    <p class="muted">
      No displayable events on this page. State events, redactions, or
      diagnostics may be hidden by timeline settings.
    </p>
  )
}

function formatTypingIndicator(
  userIds: readonly string[],
  members: MembersStore,
): string | null {
  if (userIds.length === 0) {
    return null
  }
  const names = userIds.map((userId) => members.displayName(userId))
  if (names.length === 1) {
    return `${names[0]} is typing`
  }
  if (names.length === 2) {
    return `${names[0]} and ${names[1]} are typing`
  }
  return `${names[0]}, ${names[1]}, and ${names.length - 2} ${names.length === 3 ? 'other is' : 'others are'} typing`
}

function JumpDialog({
  onClose,
  onJump,
}: {
  onClose: () => void
  onJump: (startTs: number, endTs: number) => void
}) {
  const { containerRef } = useModalFocus<HTMLDivElement>()
  const [value, setValue] = useState('')
  const [error, setError] = useState<string | null>(null)
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
    },
    { whileTyping: true, capture: true },
  )

  const submit = (event: Event) => {
    event.preventDefault()
    const day = parseCalendarDay(value)
    if (day === null) {
      setError('Enter a date like 2026-07-16, 7/16/2026, today, or yesterday.')
      return
    }
    onJump(day.start, day.end)
    onClose()
  }

  return (
    <div
      ref={containerRef}
      class="overlay"
      role="dialog"
      aria-modal="true"
      aria-label="Jump to date"
    >
      <form class="overlay-panel jump-dialog" onSubmit={submit}>
        <div class="overlay-head">
          <h2>Jump</h2>
          <button type="button" class="ghost" onClick={onClose}>
            Close
          </button>
        </div>
        <label>
          Date
          <input
            type="text"
            value={value}
            placeholder="YYYY-MM-DD"
            aria-invalid={error !== null}
            onInput={(event) => {
              setValue(event.currentTarget.value)
              setError(null)
            }}
          />
        </label>
        <label>
          Calendar
          <input
            type="date"
            value={/^\d{4}-\d{2}-\d{2}$/.test(value) ? value : ''}
            onInput={(event) => {
              setValue(event.currentTarget.value)
              setError(null)
            }}
          />
        </label>
        {error !== null && (
          <p class="error" role="alert">
            {error}
          </p>
        )}
        <div class="dialog-actions">
          <button type="submit">Jump</button>
        </div>
      </form>
    </div>
  )
}

function isScrolledToTimelineBottom(el: HTMLElement): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight <= 2
}

/**
 * Whether the top sentinel sits within the scroller's prefetch band, measured
 * from live geometry. The chain asks this itself rather than trusting the
 * observer: intersections are recomputed at frame boundaries, so what the
 * callback last reported describes the slice as it was before the page landed
 * — and the browser only reports *changes*, which is a signal that may never
 * come again once the chain has stopped.
 */
function withinPrefetchBand(sentinel: Element, root: Element | null): boolean {
  const bounds = (root ?? document.documentElement).getBoundingClientRect()
  const rect = sentinel.getBoundingClientRect()
  return (
    rect.bottom >= bounds.top - SCROLL_BACK_PREFETCH_PX &&
    rect.top <= bounds.bottom
  )
}

/** One frame, so layout and scroll anchoring settle before a re-measure. */
function nextFrame(): Promise<void> {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()))
}

function scrollTimelineToBottom(
  el: HTMLElement,
  stickToBottom: { current: boolean },
): void {
  if (!isScrolledToTimelineBottom(el)) {
    el.scrollTop = el.scrollHeight
  }
  stickToBottom.current = true
}

function findEventRow(
  scroller: HTMLElement | null,
  eventId: string,
): HTMLElement | null {
  // Matched by attribute comparison, not a selector: event ids carry `$`
  // and `:`, and `CSS.escape` does not exist under jsdom.
  return (
    [
      ...(scroller?.querySelectorAll<HTMLElement>('[data-event-id]') ?? []),
    ].find((el) => el.getAttribute('data-event-id') === eventId) ?? null
  )
}

function centerHighlightedRow(
  scroller: HTMLElement | null,
  eventId: string,
): boolean {
  const row = findEventRow(scroller, eventId)
  if (row === null) {
    return false
  }
  // jsdom has no scrollIntoView; the optional call keeps tests honest.
  row.scrollIntoView?.({ block: 'center', inline: 'nearest' })
  return true
}

function eventPreview(event: EventDto | undefined): string | null {
  const body = event?.body?.trim()
  return body === undefined || body === '' ? null : body
}

type ComposerCommand =
  | { kind: 'cancel-invite'; user: string }
  | { kind: 'formatted-message'; message: FormattedMessage }
  | { kind: 'forget' }
  | { kind: 'help' }
  | { kind: 'create' }
  | { kind: 'dm'; target: string | null }
  | { kind: 'find' }
  | { kind: 'invite'; users: string }
  | { kind: 'join'; target: string | null }
  | { kind: 'jump'; date: string | null }
  | { kind: 'knock'; target: string; reason: string | null }
  | { kind: 'leave' }
  | { kind: 'pin'; target: string | null }
  | { kind: 'react'; reaction: string | null }
  | { kind: 'refresh' }
  | { kind: 'reply'; body: string | null }
  | { kind: 'room'; target: string }
  | { kind: 'search'; args: string }
  | { kind: 'shortcuts' }
  | { kind: 'sort'; value: RoomSort | null }
  | { kind: 'thread' }
  | { kind: 'unpin'; target: string | null }
  | { kind: 'unreadthreads' }
  | { kind: 'usage'; name: SlashCommandName }
  | { kind: 'whereami' }

function parseComposerCommand(body: string): ComposerCommand | null {
  const trimmed = body.trim()
  const [name] = trimmed.split(/\s+/, 1)
  const commandName = canonicalSlashCommandName(name)
  if (commandName === null) {
    return null
  }
  const args = trimmed.slice(name.length).trim()
  if (commandName === SLASH_COMMAND.help) {
    return { kind: 'help' }
  }
  if (commandName === SLASH_COMMAND.shortcuts) {
    return { kind: 'shortcuts' }
  }
  if (commandName === SLASH_COMMAND.html) {
    return args === ''
      ? { kind: 'usage', name: commandName }
      : { kind: 'formatted-message', message: rawHtmlMessage(args) }
  }
  if (commandName === SLASH_COMMAND.literal) {
    return args === ''
      ? { kind: 'usage', name: commandName }
      : { kind: 'formatted-message', message: literalMessage(args) }
  }
  if (commandName === SLASH_COMMAND.rainbow) {
    return args === ''
      ? { kind: 'usage', name: commandName }
      : { kind: 'formatted-message', message: rainbowMessage(args) }
  }
  if (commandName === SLASH_COMMAND.spoiler) {
    if (args === '') {
      return { kind: 'usage', name: commandName }
    }
    const [reason, text] = parseSpoilerArg(args)
    return {
      kind: 'formatted-message',
      message: spoilerMessage(reason, text),
    }
  }
  if (commandName === SLASH_COMMAND.jump) {
    return { kind: 'jump', date: args === '' ? null : args }
  }
  if (commandName === SLASH_COMMAND.react) {
    return { kind: 'react', reaction: args === '' ? null : args }
  }
  if (commandName === SLASH_COMMAND.reply) {
    return { kind: 'reply', body: args === '' ? null : args }
  }
  if (commandName === SLASH_COMMAND.room) {
    // An empty target is a *known* command used wrong, not an unknown one —
    // the caller answers it with the usage line.
    return {
      kind: 'room',
      target: args,
    }
  }
  if (commandName === SLASH_COMMAND.dm) {
    return { kind: 'dm', target: args === '' ? null : args }
  }
  if (commandName === SLASH_COMMAND.create) {
    return args === ''
      ? { kind: 'create' }
      : { kind: 'usage', name: commandName }
  }
  if (commandName === SLASH_COMMAND.find) {
    return args === '' ? { kind: 'find' } : { kind: 'usage', name: commandName }
  }
  if (commandName === SLASH_COMMAND.invite) {
    return args === ''
      ? { kind: 'usage', name: commandName }
      : { kind: 'invite', users: args }
  }
  if (commandName === SLASH_COMMAND.cancel) {
    return args === ''
      ? { kind: 'usage', name: commandName }
      : { kind: 'cancel-invite', user: args }
  }
  if (commandName === SLASH_COMMAND.join) {
    return { kind: 'join', target: args === '' ? null : args }
  }
  if (commandName === SLASH_COMMAND.knock) {
    if (args === '') {
      return { kind: 'usage', name: commandName }
    }
    const [target, reason] = splitCommandTarget(args)
    return { kind: 'knock', target, reason }
  }
  if (commandName === SLASH_COMMAND.leave) {
    return args === ''
      ? { kind: 'leave' }
      : { kind: 'usage', name: commandName }
  }
  if (commandName === SLASH_COMMAND.forget) {
    return args === ''
      ? { kind: 'forget' }
      : { kind: 'usage', name: commandName }
  }
  if (commandName === SLASH_COMMAND.pin) {
    return { kind: 'pin', target: args === '' ? null : args }
  }
  if (commandName === SLASH_COMMAND.unpin) {
    return { kind: 'unpin', target: args === '' ? null : args }
  }
  if (commandName === SLASH_COMMAND.sort) {
    return {
      kind: 'sort',
      value: ROOM_SORTS.includes(args as RoomSort) ? (args as RoomSort) : null,
    }
  }
  if (commandName === SLASH_COMMAND.refresh) {
    return { kind: 'refresh' }
  }
  if (commandName === SLASH_COMMAND.unreadthreads) {
    return { kind: 'unreadthreads' }
  }
  if (commandName === SLASH_COMMAND.search) {
    return {
      kind: 'search',
      args,
    }
  }
  if (commandName === SLASH_COMMAND.thread) {
    return { kind: 'thread' }
  }
  if (commandName === SLASH_COMMAND.whereami) {
    return { kind: 'whereami' }
  }
  return null
}

function parseSpoilerArg(arg: string): [string | null, string] {
  const split = arg.split(' | ')
  if (split.length < 2) {
    return [null, arg]
  }
  const reason = split[0].trim()
  return [reason === '' ? null : reason, split.slice(1).join(' | ')]
}

function splitCommandTarget(arg: string): [string, string | null] {
  const [target] = arg.split(/\s+/, 1)
  const rest = arg.slice(target.length).trim()
  return [target, rest === '' ? null : rest]
}

function isOnlyJoinedMember(
  members: Iterable<MemberDto>,
  ownUserId: string,
): boolean {
  let joinedCount = 0
  let joinedUserId: string | null = null
  for (const member of members) {
    if (member.membership !== 'join') {
      continue
    }
    joinedCount += 1
    joinedUserId = member.user_id
    if (joinedCount > 1) {
      return false
    }
  }
  return joinedCount === 1 && joinedUserId === ownUserId
}

function resolveCommandRoom(
  rooms: Parameters<typeof resolveRoomTarget>[0],
  target: string,
): (typeof rooms)[number] | string | null {
  const resolution = resolveRoomTarget(rooms, target)
  if (resolution.kind === 'missing') {
    return null
  }
  if (resolution.kind === 'ambiguous') {
    return `room name is ambiguous: ${resolution.options.join(', ')}`
  }
  return resolution.room
}

function resolveReactionCommandKey(
  input: string,
  emojiEntries: readonly EmojiEntry[],
): string | null {
  const trimmed = input.trim()
  const alias = trimmed.replace(/^:/, '').replace(/:$/, '').toLowerCase()
  const mapped = REACTION_COMMAND_ALIASES.get(alias)
  if (mapped !== undefined) {
    return mapped
  }
  const shortcode = resolveEmojiShortcode(trimmed, emojiEntries)
  if (shortcode !== null) {
    return shortcode
  }
  const key = canonicalReactionKey(trimmed)
  return /\p{Extended_Pictographic}/u.test(key) ? key : null
}
