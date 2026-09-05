import type { ComponentChildren } from 'preact'
import { createContext } from 'preact'
import {
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'preact/hooks'
import {
  Lightbox,
  LightboxImage,
  type LightboxImageOutcome,
} from '../components/Lightbox'
import { MediaCaption } from '../components/MediaCaption'
import { imageSequence, type LightboxItem } from './image-sequence'
import { useMediaBlob } from './use-media-blob'
import { downloadMedia, isDownloadable } from './download-media'
import { useServices } from '../services'
import type { TimelineEvent } from '../stores/timeline'
import { EventActionIcon } from '../components/EventActionIcon'
import { isMessageActionable } from '../components/event-action-eligibility'

/**
 * How many history pages a single run of "older" presses may load. Mirrors
 * `AUTO_SCROLL_BACK_PAGES` in `RoomPage`, and matters because the timeline is
 * not windowed (#26): every page paged past stays in the DOM.
 */
export const AUTO_PAGE_LIMIT = 5

/**
 * Identity, never an index. The index is recomputed each render, so a
 * back-pagination prepend or a live append costs nothing and cannot silently
 * slide the viewer onto a different image. `ts` is the fallback key for when
 * the event itself disappears (redaction, gap-fill).
 */
interface Cursor {
  eventId: string
  ts: number
}

interface MediaViewer {
  /** Open the viewer at this event; a no-op if it carries no viewable image. */
  open: (eventId: string) => void
}

/** Existing surface-owned message actions exposed for the open image. */
export interface MediaViewerActions {
  ownUserId: string | null
  onReply: (event: TimelineEvent) => void
  onReact: (event: TimelineEvent) => void
  onOpenThread?: (event: TimelineEvent) => void
  onDelete: (event: TimelineEvent) => Promise<boolean>
}

const MediaViewerContext = createContext<MediaViewer | null>(null)

/**
 * The viewer for the surrounding surface, or `null` when there is none — in
 * which case `MediaImage` falls back to its own single-image lightbox, which
 * is what `MediaPreview` and search results still rely on.
 */
export function useMediaViewer(): MediaViewer | null {
  return useContext(MediaViewerContext)
}

/**
 * Resolve a cursor to a position in the current sequence.
 *
 * An exact id match is the normal case. When the event is gone, fall back to
 * the nearest *newer* image by timestamp — the user was reading forward
 * through history, so the neighbour they have not seen is the better landing
 * place than the one they just left. Failing that, the last image; failing
 * that (`-1`) the sequence is empty and the caller closes.
 */
function resolveIndex(
  sequence: readonly LightboxItem[],
  cursor: Cursor | null,
): number {
  if (cursor === null) {
    return -1
  }
  const exact = sequence.findIndex((item) => item.eventId === cursor.eventId)
  if (exact !== -1) {
    return exact
  }
  const newer = sequence.findIndex((item) => item.ts >= cursor.ts)
  return newer !== -1 ? newer : sequence.length - 1
}

/** A render-nothing holder that keeps one neighbour's bytes warm. */
function LightboxPreload({
  accountId,
  mxcUrl,
}: {
  accountId: string
  mxcUrl: string
}) {
  useMediaBlob(accountId, mxcUrl, { eager: true })
  return null
}

/** Honour Data Saver: preloading is a convenience, not a requirement. */
function prefersLessData(): boolean {
  const connection = (
    navigator as Navigator & { connection?: { saveData?: boolean } }
  ).connection
  return connection?.saveData === true
}

/**
 * One lightbox for a whole surface, rather than one per image row — which is
 * what makes paging possible at all (ADR 0081). Wrap the timeline and the
 * thread panel separately: a thread's images are their own sequence.
 *
 * `events` must be the list the surface actually *renders*. See
 * `imageSequence` for why the raw store slice would strand the viewer.
 */
export function MediaViewerProvider({
  accountId,
  events,
  onLoadOlder,
  atStart,
  findRow,
  runOf,
  actions,
  children,
}: {
  accountId: string
  events: readonly TimelineEvent[]
  /** Load one more page of history; resolves to whether it advanced. */
  onLoadOlder?: () => Promise<boolean>
  /** Whether history is exhausted, so the oldest end is genuinely the oldest. */
  atStart?: boolean
  /** Locate an event's row, for focus restoration on close. */
  findRow?: (eventId: string) => HTMLElement | null
  /**
   * This image's position within its gallery run, when it is in one.
   *
   * Paging stays global — a mis-grouped run must never become a navigation
   * cage (ADR 0081) — but "3 of 47" means nothing to someone who just opened
   * a five-image post, so the counter names both.
   */
  runOf?: (eventId: string) => { index: number; total: number } | null
  actions?: MediaViewerActions
  children: ComponentChildren
}) {
  const { media: mediaService, platform } = useServices()
  const [cursor, setCursor] = useState<Cursor | null>(null)
  /** Which way the reader last moved, for aiming the preloads. 0 until a step. */
  const [direction, setDirection] = useState<-1 | 0 | 1>(0)
  const [saving, setSaving] = useState(false)
  const [confirmingDelete, setConfirmingDelete] = useState(false)
  const [deleting, setDeleting] = useState(false)
  const [deleteError, setDeleteError] = useState<string | null>(null)
  /**
   * Why a failed save must say so: `downloadMedia` resolves `'failed'` on a
   * fetch error, and the button simply reverting to idle is indistinguishable
   * from a save that worked — the file just never appears. `MediaAttachment`
   * already surfaces this; the viewer now uses the same shape.
   *
   * Tagged with the image it happened on, and rendered only while that image
   * is the one on screen. Clearing it at each site that moves the cursor was
   * the first attempt and it leaked: there are four such sites — a step, an
   * auto-paginated landing, the redaction re-anchor, and close — and only the
   * first cleared. Clearing in an effect keyed on the image fixes the leak but
   * runs a render late, so the stale pill flashes on the new photo. Deciding
   * it at render time cannot do either.
   */
  const [saveError, setSaveError] = useState<{
    eventId: string
    message: string
  } | null>(null)
  /**
   * How the open image's display attempt ended. Reset by `LightboxImage` on
   * every new object, so paging never carries one image's verdict onto the
   * next.
   */
  const [outcome, setOutcome] = useState<LightboxImageOutcome>('pending')
  /**
   * Whether saving the open object is worth offering. A displayed image
   * obviously is; so is one that would not decode but is identifiably HEIC or
   * similar, where downloading it to open elsewhere is the only remedy left.
   * Bytes that decoded as nothing recognisable are most likely ciphertext, and
   * saving those is worse than offering nothing (ADR 0101).
   */
  const saveable = outcome === 'displayed' || outcome === 'unsupported-format'
  const [pendingOlder, setPendingOlder] = useState(false)
  const [autoPages, setAutoPages] = useState(0)
  const [loadingOlder, setLoadingOlder] = useState(false)
  /** Re-entry guard; see the auto-pagination effect for why it is not state. */
  const loadingOlderRef = useRef(false)
  const mounted = useRef(true)
  useEffect(
    () => () => {
      mounted.current = false
    },
    [],
  )

  const sequence = useMemo(() => imageSequence(events), [events])
  const index = resolveIndex(sequence, cursor)
  const item = index === -1 ? null : sequence[index]

  // What the reader can actually see: a failure that belongs to another image
  // — one paged away from, or one a slow request has outlived — is simply not
  // rendered. No reset site can be forgotten, because there are none.
  const shownSaveError =
    saveError !== null && saveError.eventId === item?.eventId
      ? saveError.message
      : null

  // Dropped once it can no longer be shown, so paging back to the image it
  // happened on does not resurrect a failure from minutes ago.
  useEffect(() => {
    setSaveError(null)
  }, [item?.eventId])

  useEffect(() => {
    setConfirmingDelete(false)
    setDeleting(false)
    setDeleteError(null)
  }, [item?.eventId])

  // The cursor's event vanished and we landed on a neighbour — re-anchor, so
  // the next step is relative to where the user actually is.
  useEffect(() => {
    if (cursor === null) {
      return
    }
    if (item === null) {
      setCursor(null)
      return
    }
    if (item.eventId !== cursor.eventId) {
      setCursor({ eventId: item.eventId, ts: item.ts })
    }
  }, [cursor, item])

  const close = useCallback(() => {
    setCursor(null)
    setDirection(0)
    setPendingOlder(false)
    setAutoPages(0)
  }, [])

  const viewer = useMemo<MediaViewer>(
    () => ({
      open: (eventId: string) => {
        const target = sequence.find(
          (candidate) => candidate.eventId === eventId,
        )
        if (target !== undefined) {
          setCursor({ eventId: target.eventId, ts: target.ts })
          setAutoPages(0)
          setDirection(0)
        }
      },
    }),
    [sequence],
  )

  /**
   * Post-await state writes are guarded by `mounted`, the same way the
   * auto-pagination effect guards its completion handlers: a save can outlive
   * the dialog it was started from, since closing unmounts the viewer while
   * the fetch is still running.
   */
  const save = useCallback(
    async (target: LightboxItem) => {
      setSaving(true)
      setSaveError(null)
      const outcome = await downloadMedia(
        mediaService,
        target.accountId ?? accountId,
        target.media,
        platform,
      )
      if (!mounted.current) {
        return
      }
      setSaving(false)
      if (outcome === 'failed') {
        setSaveError({ eventId: target.eventId, message: 'Save failed' })
      }
    },
    [mediaService, accountId, platform],
  )

  const closeThen = useCallback(
    (callback: (event: TimelineEvent) => void) => {
      if (item === null) {
        return
      }
      close()
      callback(item.event)
    },
    [close, item],
  )

  const deleteCurrent = useCallback(async () => {
    if (item === null || actions === undefined || deleting) {
      return
    }
    setDeleting(true)
    setDeleteError(null)
    const deleted = await actions.onDelete(item.event)
    if (!mounted.current) {
      return
    }
    setDeleting(false)
    if (deleted) {
      close()
    } else {
      setDeleteError('Delete failed')
    }
  }, [actions, close, deleting, item])

  const step = useCallback(
    (delta: -1 | 1) => {
      if (item === null) {
        return
      }
      const next = sequence[index + delta]
      if (next !== undefined) {
        setCursor({ eventId: next.eventId, ts: next.ts })
        setAutoPages(0)
        setDirection(delta)
        return
      }
      // Off the oldest end: ask for history. The newest end is inert — there
      // is nothing to load forward into.
      if (delta === -1) {
        setPendingOlder(true)
      }
    },
    [index, item, sequence],
  )

  // The auto-pagination loop lives here rather than in the click handler
  // because `events` arrives as a prop: the click closure would only ever see
  // the sequence as it was when the button was rendered. It chains, because a
  // page of 50 events can easily contain no images at all.
  useEffect(() => {
    if (!pendingOlder || cursor === null) {
      return
    }
    // One page at a time. `setAutoPages` below changes a dependency of this
    // effect, so without this guard it re-enters *synchronously* — before the
    // load it just started has resolved — and burns the whole page budget in
    // one go, clearing `pendingOlder` before any image arrives. The step the
    // reader asked for was then dropped, and it took a second press to move.
    if (loadingOlderRef.current) {
      return
    }
    if (index > 0) {
      // Older images arrived — take the step the user asked for.
      const next = sequence[index - 1]
      setCursor({ eventId: next.eventId, ts: next.ts })
      setPendingOlder(false)
      setAutoPages(0)
      setDirection(-1)
      return
    }
    if (
      atStart === true ||
      onLoadOlder === undefined ||
      autoPages >= AUTO_PAGE_LIMIT
    ) {
      setPendingOlder(false)
      return
    }
    // The in-flight marker is a ref, not the state flag, and the completion
    // handlers are deliberately *not* tied to this effect's lifetime. Firing
    // the load changes `autoPages`, which re-runs the effect and would run a
    // `cancelled` cleanup — silently dropping the completion, so the state
    // flag never cleared and the chain stalled after one page.
    loadingOlderRef.current = true
    setLoadingOlder(true)
    setAutoPages((pages) => pages + 1)
    void onLoadOlder()
      .then((advanced) => {
        if (mounted.current && !advanced) {
          setPendingOlder(false)
        }
      })
      .finally(() => {
        loadingOlderRef.current = false
        if (mounted.current) {
          // Clearing this re-runs the effect, which takes the step if images
          // arrived or asks for the next page if they did not.
          setLoadingOlder(false)
        }
      })
  }, [
    pendingOlder,
    cursor,
    index,
    sequence,
    atStart,
    autoPages,
    loadingOlder,
    onLoadOlder,
  ])

  // Resolved at unmount, not captured at open: the user may have paged, and
  // back-pagination may have replaced the row they came from.
  //
  // Deliberately holds the *last non-null* cursor. Closing clears `cursor`
  // first, which unmounts the lightbox — so a ref tracking `cursor` verbatim
  // would always be null by the time the cleanup ran, and focus would fall
  // back to the row the user opened from rather than the one they left on.
  // Written in an effect, never during render. Ordering against the
  // lightbox's own unmount cleanup does not matter, because this is only ever
  // *set* — the last non-null value from an earlier render survives.
  const lastCursorRef = useRef<Cursor | null>(null)
  useEffect(() => {
    if (cursor !== null) {
      lastCursorRef.current = cursor
    }
  }, [cursor])
  const restoreTo = useCallback(() => {
    const current = lastCursorRef.current
    if (current === null || findRow === undefined) {
      return null
    }
    const row = findRow(current.eventId)
    if (row === null) {
      return null
    }
    // `nearest` is a no-op when the row is already fully visible, so closing
    // without paging does not move the timeline — but paging six images back
    // and closing leaves the reader looking at the photo they were just on,
    // rather than wherever they started.
    // Optional-called: jsdom does not implement it, and a missing scroll is
    // never a reason to fail the focus restore that follows.
    row.scrollIntoView?.({ block: 'nearest' })

    const opener = row.querySelector<HTMLElement>(
      '.media-open, .gallery-cell-open, .gallery-cell-deferred',
    )
    if (opener !== null) {
      return opener
    }
    // No open button yet: media loads lazily, so an image paged to while
    // off-screen is still a skeleton. The row cannot take focus on its own,
    // and `.focus()` on it would be a silent no-op that strands focus on
    // `<body>`. `tabindex="-1"` makes it programmatically focusable without
    // adding a tab stop.
    row.tabIndex = -1
    return row
  }, [findRow])

  /**
   * Two neighbours, aimed the way the reader is going.
   *
   * Symmetric ±1 spends one of its two slots on a near-certain cache hit: the
   * image behind you was just on screen, and `LRU_CAP` counts *entries, not
   * bytes*, so a released blob parks in the zero-ref LRU rather than being
   * freed. Pointing both slots the way you are travelling costs the same
   * bandwidth and actually buys something; backtracking stays instant for the
   * same reason.
   *
   * Direction is only known after a step, so an initial open preloads both
   * sides.
   */
  const preloadIndices =
    direction === 0
      ? [index - 1, index + 1]
      : direction === 1
        ? [index + 1, index + 2]
        : [index - 1, index - 2]

  // Two at a time regardless of direction. `MAX_CONCURRENT` bounds the queue,
  // but each of these is a full-size object competing with the photo actually
  // on screen — ADR 0081 measured a 318 KB mean, with no thumbnail to fall
  // back on for most encrypted images.
  const preloadable = prefersLessData()
    ? []
    : preloadIndices
        .map((at) => sequence[at])
        .filter(
          (neighbour): neighbour is LightboxItem =>
            neighbour !== undefined && neighbour.media.url !== null,
        )

  const contextualActions =
    item !== null &&
    actions !== undefined &&
    isMessageActionable(item.event) ? (
      <span class="lightbox-actions">
        {confirmingDelete ? (
          <>
            <button
              type="button"
              class="ghost lightbox-action"
              aria-label="Cancel delete"
              title="Cancel delete"
              disabled={deleting}
              onClick={() => {
                setConfirmingDelete(false)
                setDeleteError(null)
              }}
            >
              <EventActionIcon name="cancel" />
            </button>
            <button
              type="button"
              class="ghost danger lightbox-action"
              aria-label="Confirm delete"
              title="Confirm delete"
              disabled={deleting}
              onClick={() => void deleteCurrent()}
            >
              <EventActionIcon name="confirm" />
            </button>
          </>
        ) : (
          <>
            <button
              type="button"
              class="ghost lightbox-action"
              aria-label="Reply"
              title="Reply"
              onClick={() => closeThen(actions.onReply)}
            >
              <EventActionIcon name="reply" />
            </button>
            {actions.onOpenThread !== undefined && (
              <button
                type="button"
                class="ghost lightbox-action"
                aria-label="Thread"
                title="Thread"
                onClick={() => closeThen(actions.onOpenThread!)}
              >
                <EventActionIcon name="thread" />
              </button>
            )}
            <button
              type="button"
              class="ghost lightbox-action"
              aria-label="React"
              title="React"
              onClick={() => closeThen(actions.onReact)}
            >
              <EventActionIcon name="react" />
            </button>
            {actions.ownUserId !== null &&
              item.event.sender === actions.ownUserId && (
                <button
                  type="button"
                  class="ghost danger lightbox-action"
                  aria-label="Delete"
                  title="Delete"
                  onClick={() => setConfirmingDelete(true)}
                >
                  <EventActionIcon name="delete" />
                </button>
              )}
          </>
        )}
      </span>
    ) : undefined

  return (
    <MediaViewerContext.Provider value={viewer}>
      {children}
      {item !== null && item.media.url !== null && (
        <Lightbox
          label={item.media.caption ?? item.media.filename}
          caption={
            item.media.caption === null ? null : (
              <MediaCaption
                accountId={item.accountId ?? accountId}
                caption={item.media.caption}
                content={item.event.content}
              />
            )
          }
          restoreTo={restoreTo}
          paging={{
            index,
            total: sequence.length,
            run: runOf?.(item.eventId) ?? null,
            hasPrev: index > 0 || (atStart !== true && !loadingOlder),
            hasNext: index < sequence.length - 1,
            loadingOlder,
            atOldest: atStart === true,
            onPrev: () => step(-1),
            onNext: () => step(1),
          }}
          onClose={close}
          actions={contextualActions}
          saving={saving}
          saveError={shownSaveError}
          actionError={deleteError}
          onSave={
            saveable && isDownloadable(item.media)
              ? () => void save(item)
              : undefined
          }
        >
          {/* Keyed on the url so Preact swaps the element rather than
              mutating a live `<img>`, which would otherwise show the previous
              photo until the next one decodes. */}
          <LightboxImage
            key={item.media.url}
            accountId={item.accountId ?? accountId}
            media={item.media}
            onOutcome={setOutcome}
          />
          {preloadable.map((neighbour) => (
            <LightboxPreload
              key={neighbour.eventId}
              accountId={neighbour.accountId ?? accountId}
              mxcUrl={neighbour.media.url!}
            />
          ))}
        </Lightbox>
      )}
    </MediaViewerContext.Provider>
  )
}
