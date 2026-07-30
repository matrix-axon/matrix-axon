import { useEffect, useRef } from 'preact/hooks'

/**
 * Keyboard shortcuts (ADR 0078).
 *
 * The TUI's chords cannot be mirrored literally: its room-list bindings are
 * all `Alt`-modified (ADR 0042) because a terminal has one focused pane and
 * unmodified characters must still reach the compose box — and in a browser
 * `Alt-F` opens Chrome's menu, `Alt-D` focuses the address bar, and
 * `Ctrl-N`/`Ctrl-P` cannot be intercepted from a page at all. We keep the TUI's
 * *semantics* (cycle order, staged Escape) and pick browser-safe chords.
 *
 * `mod` is Ctrl on Windows/Linux and Command on macOS, so `mod+k` is the
 * familiar Command-K there. Bare-character chords (`?`) only fire outside text
 * fields.
 */

/** A chord: modifiers in `mod+alt+shift+` order, then the lowercased key. */
export type Chord = string

/**
 * Normalize a keydown into a chord string.
 *
 * Unmodified keys are their raw `event.key` (`Escape`, `ArrowUp`, `?`) so that
 * a shifted character stays the character it prints — `shift+/` is `?`, not
 * `shift+?`. Modified keys lowercase the key, so `Ctrl+Shift+F` is
 * `mod+shift+f` regardless of the shifted `event.key` being `F`.
 */
export function chordOf(event: KeyboardEvent): Chord {
  const mod = event.ctrlKey || event.metaKey
  if (!mod && !event.altKey) {
    return event.key
  }
  const parts: string[] = []
  if (mod) {
    parts.push('mod')
  }
  if (event.altKey) {
    parts.push('alt')
  }
  if (event.shiftKey) {
    parts.push('shift')
  }
  parts.push(event.key.toLowerCase())
  return parts.join('+')
}

/**
 * Does this keydown carry any modifier?
 *
 * A handler bound to a bare key (`ArrowUp`) must check this before claiming the
 * event. `Ctrl-↑` and `↑` are different chords, but `event.key` is `ArrowUp`
 * for both — so an unguarded `key === 'ArrowUp'` swallows the chord, and
 * `preventDefault()` then hides it from `useShortcuts` entirely.
 */
export function hasModifier(event: {
  ctrlKey: boolean
  metaKey: boolean
  altKey: boolean
  shiftKey: boolean
}): boolean {
  return event.ctrlKey || event.metaKey || event.altKey || event.shiftKey
}

/**
 * Is the event aimed at somewhere the user is typing? Bare-character chords
 * must not fire there, or `?` could never be typed into a message.
 */
export function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) {
    return false
  }
  return (
    target.isContentEditable ||
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement
  )
}

export interface ShortcutOptions {
  /**
   * Fire even when the user is typing. Chords with a modifier want this
   * (`mod+k` from the composer); bare-character chords never do.
   */
  whileTyping?: boolean
  /**
   * Listen in the capture phase, so this handler runs before bubble-phase
   * ones no matter the mount order. Modals use it to claim Escape first.
   */
  capture?: boolean
}

export interface ShortcutKey {
  label: string
  aria: string
  appleLabel?: string
  appleAria?: string
}

/** App-wide event used by composer commands to request the shell help dialog. */
export const SHOW_HELP_EVENT = 'axon:show-help'

/**
 * Bind chords to handlers for as long as the component is mounted.
 *
 * A handler that acts should `preventDefault()`; other handlers skip an event
 * that is already `defaultPrevented`, which is how the staged Escape resolves
 * (modal, then thread panel, then composer) without any of them knowing about
 * the others.
 */
