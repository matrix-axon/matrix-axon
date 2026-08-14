import { useEffect, useState } from 'preact/hooks'

/**
 * Which of the three shell layouts the current path wants (ADR 0062):
 * `room`, `rooms`, and `room-entry` are the two-pane desktop surfaces
 * (sidebar + right pane), while `utility` is the full-width centered column
 * with no sidebar at all. `room-entry` becomes main-only on mobile so the
 * add-room/settings panes remain reachable from the room list route.
 *
 * Lives apart from `app.tsx` so the room list can consult it — the shell
 * imports the room list, so the room list cannot import the shell.
 */
export type LayoutMode = 'room' | 'rooms' | 'room-entry' | 'utility'

/**
 * The single-pane media query (ADR 0062): below this width only one pane
 * shows, chosen by the route. Must stay in lockstep with the `47.99rem` /
 * `48rem` breakpoints in `index.css` — this constant exists so JS callers
 * (`RoomPage`'s focus handoff) can't drift from the stylesheet (WCR-17).
 */
export const SINGLE_PANE_QUERY = '(max-width: 47.99rem)'

const ROOM_PATH = /^\/[^/]+\/rooms\/.+/

export function layoutMode(path: string): LayoutMode {
  if (ROOM_PATH.test(path)) {
    return 'room'
  }
  if (path === '/') {
    return 'rooms'
  }
  return path === '/rooms/discover' ||
    path === '/rooms/dm' ||
    path === '/rooms/create' ||
    path === '/invites' ||
    path === '/settings'
    ? 'room-entry'
    : 'utility'
}

export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(() => window.matchMedia(query).matches)

  useEffect(() => {
    const media = window.matchMedia(query)
    const onChange = () => setMatches(media.matches)
    setMatches(media.matches)
    media.addEventListener('change', onChange)
    return () => media.removeEventListener('change', onChange)
  }, [query])

  return matches
}
