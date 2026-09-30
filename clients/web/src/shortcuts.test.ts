import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  chordOf,
  currentPlatform,
  hint,
  isPrimaryModifier,
  isTypingTarget,
  keyAria,
  keyLabel,
  KEYS,
  shortcutLabel,
  SHORTCUTS,
  shortcutGroups,
} from './shortcuts'

afterEach(() => {
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})

/** A keydown that only carries what `chordOf` reads. */
function key(init: KeyboardEventInit & { key: string }): KeyboardEvent {
  return new KeyboardEvent('keydown', init)
}

describe('chordOf (ADR 0078)', () => {
  it('leaves unmodified keys as their printed key', () => {
    expect(chordOf(key({ key: 'Escape' }))).toBe('Escape')
    expect(chordOf(key({ key: 'ArrowUp' }))).toBe('ArrowUp')
    // Shift alone does not prefix: `shift+/` prints `?`, and that is the chord.
    expect(chordOf(key({ key: '?', shiftKey: true }))).toBe('?')
  })

  it('normalizes modified keys, lowercasing the key', () => {
    expect(chordOf(key({ key: 'k', ctrlKey: true }))).toBe('mod+k')
    // Ctrl+Shift+F reports `event.key === 'F'`.
    expect(chordOf(key({ key: 'F', ctrlKey: true, shiftKey: true }))).toBe(
      'mod+shift+f',
    )
    expect(chordOf(key({ key: 'ArrowDown', ctrlKey: true }))).toBe(
      'mod+arrowdown',
    )
  })

  it('treats Cmd as Ctrl so macOS gets the same chords', () => {
    expect(chordOf(key({ key: 'k', metaKey: true }))).toBe('mod+k')
    expect(chordOf(key({ key: 'b', metaKey: true }))).toBe('mod+b')
  })

  it('keeps alt distinct from mod', () => {
    expect(chordOf(key({ key: 'f', altKey: true }))).toBe('alt+f')
    expect(chordOf(key({ key: 'f', ctrlKey: true, altKey: true }))).toBe(
      'mod+alt+f',
    )
  })
})

describe('isTypingTarget', () => {
  it('recognizes the fields a bare-character chord must not steal', () => {
    expect(isTypingTarget(document.createElement('input'))).toBe(true)
    expect(isTypingTarget(document.createElement('textarea'))).toBe(true)
    expect(isTypingTarget(document.createElement('select'))).toBe(true)

    const editable = document.createElement('div')
    editable.contentEditable = 'true'
    // jsdom does not derive isContentEditable from the attribute.
    Object.defineProperty(editable, 'isContentEditable', { value: true })
    expect(isTypingTarget(editable)).toBe(true)
  })

  it('leaves everything else alone', () => {
    expect(isTypingTarget(document.createElement('div'))).toBe(false)
    expect(isTypingTarget(document.createElement('a'))).toBe(false)
    expect(isTypingTarget(null)).toBe(false)
  })
})

describe('SHORTCUTS', () => {
  it('is the single source the help popup renders', () => {
    const keys = SHORTCUTS.flatMap((group) =>
      group.rows.map((row) =>
        typeof row.keys === 'string' ? row.keys : row.keys.label,
      ),
    )
    expect(keys).toContain(KEYS.roomActions.label)
    expect(keys).toContain(KEYS.filterRooms.label)
    expect(keys).toContain(KEYS.cycleFilter.label)
    expect(keys).toContain(KEYS.toggleSpaces.label)
    expect(keys).toContain(KEYS.spaceStep.label)
    expect(keys).toContain(KEYS.showHelp.label)
    // Every row is documented; an empty description is a drift bug.
    for (const group of SHORTCUTS) {
      for (const row of group.rows) {
        expect(row.description.length).toBeGreaterThan(0)
      }
    }
  })

  it('documents favourite reorder on the room list', () => {
    const rooms = SHORTCUTS.find((group) => group.group === 'Rooms')
    expect(rooms?.rows.map((row) => row.keys)).toContain(KEYS.reorderFavorites)
    expect(rooms?.rows.map((row) => row.description)).toContain(
      'Move the focused room up or down in the favourite list',
    )
  })

  it('groups all space actions under Spaces', () => {
    const spaces = SHORTCUTS.find((group) => group.group === 'Spaces')
    expect(spaces?.rows.map((row) => row.keys)).toEqual([
      KEYS.toggleSpaces,
      KEYS.spaceStep,
      KEYS.reorderSpaces,
      'Alt-↑ / Alt-↓',
    ])
  })
})