export function useShortcuts(
  bindings: Record<Chord, (event: KeyboardEvent) => void>,
  options: ShortcutOptions = {},
): void {
  const { whileTyping = false, capture = false } = options

  // Subscribe once and read the newest bindings through a ref. Re-subscribing
  // per render looks equivalent but is not: Preact flushes effects *after
  // paint*, so a key pressed before the flush would run the previous render's
  // closure over stale state — a toggle bound this way would refuse to toggle
  // back.
  const latest = useRef(bindings)
  latest.current = bindings

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented) {
        return
      }
      const handler = latest.current[chordOf(event)]
      if (handler === undefined) {
        return
      }
      if (!whileTyping && isTypingTarget(event.target)) {
        return
      }
      handler(event)
    }
    document.addEventListener('keydown', onKeyDown, capture)
    return () => document.removeEventListener('keydown', onKeyDown, capture)
  }, [whileTyping, capture])
}

/** One row of the help popup. */
export interface ShortcutHelp {
  keys: string | ShortcutKey
  description: string
}

/**
 * The chords a control can advertise on itself, each with the label we print
 * and the `aria-keyshortcuts` spelling ARIA requires. Tooltips, the help popup
 * and the bindings all read these, so a chord cannot be renamed in one place
 * and left stale in another.
 */
export const KEYS = {
  roomActions: { label: '+', aria: '+' },
  filterRooms: { label: 'Ctrl-K', aria: 'Control+K' },
  cycleFilter: { label: 'Ctrl-Shift-Y', aria: 'Control+Shift+Y' },
  cycleSort: { label: 'Ctrl-Shift-S', aria: 'Control+Shift+S' },
  startDm: {
    label: 'Ctrl-Alt-M',
    aria: 'Control+Alt+M',
    appleLabel: '⌘-Option-M',
    appleAria: 'Meta+Alt+M',
  },
  toggleSidebar: { label: 'Ctrl-B', aria: 'Control+B' },
  toggleSpaces: {
    label: 'Ctrl-Alt-S',
    aria: 'Control+Alt+S',
    appleLabel: '⌘-Option-S',
    appleAria: 'Meta+Alt+S',
  },
  /**
   * Reordering controls are hidden by default: the rail is deliberately narrow
   * and a per-space pair of arrows would cost every row vertical space for a
   * rare action. This chord surfaces them for pointer and touch users; keyboard
   * users can move a focused space with Alt-↑/↓ without leaving it on.
   */
  reorderSpaces: {
    label: 'Ctrl-Alt-R',
    aria: 'Control+Alt+R',
    appleLabel: '⌘-Option-R',
    appleAria: 'Meta+Alt+R',
  },
  spaceStep: {
    label: 'Ctrl-Alt-[ / Ctrl-Alt-]',
    aria: 'Control+Alt+[ Control+Alt+]',
    appleLabel: '⌘-Option-[ / ⌘-Option-]',
    appleAria: 'Meta+Alt+[ Meta+Alt+]',
  },
  /**
   * `?` cannot fire while typing — it has to reach the composer as a character
   * — and the composer is where focus usually is. So help also answers to
   * `Ctrl-/`, a modifier chord that survives a text field.
   */
  showHelp: { label: '? or Ctrl-/', aria: '? Control+/' },
  /**
   * Search follows the same bare-plus-modifier-twin pattern (ADR 0066): `/`
   * is the GitHub/Zulip convention but can never fire from the composer, so
   * the modifier twin has to be browser-safe too. Windows/Linux can use
   * `Ctrl-Shift-F`; macOS keeps `Cmd-G` because `Cmd-Shift-F` is already
   * claimed in modern browsers.
   */
  search: {
    label: '/ or Ctrl-Shift-F',
    aria: '/ Control+Shift+F',
    appleLabel: '/ or ⌘-G',
    appleAria: '/ Meta+G',
  },
  roomStep: {
    label: 'Ctrl-↑ / Ctrl-↓',
    aria: 'Control+ArrowUp Control+ArrowDown',
    appleLabel: '⌘-Option-↑ / ⌘-Option-↓',
    appleAria: 'Meta+Alt+ArrowUp Meta+Alt+ArrowDown',
  },
} as const

type NavigatorWithUserAgentData = Navigator & {
  userAgentData?: {
    platform?: string
  }
}

