import { useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { currentRoomFromPath } from '../search-tokens'
import { useServices } from '../services'
import { useShortcuts } from '../shortcuts'
import {
  createThreadsStore,
  type ThreadsStore,
  type ThreadSummaryDto,
} from '../stores/threads'
import type { EventDto } from '../stores/timeline'
import { useModalFocus } from './use-modal-focus'

function eventPreview(event: EventDto | undefined): string | null {
  const body = event?.body?.trim()
  return body === undefined || body === '' ? null : body
}

function formatThreadTime(ts: number): string {
  return new Date(ts).toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
  })
}

function replyCountLabel(count: number): string {
  return count === 1 ? '1 reply' : `${count} replies`
}

export function UnreadThreadsPanel({ onClose }: { onClose: () => void }) {
  const { api, threadUnread } = useServices()
  const location = useLocation()
  const { containerRef } = useModalFocus<HTMLDivElement>()
  const unreadEntries = threadUnread.entries.value
  const room = currentRoomFromPath(location.path)
  const roomAccountId = room?.accountId
  const roomId = room?.roomId
  const threads = useMemo(() => {
    if (roomAccountId === undefined || roomId === undefined) {
      return null
    }
    return createThreadsStore(api, roomAccountId, roomId)
  }, [api, roomAccountId, roomId])
  const [preferRoomThreads, setPreferRoomThreads] = useState(false)
  const showingRoomThreads =
    room !== null && (unreadEntries.length === 0 || preferRoomThreads)

  useEffect(() => {
    if (unreadEntries.length === 0) {
      setPreferRoomThreads(false)
    }
  }, [unreadEntries.length])

  /**
   * Fetched only once the room list is actually on screen. This store
   * duplicates `RoomPage`'s — a summaries GET plus one GET per root — so a
   * drawer that opens on the unread list must not pay for it until the
   * reader toggles over.
   */
  const fetchedFor = useRef<ThreadsStore | null>(null)
  useEffect(() => {
    if (
      threads === null ||
      !showingRoomThreads ||
      fetchedFor.current === threads
    ) {
      return
    }
    fetchedFor.current = threads
    void threads.refresh()
  }, [threads, showingRoomThreads])

  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
    },
    { whileTyping: true, capture: true },
  )

  const canToggle = room !== null && unreadEntries.length > 0
  const title = showingRoomThreads ? 'Threads' : 'Unread threads'
  const toggleTitle = showingRoomThreads
    ? 'Show unread threads'
    : 'Show threads in this room'

  const openThread = (
    accountId: string,
    openRoomId: string,
    rootEventId: string,
  ) => {
    location.route(
      `/${accountId}/rooms/${encodeURIComponent(openRoomId)}?thread=${encodeURIComponent(rootEventId)}`,
    )
    onClose()
  }

  return (
    <div
      ref={containerRef}
      class="overlay unread-threads-overlay"
      role="dialog"
      aria-modal="true"
      aria-label={title}
    >
      <section class="overlay-panel unread-threads-panel">
        <div class="overlay-head">
          <h2>
            {canToggle ? (
              <button
                type="button"
                class="unread-threads-mode"
                title={toggleTitle}
                aria-pressed={showingRoomThreads}
                onClick={() => setPreferRoomThreads((current) => !current)}
              >
                {title}
              </button>
            ) : (
              title
            )}
          </h2>
          <button type="button" class="ghost" onClick={onClose}>
            Close
          </button>
        </div>
        {showingRoomThreads && room !== null ? (
          <RoomThreadList
            accountId={room.accountId}
            roomId={room.roomId}
            loading={threads?.loading.value ?? true}
            error={threads?.error.value ?? null}
            summaries={threads?.summaries.value ?? new Map()}
            roots={threads?.roots.value ?? new Map()}
            onOpen={(rootEventId) =>
              openThread(room.accountId, room.roomId, rootEventId)
            }
          />
        ) : unreadEntries.length === 0 ? (
          <p class="muted unread-threads-empty">No unread threads.</p>
        ) : (
          <ol class="unread-thread-list">
            {unreadEntries.map((entry) => (
              <li
                key={`${entry.accountId}/${entry.roomId}/${entry.rootEventId}`}
              >
                <ThreadEntryButton
                  roomTitle={entry.roomTitle}
                  rootPreview={entry.rootPreview}
                  sender={entry.latestSender}
                  latestLabel={entry.latestBody ?? 'New thread activity'}
                  latestTs={entry.latestTs}
                  onClick={() =>
                    openThread(entry.accountId, entry.roomId, entry.rootEventId)
                  }
                />
              </li>
            ))}
          </ol>
        )}
      </section>
    </div>
  )
}

function RoomThreadList({
  accountId,
  roomId,
  loading,
  error,
  summaries,
  roots,
  onOpen,
}: {
  accountId: string
  roomId: string
  loading: boolean
  error: string | null
  summaries: ReadonlyMap<string, ThreadSummaryDto>
  roots: ReadonlyMap<string, EventDto>
  onOpen: (rootEventId: string) => void
}) {
  const rows = [...summaries.values()]
    .map((summary) => {
      const root = roots.get(summary.root_event_id)
      return {
        rootEventId: summary.root_event_id,
        rootPreview: eventPreview(root),
        rootSender: root?.sender ?? null,
        latestTs: summary.latest_reply_ts ?? root?.origin_ts ?? 0,
        replyCount: summary.reply_count,
      }
    })
    .sort(
      (a, b) =>
        b.latestTs - a.latestTs || a.rootEventId.localeCompare(b.rootEventId),
    )

  if (loading && rows.length === 0) {
    return <p class="muted unread-threads-empty">Loading threads…</p>
  }
  if (error !== null && rows.length === 0) {
    return <p class="muted unread-threads-empty">{error}</p>
  }
  if (rows.length === 0) {
    return <p class="muted unread-threads-empty">No threads.</p>
  }

  return (
    <ol class="unread-thread-list">
      {rows.map((row) => (
        <li key={`${accountId}/${roomId}/${row.rootEventId}`}>
          <ThreadEntryButton
            roomTitle={null}
            rootPreview={row.rootPreview}
            sender={row.rootSender}
            latestLabel={replyCountLabel(row.replyCount)}
            latestTs={row.latestTs}
            onClick={() => onOpen(row.rootEventId)}
          />
        </li>
      ))}
    </ol>
  )
}

function ThreadEntryButton({
  roomTitle,
  rootPreview,
  sender,
  latestLabel,
  latestTs,
  onClick,
}: {
  roomTitle: string | null
  rootPreview: string | null
  sender: string | null
  latestLabel: string
  latestTs: number
  onClick: () => void
}) {
  return (
    <button type="button" class="unread-thread-entry" onClick={onClick}>
      {roomTitle !== null && (
        <span class="unread-thread-room">{roomTitle}</span>
      )}
      {rootPreview !== null && (
        <span class="unread-thread-root">{rootPreview}</span>
      )}
      <span class="unread-thread-latest">
        {sender !== null && <span class="event-sender">{sender}</span>}
        {latestLabel}
      </span>
      {latestTs > 0 && (
        <time dateTime={new Date(latestTs).toISOString()}>
          {formatThreadTime(latestTs)}
        </time>
      )}
    </button>
  )
}
