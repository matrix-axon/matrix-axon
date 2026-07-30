import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  chordOf,
  currentPlatform,
  hint,
  isTypingTarget,
  keyLabel,
  KEYS,
  shortcutLabel,
  SHORTCUTS,
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
