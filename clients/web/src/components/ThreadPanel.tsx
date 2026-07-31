import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { inBackground } from '../api/client'
import { timelineEvent } from '../api/frames'
import { useServices } from '../services'
import type { MembersStore } from '../stores/members'
import type { RoomDto } from '../stores/room-list'
import type { ThreadUnreadStore } from '../stores/thread-unread'
import { threadRootId } from '../stores/threads'
import { localThreadEventHref } from '../matrix-to'
import { createTimelineStore, type EventDto } from '../stores/timeline'
import type { TimelineEvent, TimelineStore } from '../stores/timeline'
import { Composer, type ComposerAutocompleteOption } from './Composer'
import { ErrorBanner } from './ErrorBanner'
import { Fragment } from 'preact'
import { MediaViewerProvider } from '../media/media-viewer'
import { MediaGalleryRow } from './MediaGalleryRow'
import { groupMediaRuns } from '../timeline/group-media-runs'
import { EventBody } from './EventBody'
import { MessageEventRow } from './MessageEventRow'
import { UserAvatar } from './UserAvatar'
import { useMessageComposer, type ComposerAction } from './use-message-composer'

type ThreadCommandHandler = (
  body: string,
  timeline: TimelineStore,
  visibleEvents: readonly TimelineEvent[],
  isVisibleEvent: (event: TimelineEvent) => boolean,
  action: ComposerAction | null,
  setAction: (action: ComposerAction | null) => void,
  setReactionPickerEventId: (eventId: string | null) => void,
  formatComposerBody: ReturnType<
    typeof useMessageComposer
  >['formatComposerBody'],
) => boolean | Promise<boolean>

function ThreadRoot({
  accountId,
  rootId,
  root,
  members,
  compact = false,
}: {
  accountId: string
  rootId: string
  root: EventDto | undefined
  members: MembersStore
  compact?: boolean
}) {
  return (
    <div class={`thread-root${compact ? ' compact' : ''}`}>
      {root !== undefined ? (
        <>
          <UserAvatar
            accountId={accountId}
            userId={root.sender}
            displayName={members.displayName(root.sender)}
            member={members.members.value.get(root.sender)}
          />
          <span class="thread-root-copy">
            <span class="event-sender">{members.displayName(root.sender)}</span>{' '}
            <EventBody
              event={root}
              senderDisplay={members.displayName(root.sender)}
            />
          </span>
        </>
      ) : (
        <span class="muted">
          root <code>{rootId}</code>
        </span>
      )}
    </div>
  )
}

/**
 * One open thread (ADR 0046, M-W7; ADR 0032): the root event, the
 * thread-scoped timeline, and a composer that sends into the thread. Opened
 * via `?thread=<root_id>` — the deep-link contract — and closed by clearing
 * that query param (the parent owns the routing).
 */
