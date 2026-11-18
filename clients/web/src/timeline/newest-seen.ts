/**
 * Releasing an anchored view's read-state gate once it has *shown* the live end.
 *
 * A notification tap opens a room (or a thread) through `?event=<the notified
 * message>`, which is usually the newest message. Every read-state claim is
 * gated on the view not being anchored (`viewMayClaimReadState`), because an
 * anchor normally parks the view in history — a search hit, a permalink — and
 * claiming from there would mark messages read that were never displayed. A
 * tap lands at the live end instead, and the view has then shown everything an
 * unanchored open would have; it may claim the same.
 *
 * "Shown" is measured, not inferred: the view renders a marker just past its
 * list, and `observeNewestSeen` reports when that marker — the bottom edge of
 * the newest rendered row — is on screen. `atEnd` alone is not enough: once a
 * room summary is known the head gets gap-filled, so a view anchored days back
 * can have the newest events loaded while the reader looks at history.
 *
 * The room stream (`RoomPage`) and the thread panel each answer this for
 * themselves, with their own anchor, slice and scroller; the rule is shared so
 * a future condition cannot be added to one and missed in the other.
 */

/** What an anchored view reported seeing: the bottom of `eventId`'s row. */
export interface NewestSeen {
  /** Which view: account, room (and thread), so a reused page cannot carry it. */
  scope: string
  /** The `?event=` anchor the view was showing when it was seen. */
  anchor: string
  /** The newest rendered event whose row's bottom was on screen. */
  eventId: string
}

/**
 * Whether an anchored view has caught up past its anchor: it reaches the live
 * end, and the reader has had the bottom of its *current* newest rendered row
 * on screen. Comparing the id on every render is what keeps the release from
 * outliving what was seen — a message arriving below the fold, or a second
 * anchor in the same view, closes the gate again until it too is seen.
 */
export function anchorCaughtUp(view: {
  seen: NewestSeen | null
  scope: string
  anchor: string | null
  atEnd: boolean
  newestRenderedId: string | null
}): boolean {
  const { seen } = view
  return (
    view.anchor !== null &&
    view.atEnd &&
    view.newestRenderedId !== null &&
    seen !== null &&
    seen.scope === view.scope &&
    seen.anchor === view.anchor &&
    seen.eventId === view.newestRenderedId
  )
}

/**
 * Watch `marker` (an element just past the view's list) and call `onSeen` when
 * it is on screen within `root`, the view's scroller. Returns the disconnect.
 *
 * Attach a fresh observer per newest rendered id: a new observer's first
 * callback reports the current state rather than waiting for a change, so an
 * arrival that is already on screen is reported at once. The callback waits on
 * `ready` — the view's reveal of its anchor — and re-measures live geometry,
 * because intersections are computed at frame boundaries and a report taken
 * while mount effects were still moving the scroller would claim a bottom the
 * reader was never shown.
 */
export function observeNewestSeen(
  marker: Element,
  root: Element,
  ready: () => boolean,
  onSeen: () => void,
): () => void {
  if (typeof IntersectionObserver === 'undefined') {
    return () => {}
  }
  const observer = new IntersectionObserver(
    (entries) => {
      if (
        !marker.isConnected ||
        !entries.some((entry) => entry.isIntersecting) ||
        !ready()
      ) {
        return
      }
      const bounds = root.getBoundingClientRect()
      const rect = marker.getBoundingClientRect()
      if (rect.top <= bounds.bottom + 2 && rect.bottom >= bounds.top) {
        onSeen()
      }
    },
    { root },
  )
  observer.observe(marker)
  return () => observer.disconnect()
}