describe('help chords', () => {
  it('Ctrl-/ normalizes across the layouts that report ? for shift-slash', () => {
    // A US layout reports `?` for Ctrl+Shift+/; others report `/`.
    expect(chordOf(key({ key: '/', ctrlKey: true }))).toBe('mod+/')
    expect(chordOf(key({ key: '/', ctrlKey: true, shiftKey: true }))).toBe(
      'mod+shift+/',
    )
    expect(chordOf(key({ key: '?', ctrlKey: true, shiftKey: true }))).toBe(
      'mod+shift+?',
    )
  })

  it('advertises both spellings, because ? cannot fire while typing', () => {
    expect(KEYS.showHelp.label).toContain('?')
    expect(KEYS.showHelp.label).toContain('Ctrl-/')
    // A bare `?` is withheld in a text field; the modifier chord is not.
    expect(isTypingTarget(document.createElement('textarea'))).toBe(true)
  })
})

describe('hint', () => {
  it('suffixes a label with its chord', () => {
    expect(hint('Hide rooms', KEYS.toggleSidebar)).toBe('Hide rooms (Ctrl-B)')
  })

  it('formats shortcut labels for Apple platforms', () => {
    expect(shortcutLabel(KEYS.toggleSidebar.label, 'MacIntel')).toBe('⌘-B')
    expect(keyLabel(KEYS.showHelp, 'iPhone')).toBe('? or ⌘-/')
    expect(keyLabel(KEYS.search, 'MacIntel')).toBe('/ or ⌘-G')
    expect(keyLabel(KEYS.roomStep, 'MacIntel')).toBe('⌘-Option-↑ / ⌘-Option-↓')
    expect(keyLabel(KEYS.spaceStep, 'MacIntel')).toBe('⌘-Option-[ / ⌘-Option-]')
    expect(shortcutLabel(KEYS.toggleSidebar.label, 'Win32')).toBe('Ctrl-B')
    expect(keyLabel(KEYS.roomStep, 'Win32')).toBe('Ctrl-↑ / Ctrl-↓')
  })

  it('prefers User-Agent Client Hints when available', () => {
    vi.stubGlobal('navigator', {
      ...navigator,
      userAgent: 'Mozilla/5.0 (X11; Linux x86_64)',
      userAgentData: { platform: 'macOS' },
    })

    expect(currentPlatform()).toBe('macOS')
    expect(shortcutLabel(KEYS.toggleSidebar.label)).toBe('⌘-B')
  })
})

describe('native-shell chords (ADR 0107)', () => {
  it('swaps in the platform-standard chord only inside the shell', () => {
    expect(keyLabel(KEYS.search, 'Win32', false)).toBe('/ or Ctrl-Shift-F')
    expect(keyLabel(KEYS.search, 'Win32', true)).toBe('/ or Ctrl-F')
    expect(keyLabel(KEYS.search, 'MacIntel', false)).toBe('/ or ⌘-G')
    expect(keyLabel(KEYS.search, 'MacIntel', true)).toBe('/ or ⌘-F')
    expect(keyAria(KEYS.search, 'MacIntel', true)).toBe('/ Meta+F')
    expect(keyLabel(KEYS.startDm, 'Linux x86_64', true)).toBe('Ctrl-N')
    expect(keyLabel(KEYS.startDm, 'MacIntel', true)).toBe('⌘-N')
    expect(keyLabel(KEYS.showHelp, 'Win32', true)).toBe('? or F1')
    expect(keyLabel(KEYS.showHelp, 'MacIntel', true)).toBe('? or ⌘-?')
    // No override: the shell binds the same chord the browser does.
    expect(keyLabel(KEYS.toggleSidebar, 'MacIntel', true)).toBe('⌘-B')
  })

  it('hides shell-only rows from a browser', () => {
    const rows = (native: boolean) =>
      shortcutGroups(native).flatMap(({ rows }) => rows)
    const settings = (native: boolean) =>
      rows(native).filter((row) => row.keys === KEYS.openSettings)
    expect(settings(true)).toHaveLength(1)
    expect(settings(false)).toHaveLength(0)
    expect(rows(true).length).toBe(rows(false).length + 1)
  })

  it('reads the primary modifier the platform actually means', () => {
    const cmd = { ctrlKey: false, metaKey: true }
    const ctrl = { ctrlKey: true, metaKey: false }
    const both = { ctrlKey: true, metaKey: true }
    expect(isPrimaryModifier(cmd, true)).toBe(true)
    expect(isPrimaryModifier(ctrl, true)).toBe(false)
    expect(isPrimaryModifier(both, true)).toBe(false)
    expect(isPrimaryModifier(ctrl, false)).toBe(true)
    expect(isPrimaryModifier(cmd, false)).toBe(false)
  })
})