export function ThreadPanel({
  accountId,
  roomId,
  rootId,
  rootEvent,
  ownUserId,
  members,
  rooms,
  roomTitles,
  threadUnread,
  onCommand,
  roomCompletions,
  onClose,
  highlightedEventId = null,
}: {
  accountId: string
  roomId: string
  rootId: string
  rootEvent: EventDto | undefined
  ownUserId: string | null
  members: MembersStore
  rooms: readonly RoomDto[]
  roomTitles: ReadonlyMap<string, string>
  threadUnread: ThreadUnreadStore
  onCommand?: ThreadCommandHandler
  roomCompletions?(query: string): ComposerAutocompleteOption[]
  onClose: () => void
  /** An event in this thread to fetch, reveal, and highlight. */
  highlightedEventId?: string | null
}) {
  const { api, deviceState, live, media, search, settings } = useServices()
  const location = useLocation()
  const hideRedacted = settings.hideRedactedEvents.value
  const thread = useMemo(
    () => createTimelineStore(api, media, accountId, roomId, rootId),
    [api, media, accountId, roomId, rootId],
  )
  const [root, setRoot] = useState<EventDto | undefined>(rootEvent)
  const [reactionPickerEventId, setReactionPickerEventId] = useState<
    string | null
  >(null)
  /** Scopes the viewer's focus-restore lookup to this panel's own rows. */
  const threadListRef = useRef<HTMLOListElement>(null)
  const rootRef = useRef<HTMLDivElement>(null)
  const [rootPinned, setRootPinned] = useState(false)
  const revealedEventId = useRef<string | null>(null)
  const targetEventId =
    highlightedEventId ??
    (typeof location.query.event === 'string' ? location.query.event : null)

  // The full root always stays in the scrollable thread. Once it has scrolled
  // out of view, a compact overlay replaces it at the top of the scrollport.
  // Keeping that overlay before the root in the DOM makes its sticky position
  // independent of the full root's height.
  const handleThreadScroll = (event: Event) => {
    const scroller = event.currentTarget as HTMLDivElement
    const scrollTop = scroller.scrollTop
    const root = rootRef.current
    if (root === null) {
      return
    }
    const rootHasPassed = scrollTop >= root.offsetHeight
    if (rootPinned) {
      if (!rootHasPassed) {
        setRootPinned(false)
      }
      return
    }
    if (rootHasPassed) {
      setRootPinned(true)
    }
  }

  useEffect(() => {
    setRootPinned(false)
  }, [rootId])

  // The root starts as a compact placeholder for cold thread links. Once its
  // real event arrives, its height can change substantially without another
  // scroll event; recalculate the overlay state from the current geometry.
  useLayoutEffect(() => {
    const rootElement = rootRef.current
    const scroller = rootElement?.parentElement
    if (rootElement === null || scroller === null || scroller === undefined) {
      return
    }
    setRootPinned(scroller.scrollTop >= rootElement.offsetHeight)
  }, [root])
  const {
    setAction,
    banner: composerBanner,
    attachable,
    attachments,
    stage,
    removeAttachment,
    dragging,
    dropHandlers,
    formatComposerBody,
    mentionCompletions,
    roomReferenceCompletions,
    emojiCompletions,
    submitMessage,
    action,
  } = useMessageComposer({
    timeline: thread,
    accountId,
    members,
    rooms,
    roomTitles,
    ownUserId,
    attachmentScope: `${accountId} ${roomId} ${rootId}`,
    onMutation: search.clear,
  })

  // The room page's `?event=` jump owns the main timeline. A thread needs the
  // same lookup against its own paged endpoint, otherwise a deep link closes
  // the panel and lands in the room timeline instead.
  useEffect(() => {
    if (targetEventId === null) {
      void thread.loadLatest()
      return
    }
    // A reply may explicitly target the thread root. The root is not a
    // `m.thread` relation, but it is already represented above the timeline;
    // do not discard an already-loaded reply position by reloading the thread.
    if (targetEventId === rootId) {
      // A cold root deep link still needs the initial reply page; without it
      // the store remains in its initial loading state forever.
      if (thread.events.value.length === 0) {
        void thread.loadLatest()
      }
      return
    }
    if (thread.events.value.some((event) => event.event_id === targetEventId)) {
      return
    }
    let cancelled = false
    inBackground(
      api
        .GET('/v1/accounts/{account_id}/events/{event_id}', {
          params: {
            path: { account_id: accountId, event_id: targetEventId },
          },
        })
        .then(({ data }) => {
          if (cancelled) {
            return
          }
          if (data === undefined || threadRootId(data.data) !== rootId) {
            void thread.loadLatest()
            return
          }
          void thread.jumpTo(data.data.origin_ts)
        }),
    )
    return () => {
      cancelled = true
    }
  }, [api, accountId, rootId, thread, targetEventId])

  useEffect(() => {
    if (thread.loading.value) {
      return
    }
    const latest = thread.events.value.findLast(
      (event) => !event.event_id.startsWith('local:'),
    )
    if (latest === undefined) {
      return
    }
    deviceState.advanceThreadReadMarker(
      accountId,
      roomId,
      rootId,
      latest.event_id,
      latest.origin_ts,
    )
    threadUnread.markThreadRead(accountId, roomId, rootId)
  }, [
    thread.loading.value,
    thread.events.value,
    deviceState,
    threadUnread,
    accountId,
    roomId,
    rootId,
  ])

  // Live replies while the panel is open, mirroring RoomPage's two effects
  // (WCR-06): the store self-filters to this room and thread, so every
  // timeline frame can be offered. Without this an open thread went silently
  // stale — the main timeline hides thread members, so nothing else showed
  // an incoming reply.
  useEffect(() => {
    return live.subscribe((frame) => {
      const event = timelineEvent(frame)
      if (event !== null) {
        thread.ingestLive(event)
      }
    })
  }, [live, thread])

  // Reconnect gap-fill (ADR 0061): the bus is lossy, so re-read the thread's
  // head after a drop. `reconnects` starts at 0; the initial load above is
  // not doubled.
  useEffect(() => {
    if (live.reconnects.value === 0) {
      return
    }
    void thread.loadLatest()
  }, [live.reconnects.value, thread])

  useEffect(() => {
    if (root !== undefined) {
      return
    }
    let cancelled = false
    inBackground(
      api
        .GET('/v1/accounts/{account_id}/events/{event_id}', {
          params: { path: { account_id: accountId, event_id: rootId } },
        })
        .then(({ data }) => {
          if (!cancelled && data !== undefined) {
            setRoot(data.data)
          }
        }),
    )
    return () => {
      cancelled = true
    }
  }, [api, accountId, rootId, root])

  const visibleEvents = thread.events.value.filter(
    (event) => !hideRedacted || !event.redacted,
  )
  const isVisibleEvent = (event: TimelineEvent): boolean =>
    !hideRedacted || !event.redacted

  /**
   * Threads group images exactly as the room timeline does — the same
   * function, so "must not regress" is "provably one code path" (ADR 0081).
   */
  const rows = useMemo(() => groupMediaRuns(visibleEvents), [visibleEvents])
  const jumpToReplyContext = useCallback(
    (eventId: string) => {
      location.route(
        localThreadEventHref(accountId, roomId, rootId, eventId),
        true,
      )
    },
    [accountId, location, roomId, rootId],
  )

  useLayoutEffect(() => {
    if (targetEventId === null || revealedEventId.current === targetEventId) {
      return
    }
    if (targetEventId === rootId) {
      rootRef.current?.scrollIntoView?.({ block: 'start', inline: 'nearest' })
      revealedEventId.current = targetEventId
      return
    }
    const row = [
      ...(threadListRef.current?.querySelectorAll<HTMLElement>(
        '[data-event-id]',
      ) ?? []),
    ].find((element) => element.getAttribute('data-event-id') === targetEventId)
    if (row !== undefined) {
      row.scrollIntoView?.({ block: 'center', inline: 'nearest' })
      revealedEventId.current = targetEventId
    }
  }, [rootId, targetEventId, visibleEvents])

  /** One ordinary reply row; reused by a gallery's ungroup control. */
  const renderRow = (event: TimelineEvent) => (
    <MessageEventRow
      event={event}
      timeline={thread}
      members={members}
      accountId={accountId}
      ownUserId={ownUserId}
      highlighted={event.event_id === targetEventId}
      settings={settings}
      reactionPickerOpen={reactionPickerEventId === event.event_id}
      onSetReactionPicker={setReactionPickerEventId}
      onReply={(replyTo) => setAction({ kind: 'reply', event: replyTo })}
      onEdit={(editing) => setAction({ kind: 'edit', event: editing })}
      showThreadAction={false}
      threadRootId={rootId}
      onReplyContextJump={jumpToReplyContext}
      onMutation={search.clear}
    />
  )

  return (
    <aside
      class="thread-panel"
      aria-label="Thread"
      {...(attachable ? dropHandlers : {})}
    >
      {dragging && (
        <div class="drop-overlay" aria-hidden="true">
          <p>Drop to attach</p>
        </div>
      )}
      <div class="overlay-head">
        <h2>Thread</h2>
        <button type="button" class="ghost" onClick={onClose}>
          Close
        </button>
      </div>

      <ErrorBanner error={thread.error} />

      <div class="thread-timeline" onScroll={handleThreadScroll}>
        {rootPinned && (
          <div class="thread-root-preview">
            <ThreadRoot
              accountId={accountId}
              rootId={rootId}
              root={root}
              members={members}
              compact
            />
          </div>
        )}
        <div ref={rootRef}>
          <ThreadRoot
            accountId={accountId}
            rootId={rootId}
            root={root}
            members={members}
          />
        </div>
        {thread.loading.value ? (
          <p class="muted">Loading thread…</p>
        ) : (
          <>
            {!thread.atStart.value && (
              <button
                type="button"
                class="timeline-edge"
                disabled={thread.loadingOlder.value}
                onClick={() => void thread.loadOlder()}
              >
                {thread.loadingOlder.value ? 'Loading…' : 'Load older replies'}
              </button>
            )}
            {/* Its own provider: a thread's images are a separate sequence
                from the room's, so paging inside one must not wander into
                the other. */}
            <MediaViewerProvider
              accountId={accountId}
              events={visibleEvents}
              onLoadOlder={() => thread.loadOlder()}
              atStart={thread.atStart.value}
              findRow={(eventId) =>
                [
                  ...(threadListRef.current?.querySelectorAll<HTMLElement>(
                    '[data-event-id]',
                  ) ?? []),
                ].find((el) => el.getAttribute('data-event-id') === eventId) ??
                null
              }
              actions={{
                ownUserId,
                onReply: (event) => setAction({ kind: 'reply', event }),
                onReact: (event) => setReactionPickerEventId(event.event_id),
                onDelete: async (event) => {
                  const deleted = await thread.redact(event.event_id)
                  if (deleted) search.clear()
                  return deleted
                },
              }}
            >
              <div class="timeline-list-shell">
                <ol ref={threadListRef} class="event-list thread-list">
                  {rows.map((row) => (
                    <Fragment key={row.key}>
                      {row.kind === 'gallery' ? (
                        <MediaGalleryRow
                          events={row.events}
                          accountId={accountId}
                          members={members}
                          highlighted={targetEventId}
                          renderEvent={renderRow}
                        />
                      ) : (
                        renderRow(row.event)
                      )}
                    </Fragment>
                  ))}
                </ol>
              </div>
            </MediaViewerProvider>
          </>
        )}
      </div>

      <Composer
        placeholder="Reply in thread"
        autoFocus
        height={settings.messageComposerHeight.value}
        onHeightChange={(height) =>
          (settings.messageComposerHeight.value = height)
        }
        key={
          action?.kind === 'edit'
            ? action.event.event_id
            : `send:${accountId}:${roomId}:${rootId}`
        }
        banner={composerBanner}
        initialValue={
          action?.kind === 'edit'
            ? (action.draft ?? action.event.body ?? '')
            : ''
        }
        mentionCompletions={mentionCompletions}
        roomReferenceCompletions={roomReferenceCompletions}
        emojiCompletions={emojiCompletions}
        onCommand={
          onCommand === undefined
            ? undefined
            : (body) =>
                onCommand(
                  body,
                  thread,
                  visibleEvents,
                  isVisibleEvent,
                  action,
                  setAction,
                  setReactionPickerEventId,
                  formatComposerBody,
                )
        }
        roomCompletions={roomCompletions}
        onAttach={attachable ? stage : undefined}
        attachments={{ ...attachments, onRemove: removeAttachment }}
        // The store is thread-scoped, so `thread_root` is already implied.
        onSubmit={submitMessage}
      />
    </aside>
  )
}
