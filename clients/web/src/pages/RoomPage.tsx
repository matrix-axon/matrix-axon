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
import { parseCalendarDay } from '../calendar-day'
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
import { withSearchParam } from '../search-tokens'
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
  type EventDto,
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
  const openThread = typeof query.thread === 'string' ? query.thread : null
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
    rooms.noteUnreadCounts(accountId, roomId, 0, 0)
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
    if (timeline.events.value.some((event) => event.event_id === highlighted)) {
      return
    }
    void api
      .GET('/v1/accounts/{account_id}/events/{event_id}', {
        params: { path: { account_id: accountId, event_id: highlighted } },
      })
      .then(
        ({ data }) =>
          data !== undefined
            ? timeline.jumpTo(data.data.origin_ts)
            : timeline.loadLatest(),
        () => timeline.loadLatest(),
      )
  }, [api, accountId, timeline, highlighted])

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
  // quick mobile switches.
  useEffect(() => {
    const room = rooms.rooms.value.find(
      (candidate) =>
        candidate.account_id === accountId && candidate.room_id === roomId,
    )
    if (
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
  }, [accountId, roomId, rooms.rooms.value, deviceState, unreadThreadCutoff])

  // Advance this room's read marker to the newest event while it is open, so
  // sibling devices see it as read (M-W6 step 5c, ADR 0048). Hidden unread
  // thread replies are a hard stop: the user has opened the room, not that
  // thread panel, so a room marker must not silently consume them.
  useEffect(() => {
    const events = timeline.events.value
    const last = events.findLast(
      (event) =>
        !event.event_id.startsWith('local:') &&
        (unreadThreadCutoff === null || event.origin_ts < unreadThreadCutoff),
    )
    if (last !== undefined) {
      deviceState.advanceReadMarker(
        accountId,
        roomId,
        last.event_id,
        last.origin_ts,
      )
      // Second, fire-and-forget action alongside the cross-device marker
      // (ADR 0067): tell the homeserver too, so third-party Matrix clients see
      // the room as read. Forward-only + debounced inside the sender.
      ephemeralSender.noteRead(accountId, roomId, last.event_id, last.origin_ts)
    }
  }, [
    timeline.events.value,
    unreadThreadCutoff,
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
    // (`roomKey`), and a staged file must not survive switching between
    // them — it would send from the wrong account.
    attachmentScope: `${accountId} ${roomId}`,
    onMutation: search.clear,
  })
  void ephemeral.revision.value
  const typingText = formatTypingIndicator(
    ephemeral.typingUsers(accountId, roomId, ownUserId),
    members,
  )
  const roomReadMarker = deviceState.readMarker(accountId, roomId)
  const threadSummaryStates = [...threads.summaries.value.values()].map(
    (summary) => ({
      summary,
      threadMarker: deviceState.threadReadMarker(
        accountId,
        roomId,
        summary.root_event_id,
      ),
      rootPreview: eventPreview(threads.roots.value.get(summary.root_event_id)),
    }),
  )

  useEffect(() => {
    for (const { summary, threadMarker, rootPreview } of threadSummaryStates) {
      threadUnread.reconcileSummary(summary, {
        accountId,
        roomId,
        roomTitle: title,
        rootPreview,
        roomMarker: roomReadMarker,
        threadMarker,
      })
    }
  }, [
    threadUnread,
    threadSummaryStates,
    accountId,
    roomId,
    title,
    roomReadMarker,
  ])

  // Thread members live in the panel, not the main timeline (TUI parity);
  // state events are tiered behind the visibility setting, and bodyless
  // unsupported events are developer diagnostics rather than ordinary timeline
  // content.
  const isVisibleTimelineEvent = (event: TimelineEvent): boolean => {
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
  }
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
    // The filter is re-created every render, so it can never be a dependency
    // itself; these are the values it closes over.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [
      timeline.events.value,
      hideRedacted,
      stateEvents,
      settings.developerMode.value,
    ],
  )
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
            <p>Loading timeline…</p>
          ) : (
            <MediaViewerProvider
              accountId={accountId}
              events={visible}
              onLoadOlder={() => timeline.loadOlder()}
              atStart={timeline.atStart.value}
              findRow={(eventId) => findEventRow(roomStream.current, eventId)}
              runOf={(eventId) => runPosition(rows, eventId)}
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
  const resizePinFrame = useRef<number | null>(null)
  const highlightedCentering = useRef<{
    eventId: string | null
    active: boolean
  }>({ eventId: null, active: false })

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
                !sameDay(rowTs(rows[index - 1]), rowTs(row))) && (
                <li class="day-separator" role="separator">
                  {new Date(rowTs(row)).toLocaleDateString(undefined, {
                    weekday: 'short',
                    year: 'numeric',
                    month: 'short',
                    day: 'numeric',
                  })}
                </li>
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

function sameDay(a: number, b: number): boolean {
  return new Date(a).toDateString() === new Date(b).toDateString()
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