export function currentPlatform(): string {
  return (
    (navigator as NavigatorWithUserAgentData).userAgentData?.platform ??
    navigator.userAgent
  )
}

export function isApplePlatform(
  platform = currentPlatform(),
  touchPoints = navigator.maxTouchPoints ?? 0,
): boolean {
  return (
    /\b(Mac|iPhone|iPad|iPod|macOS|iOS)/i.test(platform) ||
    (/Macintosh/i.test(platform) && touchPoints > 1)
  )
}

export function shortcutLabel(
  label: string,
  platform = currentPlatform(),
): string {
  return isApplePlatform(platform) ? label.replaceAll('Ctrl', '⌘') : label
}

export function keyLabel(
  key: ShortcutKey,
  platform = currentPlatform(),
): string {
  if (isApplePlatform(platform)) {
    return key.appleLabel ?? shortcutLabel(key.label, platform)
  }
  return key.label
}

export function keyAria(
  key: ShortcutKey,
  platform = currentPlatform(),
): string {
  if (isApplePlatform(platform)) {
    return key.appleAria ?? key.aria.replaceAll('Control', 'Meta')
  }
  return key.aria
}

/** `hint('Hide rooms', KEYS.toggleSidebar)` → `Hide rooms (Ctrl-B)`. */
export function hint(text: string, key: ShortcutKey): string {
  return `${text} (${keyLabel(key)})`
}

/**
 * The canonical, user-facing shortcut list. `ShortcutsHelp` renders exactly
 * this, so the help popup cannot drift from what is bound — the web analogue
 * of the TUI's `popup_shortcuts_lines` (ui.rs), which has to be kept in sync
 * by hand.
 */
export const SHORTCUTS: { group: string; rows: ShortcutHelp[] }[] = [
  {
    group: 'Rooms',
    rows: [
      { keys: KEYS.roomActions, description: 'Open room actions' },
      { keys: KEYS.filterRooms.label, description: 'Filter rooms by name' },
      { keys: '↑ / ↓', description: 'Move through the room list' },
      { keys: 'Enter', description: 'Open the selected room' },
      { keys: KEYS.roomStep, description: 'Previous / next room' },
      { keys: KEYS.startDm, description: 'Start direct message' },
      {
        keys: KEYS.cycleFilter,
        description: 'Cycle filter: all, DMs, groups, unread, favorites',
      },
      {
        keys: KEYS.cycleSort,
        description: 'Cycle sort: recent, oldest, A–Z, Z–A',
      },
      {
        keys: KEYS.toggleSidebar,
        description: 'Show or hide the room list',
      },
    ],
  },
  {
    group: 'Spaces',
    rows: [
      { keys: KEYS.toggleSpaces, description: 'Show or hide spaces' },
      { keys: KEYS.spaceStep, description: 'Previous / next space' },
      {
        keys: KEYS.reorderSpaces,
        description: 'Show or hide the space reordering controls',
      },
      {
        keys: 'Alt-↑ / Alt-↓',
        description: 'Move the focused space up or down',
      },
    ],
  },
  {
    group: 'Messages',
    rows: [
      { keys: 'Shift-Enter', description: 'Insert a line break' },
      { keys: ':emoji-shortcode:', description: 'Insert an emoji' },
      { keys: '↑', description: 'Edit your last message (empty composer)' },
      { keys: 'Escape', description: 'Cancel reply or edit' },
      {
        keys: 'Ctrl-Shift-. / Ctrl-Shift-,',
        description: 'Grow / shrink the message box',
      },
      { keys: 'Ctrl-Shift-0', description: 'Reset the message box height' },
    ],
  },
  {
    group: 'Everywhere',
    rows: [
      {
        keys: 'Escape',
        description: 'Close the open panel, then return to the composer',
      },
      { keys: KEYS.search, description: 'Search messages' },
      { keys: KEYS.showHelp, description: 'Show this list' },
    ],
  },
]
