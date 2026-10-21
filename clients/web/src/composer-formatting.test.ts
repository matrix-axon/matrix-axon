import { describe, expect, it } from 'vitest'
import {
  applyFormatEdit,
  formatEdit,
  formatKindForEvent,
  type FormatKind,
} from './composer-formatting'

function key(init: KeyboardEventInit & { key: string }): KeyboardEvent {
  return new KeyboardEvent('keydown', init)
}

/** Run an edit over `text` the way the composer does and show the result. */
function run(kind: FormatKind, text: string, start: number, end = start) {
  const edit = formatEdit(kind, text, start, end)
  const next = text.slice(0, edit.from) + edit.insert + text.slice(edit.to)
  return {
    next,
    selected: next.slice(edit.selectionStart, edit.selectionEnd),
    caret: edit.selectionStart,
  }
}

describe('formatKindForEvent (ADR 0123)', () => {
  it('uses Ctrl off Apple platforms and Command on them', () => {
    expect(formatKindForEvent(key({ key: 'b', ctrlKey: true }), false)).toBe(
      'bold',
    )
    expect(formatKindForEvent(key({ key: 'b', metaKey: true }), false)).toBe(
      null,
    )
    expect(formatKindForEvent(key({ key: 'i', metaKey: true }), true)).toBe(
      'italic',
    )
    // `Ctrl-I` is an Emacs cursor key on macOS and must reach the field.
    expect(formatKindForEvent(key({ key: 'i', ctrlKey: true }), true)).toBe(
      null,
    )
  })

  it('maps every format, with Shift for strike and link only', () => {
    const kind = (k: string, shiftKey = false) =>
      formatKindForEvent(key({ key: k, ctrlKey: true, shiftKey }), false)
    expect(kind('e')).toBe('code')
    expect(kind('x', true)).toBe('strike')
    expect(kind('x')).toBe(null) // plain Ctrl-X is cut
    expect(kind('k', true)).toBe('link')
    expect(kind('k')).toBe(null) // plain Ctrl-K is the room filter
    expect(kind('b', true)).toBe(null) // the sidebar toggle's twin
  })

  it('follows the printed key, shifted or not', () => {
    expect(
      formatKindForEvent(
        key({ key: 'X', ctrlKey: true, shiftKey: true }),
        false,
      ),
    ).toBe('strike')
  })

  it('leaves keydowns to the IME while a composition is open', () => {
    expect(
      formatKindForEvent(
        key({ key: 'b', ctrlKey: true, isComposing: true }),
        false,
      ),
    ).toBe(null)
    // Engines that flag the composing keydown only by its legacy keyCode.
    expect(
      formatKindForEvent(key({ key: 'b', ctrlKey: true, keyCode: 229 }), false),
    ).toBe(null)
  })

  it('ignores AltGr, which arrives as Ctrl+Alt', () => {
    expect(
      formatKindForEvent(key({ key: 'e', ctrlKey: true, altKey: true }), false),
    ).toBe(null)
  })
})

describe('formatEdit', () => {
  it('wraps a selection and keeps it selected', () => {
    expect(run('bold', 'say hi now', 4, 6)).toMatchObject({
      next: 'say **hi** now',
      selected: 'hi',
    })
    expect(run('strike', 'a b', 2, 3).next).toBe('a ~~b~~')
    expect(run('code', 'run ls', 4, 6).next).toBe('run `ls`')
  })

  it('inserts an empty pair with the caret between when nothing is selected', () => {
    expect(run('italic', 'ab', 1)).toMatchObject({
      next: 'a**b',
      caret: 2,
      selected: '',
    })
  })

  it('unwraps when the markers surround the selection', () => {
    expect(run('bold', 'say **hi** now', 6, 8)).toMatchObject({
      next: 'say hi now',
      selected: 'hi',
    })
    expect(run('code', '`x`', 1, 2).next).toBe('x')
  })

  it('unwraps when the selection includes the markers', () => {
    expect(run('bold', 'say **hi** now', 4, 10)).toMatchObject({
      next: 'say hi now',
      selected: 'hi',
    })
    expect(run('strike', '~~hi~~', 0, 6).next).toBe('hi')
  })

  it('unwraps an empty pair the caret sits in', () => {
    expect(run('bold', 'a****b', 3).next).toBe('ab')
  })

  it('tells bold from italic when both use asterisks', () => {
    // `**x**` is not italic: italicizing adds a third marker rather than
    // stripping one.
    expect(run('italic', '**x**', 2, 3).next).toBe('***x***')
    expect(run('bold', '*x*', 1, 2).next).toBe('***x***')
    // And each can be taken off a bold-italic run without hurting the other.
    expect(run('italic', '***x***', 3, 4).next).toBe('**x**')
    expect(run('bold', '***x***', 3, 4).next).toBe('*x*')
  })

  it('does not mistake a lone marker on one side for a pair', () => {
    expect(run('bold', 'a **b', 4, 4).next).toBe('a ******b')
    expect(run('italic', '2 * x', 4, 5).next).toBe('2 * *x*')
  })

  it('keeps whitespace at the edges of the selection outside the markers', () => {
    expect(run('bold', 'hello world', 0, 6)).toMatchObject({
      next: '**hello** world',
      selected: 'hello',
    })
    expect(run('italic', 'a  hi\n', 1, 6).next).toBe('a  *hi*\n')
    expect(run('link', 'hello world', 5, 11).next).toBe('hello [world](url)')
    // Only whitespace selected: nothing to format, so it acts as a caret.
    expect(run('bold', 'a  b', 1, 3)).toMatchObject({
      next: 'a  ****b',
      caret: 5,
    })
  })

  it('unwraps a selection that has whitespace around the formatted text', () => {
    expect(run('bold', 'say **hi** now', 3, 11).next).toBe('say hi now')
  })

  it('sizes the code delimiter to the content', () => {
    expect(run('code', 'a`b', 0, 3)).toMatchObject({
      next: '``a`b``',
      selected: 'a`b',
    })
    // A run the same length as the shorter delimiter forces the next size up.
    expect(run('code', 'a``b`c', 0, 6).next).toBe('```a``b`c```')
    // Content that touches the delimiter gets a padding space.
    expect(run('code', '`a', 0, 2)).toMatchObject({
      next: '`` `a ``',
      selected: '`a',
    })
  })

  it('removes the whole code delimiter when unwrapping', () => {
    expect(run('code', '``a`b``', 2, 5)).toMatchObject({
      next: 'a`b',
      selected: 'a`b',
    })
    expect(run('code', '``a`b``', 0, 7).next).toBe('a`b')
    // The padding space goes with it.
    expect(run('code', '`` `a ``', 3, 5).next).toBe('`a')
    expect(run('code', '`` `a ``', 0, 8).next).toBe('`a')
    // A plain span still loses its single tick.
    expect(run('code', 'x `y` z', 3, 4).next).toBe('x y z')
  })

  it('makes a link, leaving the URL placeholder selected', () => {
    expect(run('link', 'see docs here', 4, 8)).toMatchObject({
      next: 'see [docs](url) here',
      selected: 'url',
    })
    expect(run('link', 'ab', 1)).toMatchObject({
      next: 'a[link](url)b',
      selected: 'url',
    })
  })
})

describe('applyFormatEdit', () => {
  it('edits through the input event when execCommand is unavailable', () => {
    const textarea = document.createElement('textarea')
    document.body.append(textarea)
    textarea.value = 'hello world'
    const seen: string[] = []
    textarea.addEventListener('input', () => seen.push(textarea.value))
    applyFormatEdit(textarea, formatEdit('bold', textarea.value, 6, 11))
    expect(textarea.value).toBe('hello **world**')
    expect(seen).toEqual(['hello **world**'])
    expect(
      textarea.value.slice(textarea.selectionStart, textarea.selectionEnd),
    ).toBe('world')
    textarea.remove()
  })

  it('goes through execCommand when the browser has it, to keep undo', () => {
    const textarea = document.createElement('textarea')
    document.body.append(textarea)
    textarea.value = 'ab'
    const calls: unknown[][] = []
    document.execCommand = (...args: unknown[]) => {
      calls.push(args)
      return true
    }
    try {
      applyFormatEdit(textarea, formatEdit('bold', 'ab', 0, 2))
      expect(calls).toEqual([['insertText', false, '**ab**']])
      // `execCommand` did the edit, so the value was not touched directly.
      expect(textarea.value).toBe('ab')
    } finally {
      delete (document as { execCommand?: unknown }).execCommand
      textarea.remove()
    }
  })
})
