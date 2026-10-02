import {
  act,
  cleanup,
  fireEvent,
  render,
  waitFor,
} from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { Composer } from './Composer'

const originalMatchMedia = Object.getOwnPropertyDescriptor(window, 'matchMedia')
const originalTextareaScrollHeight = Object.getOwnPropertyDescriptor(
  HTMLTextAreaElement.prototype,
  'scrollHeight',
)

afterEach(() => {
  cleanup()
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
  if (originalMatchMedia === undefined) {
    delete (window as { matchMedia?: typeof window.matchMedia }).matchMedia
  } else {
    Object.defineProperty(window, 'matchMedia', originalMatchMedia)
  }
  if (originalTextareaScrollHeight === undefined) {
    delete (HTMLTextAreaElement.prototype as { scrollHeight?: number })
      .scrollHeight
  } else {
    Object.defineProperty(
      HTMLTextAreaElement.prototype,
      'scrollHeight',
      originalTextareaScrollHeight,
    )
  }
})

function mobileMatchMedia(matches: boolean): typeof window.matchMedia {
  return vi.fn().mockImplementation((query: string) => ({
    matches,
    media: query,
    onchange: null,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    addListener: vi.fn(),
    removeListener: vi.fn(),
    dispatchEvent: vi.fn(),
  }))
}

function renderComposer(props: Partial<Parameters<typeof Composer>[0]> = {}) {
  const onSubmit = vi.fn(async () => true)
  const onDraftChange = vi.fn()
  const utils = render(
    <Composer
      placeholder="Message Ops"
      onSubmit={onSubmit}
      onDraftChange={onDraftChange}
      {...props}
    />,
  )
  const textarea = utils.getByRole('textbox') as HTMLTextAreaElement
  const form = textarea.closest('form')!
  return { onSubmit, onDraftChange, textarea, form, ...utils }
}

describe('Composer drafts (M-W6 step 5b)', () => {
  it('reports each edit via onDraftChange', () => {
    const { textarea, onDraftChange } = renderComposer()
    fireEvent.input(textarea, { target: { value: 'hi there' } })
    expect(onDraftChange).toHaveBeenCalledWith('hi there')
    expect(textarea.value).toBe('hi there')
  })

  it('does not persist slash commands as drafts, except escaped literal slashes', () => {
    const { textarea, onDraftChange } = renderComposer({ onCommand: vi.fn() })

    fireEvent.input(textarea, { target: { value: '/react' } })
    expect(onDraftChange).not.toHaveBeenCalled()

    fireEvent.input(textarea, { target: { value: '//react' } })
    expect(onDraftChange).toHaveBeenCalledWith('//react')
  })

  it('sends and persists slashed text verbatim when commands are disabled', () => {
    // The thread composer takes no `onCommand`: there, `/` is just a character,
    // and intercepting it would silently swallow the reply.
    const { textarea, form, onSubmit, onDraftChange } = renderComposer()

    fireEvent.input(textarea, { target: { value: '/shrug' } })
    expect(onDraftChange).toHaveBeenCalledWith('/shrug')

    fireEvent.submit(form)
    expect(onSubmit).toHaveBeenCalledWith('/shrug')
    expect(textarea.value).toBe('')

    fireEvent.input(textarea, { target: { value: '//shrug' } })
    fireEvent.submit(form)
    expect(onSubmit).toHaveBeenLastCalledWith('//shrug')
  })

  it('clears the persisted draft on send', () => {
    const { textarea, form, onSubmit, onDraftChange } = renderComposer()
    fireEvent.input(textarea, { target: { value: 'ship it' } })
    fireEvent.submit(form)
    expect(onSubmit).toHaveBeenCalledWith('ship it')
    expect(onDraftChange).toHaveBeenLastCalledWith('')
    expect(textarea.value).toBe('')
  })

  it('renders an inline status near the input', () => {
    const { getByRole } = renderComposer({ status: 'Joining #ops:hs…' })

    expect(getByRole('status').textContent).toBe('Joining #ops:hs…')
  })

  it('intercepts slash commands without sending them as messages', () => {
    const onCommand = vi.fn(() => true)
    const { textarea, form, onSubmit, onDraftChange } = renderComposer({
      onCommand,
    })
    fireEvent.input(textarea, { target: { value: '/react' } })
    fireEvent.submit(form)

    expect(onCommand).toHaveBeenCalledWith('/react')
    expect(onSubmit).not.toHaveBeenCalled()
    expect(onDraftChange).toHaveBeenLastCalledWith('')
    expect(textarea.value).toBe('')
  })

  it('leaves an unhandled slash command in the composer', () => {
    const onCommand = vi.fn(() => false)
    const { textarea, form, onSubmit, onDraftChange } = renderComposer({
      onCommand,
    })
    fireEvent.input(textarea, { target: { value: '/unknown' } })
    fireEvent.submit(form)

    expect(onCommand).toHaveBeenCalledWith('/unknown')
    expect(onSubmit).not.toHaveBeenCalled()
    expect(onDraftChange).not.toHaveBeenCalledWith('')
    expect(textarea.value).toBe('/unknown')
  })

  it('waits for async slash commands before clearing the composer', async () => {
    const onCommand = vi.fn(async () => true)
    const { textarea, form, onSubmit, onDraftChange } = renderComposer({
      onCommand,
    })
    fireEvent.input(textarea, { target: { value: '/leave' } })
    fireEvent.submit(form)

    expect(onCommand).toHaveBeenCalledWith('/leave')
    expect(onSubmit).not.toHaveBeenCalled()
    expect(textarea.value).toBe('/leave')
    await vi.waitFor(() => expect(textarea.value).toBe(''))
    expect(onDraftChange).toHaveBeenLastCalledWith('')
  })

  it('keeps the composer recoverable when an async slash command rejects', async () => {
    const onCommand = vi.fn(async () => {
      throw new Error('command failed')
    })
    const { textarea, form, onSubmit, onDraftChange } = renderComposer({
      onCommand,
    })
    fireEvent.input(textarea, { target: { value: '/leave' } })
    fireEvent.submit(form)

    expect(onCommand).toHaveBeenCalledWith('/leave')
    expect(onSubmit).not.toHaveBeenCalled()
    await vi.waitFor(() => expect(onCommand).toHaveBeenCalledTimes(1))
    expect(onDraftChange).not.toHaveBeenCalledWith('')
    expect(textarea.value).toBe('/leave')
  })

  it('sends double-slash input as a literal leading slash message', () => {
    const onCommand = vi.fn(() => true)
    const { textarea, form, onSubmit } = renderComposer({ onCommand })
    fireEvent.input(textarea, { target: { value: '//react' } })
    fireEvent.submit(form)

    expect(onCommand).not.toHaveBeenCalled()
    expect(onSubmit).toHaveBeenCalledWith('/react')
  })

  it('adopts a draft that hydrates after mount while untouched', () => {
    const { textarea, rerender } = renderComposer({ initialValue: '' })
    expect(textarea.value).toBe('')
    rerender(
      <Composer
        placeholder="Message Ops"
        initialValue="from another device"
        onSubmit={vi.fn()}
        onDraftChange={vi.fn()}
      />,
    )
    expect(textarea.value).toBe('from another device')
  })

  it('adopts a sibling device clearing the draft', () => {
    const { textarea, rerender } = renderComposer({
      initialValue: 'from another device',
    })
    expect(textarea.value).toBe('from another device')
    rerender(
      <Composer
        placeholder="Message Ops"
        initialValue=""
        onSubmit={vi.fn()}
        onDraftChange={vi.fn()}
      />,
    )
    expect(textarea.value).toBe('')
  })

  /**
   * `Ctrl-↑` is "previous room" (ADR 0078). The composer holds focus most of
   * the time, so an unguarded `key === 'ArrowUp'` here claimed the chord: it
   * opened an edit *and* — by calling `preventDefault()` — stopped the room
   * from changing, since `useShortcuts` ignores a defaultPrevented event.
   */
  it('edits the last message on a bare ArrowUp, not a modified one', () => {
    const onEditLast = vi.fn()
    const { textarea } = renderComposer({ onEditLast })

    for (const modifier of [
      { ctrlKey: true },
      { metaKey: true },
      { altKey: true },
      { shiftKey: true },
    ]) {
      const event = new KeyboardEvent('keydown', {
        key: 'ArrowUp',
        bubbles: true,
        cancelable: true,
        ...modifier,
      })
      textarea.dispatchEvent(event)
      expect(onEditLast).not.toHaveBeenCalled()
      // The chord has to stay visible to the document-level shortcut layer.
      expect(event.defaultPrevented).toBe(false)
    }

    fireEvent.keyDown(textarea, { key: 'ArrowUp' })
    expect(onEditLast).toHaveBeenCalledTimes(1)
  })

  it('never clobbers in-progress typing with a late draft', () => {
    const { textarea, rerender } = renderComposer({ initialValue: '' })
    fireEvent.input(textarea, { target: { value: 'my words' } })
    rerender(
      <Composer
        placeholder="Message Ops"
        initialValue="late draft"
        onSubmit={vi.fn()}
        onDraftChange={vi.fn()}
      />,
    )
    expect(textarea.value).toBe('my words')
  })

  it('does not grow an empty mobile composer to fit a long placeholder', () => {
    Object.defineProperty(window, 'matchMedia', {
      configurable: true,
      value: mobileMatchMedia(true),
    })
    Object.defineProperty(HTMLTextAreaElement.prototype, 'scrollHeight', {
      configurable: true,
      get: () => 120,
    })

    const { textarea } = renderComposer({
      placeholder: 'Message a very long room name that would wrap on a phone',
    })

    expect(textarea.value).toBe('')
    expect(textarea.style.height).toBe('38px')
    expect(textarea.style.overflowY).toBe('hidden')
  })

  it('resizes the message input from the top grip', () => {
    const { textarea, getByRole } = renderComposer()
    vi.spyOn(textarea, 'getBoundingClientRect').mockReturnValue({
      width: 320,
      height: 48,
      top: 0,
      left: 0,
      right: 320,
      bottom: 48,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    })

    fireEvent.keyDown(getByRole('button', { name: 'Resize message input' }), {
      key: 'ArrowUp',
    })

    expect(textarea.style.height).toBe('72px')
  })

  it('resizes the message input from the composer with mod+shift chords', () => {
    const onHeightChange = vi.fn()
    const { textarea } = renderComposer({ height: 100, onHeightChange })
    vi.spyOn(textarea, 'getBoundingClientRect').mockReturnValue({
      width: 320,
      height: 100,
      top: 0,
      left: 0,
      right: 320,
      bottom: 100,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    })

    // The chord resolves on `event.key` (the character the layout emits),
    // never `event.code` (the physical QWERTY position) — so `code` is set to a
    // *non*-QWERTY-matching value here to prove a Dvorak user pressing their
    // own `.`/`,`/`0` keys (which land on different physical keys) still
    // resizes. Grow, shrink, then reset to the auto-fit height.
    fireEvent.keyDown(textarea, {
      key: '>',
      code: 'KeyE',
      ctrlKey: true,
      shiftKey: true,
    })
    expect(onHeightChange).toHaveBeenLastCalledWith(124)

    fireEvent.keyDown(textarea, {
      key: '<',
      code: 'KeyW',
      ctrlKey: true,
      shiftKey: true,
    })
    expect(onHeightChange).toHaveBeenLastCalledWith(76)

    fireEvent.keyDown(textarea, {
      key: ')',
      code: 'Digit0',
      ctrlKey: true,
      shiftKey: true,
    })
    expect(onHeightChange).toHaveBeenLastCalledWith(null)
  })

  it('leaves a shifted period in the draft without a resize modifier', () => {
    const onHeightChange = vi.fn()
    const { textarea } = renderComposer({ height: 100, onHeightChange })

    fireEvent.keyDown(textarea, { key: '>', code: 'KeyE', shiftKey: true })

    expect(onHeightChange).not.toHaveBeenCalled()
  })

  it('leaves Ctrl-A to the platform select-all default', () => {
    const { textarea } = renderComposer({
      initialValue: 'first line\nsecond line',
    })
    textarea.setSelectionRange(
      'first line\nsecond'.length,
      'first line\nsecond'.length,
    )
    const event = new KeyboardEvent('keydown', {
      key: 'a',
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    })

    textarea.dispatchEvent(event)

    expect(event.defaultPrevented).toBe(false)
    expect(textarea.selectionStart).toBe('first line\nsecond'.length)
    expect(textarea.selectionEnd).toBe('first line\nsecond'.length)
  })

  it('moves Ctrl-Home and Ctrl-End across the whole message on Windows and Linux', async () => {
    const { textarea } = renderComposer({
      initialValue: 'first line\nsecond line',
    })
    Object.defineProperty(textarea, 'scrollHeight', {
      configurable: true,
      value: 240,
    })
    textarea.setSelectionRange(
      'first line\nsecond'.length,
      'first line\nsecond'.length,
    )
    textarea.scrollTop = 120

    fireEvent.keyDown(textarea, { key: 'Home', ctrlKey: true })

    expect(textarea.selectionStart).toBe(0)
    expect(textarea.selectionEnd).toBe(0)
    expect(textarea.scrollTop).toBe(0)
    await act(async () => {
      await new Promise((resolve) => requestAnimationFrame(resolve))
    })
    expect(textarea.scrollTop).toBe(0)

    fireEvent.keyDown(textarea, { key: 'End', ctrlKey: true })

    expect(textarea.selectionStart).toBe(textarea.value.length)
    expect(textarea.selectionEnd).toBe(textarea.value.length)
    expect(textarea.scrollTop).toBe(240)
  })

  it('moves Cmd-Up and Cmd-Down across the whole message on macOS', async () => {
    vi.stubGlobal('navigator', {
      ...navigator,
      userAgent: 'MacIntel',
      maxTouchPoints: 0,
    })
    const { textarea } = renderComposer({
      initialValue: 'first line\nsecond line',
    })
    Object.defineProperty(textarea, 'scrollHeight', {
      configurable: true,
      value: 240,
    })
    textarea.setSelectionRange(
      'first line\nsecond'.length,
      'first line\nsecond'.length,
    )
    textarea.scrollTop = 120

    fireEvent.keyDown(textarea, { key: 'ArrowUp', metaKey: true })

    expect(textarea.selectionStart).toBe(0)
    expect(textarea.selectionEnd).toBe(0)
    expect(textarea.scrollTop).toBe(0)
    await act(async () => {
      await new Promise((resolve) => requestAnimationFrame(resolve))
    })
    expect(textarea.scrollTop).toBe(0)

    fireEvent.keyDown(textarea, { key: 'ArrowDown', metaKey: true })

    expect(textarea.selectionStart).toBe(textarea.value.length)
    expect(textarea.selectionEnd).toBe(textarea.value.length)
    expect(textarea.scrollTop).toBe(240)
  })
})

describe('Composer Enter-key behavior (mobile newline vs. desktop send)', () => {
  it('submits on Enter without Shift when the viewport is wide (desktop)', () => {
    const { textarea, onSubmit } = renderComposer()
    fireEvent.input(textarea, { target: { value: 'hello there' } })

    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(onSubmit).toHaveBeenCalledWith('hello there')
  })

  it('still inserts a newline on Shift+Enter when the viewport is wide', () => {
    const { textarea, onSubmit } = renderComposer()
    fireEvent.input(textarea, { target: { value: 'hello there' } })

    const event = new KeyboardEvent('keydown', {
      key: 'Enter',
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    })
    textarea.dispatchEvent(event)

    expect(onSubmit).not.toHaveBeenCalled()
    expect(event.defaultPrevented).toBe(false)
  })

  it('inserts a newline on Enter instead of submitting when the viewport is narrow', () => {
    Object.defineProperty(window, 'matchMedia', {
      configurable: true,
      value: mobileMatchMedia(true),
    })
    const { textarea, onSubmit } = renderComposer()
    fireEvent.input(textarea, { target: { value: 'hello there' } })

    const event = new KeyboardEvent('keydown', {
      key: 'Enter',
      bubbles: true,
      cancelable: true,
    })
    textarea.dispatchEvent(event)

    expect(onSubmit).not.toHaveBeenCalled()
    expect(event.defaultPrevented).toBe(false)
  })

  it('does not submit on an IME-composing Enter', () => {
    const { textarea, onSubmit } = renderComposer()
    fireEvent.input(textarea, { target: { value: 'hello there' } })

    const event = new KeyboardEvent('keydown', {
      key: 'Enter',
      isComposing: true,
      bubbles: true,
      cancelable: true,
    })
    textarea.dispatchEvent(event)

    expect(onSubmit).not.toHaveBeenCalled()
  })

  it("sets enterKeyHint to 'send' on wide viewport and 'enter' on narrow viewport", () => {
    const { textarea: wideTextarea } = renderComposer()
    expect(wideTextarea.getAttribute('enterkeyhint')).toBe('send')
    cleanup()

    Object.defineProperty(window, 'matchMedia', {
      configurable: true,
      value: mobileMatchMedia(true),
    })
    const { textarea: narrowTextarea } = renderComposer()
    expect(narrowTextarea.getAttribute('enterkeyhint')).toBe('enter')
  })
})

describe('Composer slash command autocomplete', () => {
  it('filters slash commands by the typed prefix', () => {
    const { textarea, getByRole, queryByRole } = renderComposer({
      onCommand: vi.fn(),
    })

    fireEvent.input(textarea, { target: { value: '/re' } })

    const menu = getByRole('listbox', { name: 'Slash commands' })
    expect(menu.textContent).toContain('/react')
    expect(menu.textContent).toContain('/reply')
    expect(menu.textContent).not.toContain('/thread')
    expect(queryByRole('option', { name: /\/help/ })).toBeNull()
    expect(textarea.getAttribute('aria-expanded')).toBe('true')
  })

  it('shows the jump date format in slash command completions', () => {
    const { textarea, getByRole } = renderComposer({ onCommand: vi.fn() })

    fireEvent.input(textarea, { target: { value: '/ju' } })

    const menu = getByRole('listbox', { name: 'Slash commands' })
    expect(menu.textContent).toContain('/jump [YYYY-MM-DD]')
  })

  it('completes the selected partial command with Enter', () => {
    const { textarea, queryByRole } = renderComposer({ onCommand: vi.fn() })

    fireEvent.input(textarea, { target: { value: '/re' } })
    fireEvent.keyDown(textarea, { key: 'ArrowDown' })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(textarea.value).toBe('/reply ')
    expect(queryByRole('listbox', { name: 'Slash commands' })).toBeNull()
  })

  it('keeps modified arrows inside autocomplete instead of jumping the message caret', () => {
    vi.stubGlobal('navigator', {
      ...navigator,
      userAgent: 'MacIntel',
      maxTouchPoints: 0,
    })
    const { textarea } = renderComposer({ onCommand: vi.fn() })

    fireEvent.input(textarea, { target: { value: '/re' } })
    textarea.setSelectionRange(textarea.value.length, textarea.value.length)
    fireEvent.keyDown(textarea, { key: 'ArrowDown', metaKey: true })

    expect(textarea.getAttribute('aria-activedescendant')).toBe(
      'composer-slash-command-reply',
    )
    expect(textarea.selectionStart).toBe(textarea.value.length)
  })

  it('completes the selected partial command with Enter on a narrow viewport too', () => {
    // The mobile "Enter inserts a newline" branch must not shadow autocomplete
    // completion, which claims Enter earlier in the keydown chain.
    Object.defineProperty(window, 'matchMedia', {
      configurable: true,
      value: mobileMatchMedia(true),
    })
    const { textarea, queryByRole } = renderComposer({ onCommand: vi.fn() })

    fireEvent.input(textarea, { target: { value: '/re' } })
    fireEvent.keyDown(textarea, { key: 'ArrowDown' })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(textarea.value).toBe('/reply ')
    expect(queryByRole('listbox', { name: 'Slash commands' })).toBeNull()
  })

  it('completes the selected command with Tab', () => {
    const { textarea, queryByRole } = renderComposer({ onCommand: vi.fn() })

    fireEvent.input(textarea, { target: { value: '/th' } })
    fireEvent.keyDown(textarea, { key: 'Tab' })

    expect(textarea.value).toBe('/thread ')
    expect(queryByRole('listbox', { name: 'Slash commands' })).toBeNull()
  })

  it('submits an exact slash command instead of completing it again', () => {
    const onCommand = vi.fn(() => true)
    const { textarea, onSubmit } = renderComposer({ onCommand })

    fireEvent.input(textarea, { target: { value: '/react' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(onCommand).toHaveBeenCalledWith('/react')
    expect(onSubmit).not.toHaveBeenCalled()
    expect(textarea.value).toBe('')
  })

  it('dismisses the /room menu with Escape rather than swapping it for room matches', () => {
    const { textarea, queryByRole } = renderComposer({
      onCommand: vi.fn(),
      roomCompletions: () => [
        {
          value: '#axontest:bostoncoop.net',
          label: '#axontest:bostoncoop.net',
          description: '!axontest:bostoncoop.net',
        },
      ],
    })

    // `/room` is both a complete command name and an empty room query, so a
    // dismissal has to name the menu it dismissed, not just the text.
    fireEvent.input(textarea, { target: { value: '/room' } })
    expect(queryByRole('listbox', { name: 'Slash commands' })).toBeTruthy()

    fireEvent.keyDown(textarea, { key: 'Escape' })

    expect(queryByRole('listbox', { name: 'Slash commands' })).toBeNull()
    expect(queryByRole('listbox', { name: 'Room matches' })).toBeNull()
  })

  it('dismisses autocomplete with Escape without cancelling the banner', () => {
    const onCancel = vi.fn()
    const { textarea, queryByRole, queryByText } = renderComposer({
      onCommand: vi.fn(),
      banner: {
        label: 'Replying to',
        excerpt: '@alice: hi',
        onCancel,
      },
    })

    fireEvent.input(textarea, { target: { value: '/re' } })
    fireEvent.keyDown(textarea, { key: 'Escape' })

    expect(onCancel).not.toHaveBeenCalled()
    expect(queryByRole('listbox', { name: 'Slash commands' })).toBeNull()
    expect(queryByText('Replying to')).toBeTruthy()
  })

  it('does not offer command names after the command argument starts', () => {
    const { textarea, queryByRole } = renderComposer({ onCommand: vi.fn() })

    fireEvent.input(textarea, { target: { value: '/reply hello' } })

    expect(queryByRole('listbox', { name: 'Slash commands' })).toBeNull()
  })

  it('offers room completions after /room', () => {
    const { textarea, getByRole, queryByRole } = renderComposer({
      onCommand: vi.fn(),
      roomCompletions: (query) =>
        query === 'axont'
          ? [
              {
                value: '#axontest:bostoncoop.net',
                label: '#axontest:bostoncoop.net',
                description: '!axontest:bostoncoop.net',
              },
            ]
          : [],
    })

    fireEvent.input(textarea, { target: { value: '/room axont' } })

    const menu = getByRole('listbox', { name: 'Room matches' })
    expect(menu.textContent).toContain('#axontest:bostoncoop.net')
    expect(queryByRole('listbox', { name: 'Slash commands' })).toBeNull()
  })

  it('completes the selected room match with Enter', () => {
    const { textarea, queryByRole } = renderComposer({
      onCommand: vi.fn(),
      roomCompletions: () => [
        {
          value: '#axontest:bostoncoop.net',
          label: '#axontest:bostoncoop.net',
          description: '!axontest:bostoncoop.net',
        },
      ],
    })

    fireEvent.input(textarea, { target: { value: '/room axont' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(textarea.value).toBe('/room #axontest:bostoncoop.net ')
    expect(queryByRole('listbox', { name: 'Room matches' })).toBeNull()
  })

  it('offers and completes member mentions inside message text', () => {
    const { textarea, getByRole, queryByRole } = renderComposer({
      mentionCompletions: (query) =>
        query === 'al'
          ? [
              {
                value: '@alice',
                label: '@alice',
                description: 'Alice Cooper · @alice:hs',
              },
            ]
          : [],
    })

    fireEvent.input(textarea, { target: { value: 'hi @al' } })

    const menu = getByRole('listbox', { name: 'User matches' })
    expect(menu.textContent).toContain('@alice')
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(textarea.value).toBe('hi @alice ')
    expect(queryByRole('listbox', { name: 'User matches' })).toBeNull()
  })

  it('offers and completes room references inside message text', () => {
    const { textarea, getByRole, queryByRole } = renderComposer({
      roomReferenceCompletions: (query) =>
        query === 'Op'
          ? [
              {
                value: '#Ops',
                label: '#Ops',
                description: '!ops:hs',
              },
            ]
          : [],
    })

    fireEvent.input(textarea, { target: { value: 'see #Op later' } })
    textarea.setSelectionRange('see #Op'.length, 'see #Op'.length)
    fireEvent.keyUp(textarea, { key: 'ArrowLeft' })

    const menu = getByRole('listbox', { name: 'Room matches' })
    expect(menu.textContent).toContain('#Ops')
    fireEvent.keyDown(textarea, { key: 'Tab' })

    expect(textarea.value).toBe('see #Ops later')
    expect(queryByRole('listbox', { name: 'Room matches' })).toBeNull()
  })

  it('offers and completes emoji shortcodes inside message text', () => {
    const { textarea, getByRole, queryByRole } = renderComposer({
      emojiCompletions: (query) =>
        query === '+'
          ? [
              {
                value: ':+1:',
                label: '👍 :+1:',
                description: 'thumbs up',
              },
            ]
          : [],
    })

    fireEvent.input(textarea, { target: { value: 'send :+' } })

    const menu = getByRole('listbox', { name: 'Emoji matches' })
    expect(menu.textContent).toContain(':+1:')
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(textarea.value).toBe('send :+1: ')
    expect(queryByRole('listbox', { name: 'Emoji matches' })).toBeNull()
  })

  it('offers and completes emoji shortcodes as /react arguments', () => {
    const { textarea, getByRole, queryByRole } = renderComposer({
      onCommand: vi.fn(),
      emojiCompletions: (query) =>
        query === 'pa'
          ? [
              {
                value: ':partying:',
                label: '🥳 :partying:',
                description: 'partying face',
              },
            ]
          : [],
    })

    fireEvent.input(textarea, { target: { value: '/react pa' } })

    const menu = getByRole('listbox', { name: 'Emoji matches' })
    expect(menu.textContent).toContain(':partying:')
    fireEvent.keyDown(textarea, { key: 'Tab' })

    expect(textarea.value).toBe('/react :partying: ')
    expect(queryByRole('listbox', { name: 'Emoji matches' })).toBeNull()
  })

  it('offers emoji shortcode completion for /+ reaction aliases', () => {
    const { textarea, getByRole, queryByRole } = renderComposer({
      onCommand: vi.fn(),
      emojiCompletions: (query) =>
        query === 'pa'
          ? [
              {
                value: ':partying:',
                label: '🥳 :partying:',
                description: 'partying face',
              },
            ]
          : [],
    })

    fireEvent.input(textarea, { target: { value: '/+ pa' } })

    const menu = getByRole('listbox', { name: 'Emoji matches' })
    expect(menu.textContent).toContain(':partying:')
    fireEvent.keyDown(textarea, { key: 'Tab' })

    expect(textarea.value).toBe('/+ :partying: ')
    expect(queryByRole('listbox', { name: 'Emoji matches' })).toBeNull()
  })

  it('submits an exact /react emoji shortcode without requiring completion first', () => {
    const onCommand = vi.fn(() => true)
    const { textarea } = renderComposer({
      onCommand,
      emojiCompletions: (query) =>
        query === 'partying'
          ? [
              {
                value: ':partying:',
                label: '🥳 :partying:',
                description: 'partying face',
              },
            ]
          : [],
    })

    fireEvent.input(textarea, { target: { value: '/react partying' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(onCommand).toHaveBeenCalledWith('/react partying')
    expect(textarea.value).toBe('')
  })

  it('uses arrow keys to move through emoji matches and keep the active option visible', async () => {
    const originalScrollIntoView = HTMLElement.prototype.scrollIntoView
    const scrollIntoView = vi.fn()
    Object.defineProperty(HTMLElement.prototype, 'scrollIntoView', {
      configurable: true,
      value: scrollIntoView,
    })

    try {
      const options = Array.from({ length: 12 }, (_, index) => ({
        value: `:smile${index}:`,
        label: `😄 :smile${index}:`,
        description: `smile ${index}`,
      }))
      const { textarea } = renderComposer({
        emojiCompletions: (query) => (query === 's' ? options : []),
      })

      fireEvent.input(textarea, { target: { value: 'send :s' } })
      scrollIntoView.mockClear()

      fireEvent.keyDown(textarea, { key: 'ArrowDown' })

      await waitFor(() => {
        expect(textarea.getAttribute('aria-activedescendant')).toContain(
          encodeURIComponent(':smile1:'),
        )
        expect(scrollIntoView).toHaveBeenCalled()
      })

      fireEvent.keyDown(textarea, { key: 'ArrowUp' })
      fireEvent.keyDown(textarea, { key: 'ArrowUp' })
      fireEvent.keyDown(textarea, { key: 'Enter' })

      expect(textarea.value).toBe('send :smile11: ')
    } finally {
      Object.defineProperty(HTMLElement.prototype, 'scrollIntoView', {
        configurable: true,
        value: originalScrollIntoView,
      })
    }
  })

  it('does not offer emoji completions for escaped shortcodes', () => {
    const { textarea, queryByRole } = renderComposer({
      emojiCompletions: () => [
        {
          value: ':+1:',
          label: '👍 :+1:',
          description: 'thumbs up',
        },
      ],
    })

    fireEvent.input(textarea, { target: { value: '\\:+' } })

    expect(queryByRole('listbox', { name: 'Emoji matches' })).toBeNull()
  })
})

describe('Composer attachments (M-W8.5, ADR 0065; multi-image ADR 0081)', () => {
  const png = (name = 'cat.png') =>
    new File(['bytes'], name, { type: 'image/png' })

  const staged = (files: File[], extra: object = {}) => ({
    items: files.map((file, index) => ({
      id: `id-${index}`,
      file,
      previewUrl: null,
    })),
    skipped: 0,
    skippedReason: null,
    // The real hook derives this; the strip reads it rather than re-adding
    // the sizes itself.
    totalBytes: files.reduce((sum, file) => sum + file.size, 0),
    onRemove: vi.fn(),
    ...extra,
  })

  it('reports files chosen from the picker', () => {
    const onAttach = vi.fn()
    const { getByLabelText } = renderComposer({ onAttach })
    const input = getByLabelText('Attach a file') as HTMLInputElement
    const file = png()

    fireEvent.change(input, { target: { files: [file] } })

    // The whole list goes through; only the staging hook knows the caps.
    expect([...onAttach.mock.calls[0][0]]).toEqual([file])
  })

  describe('attach menu (packaged Android app)', () => {
    const HOLD = 600 // past MESSAGE_TOUCH_HOLD_MS

    function inAndroidShell(): void {
      vi.stubGlobal('navigator', {
        userAgent:
          'Mozilla/5.0 (Linux; Android 13; SM-G781U1; wv) AppleWebKit/537.36 Chrome/155.0.0.0 Mobile Safari/537.36',
      })
      ;(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {}
    }
    afterEach(() => {
      vi.useRealTimers()
      delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__
    })

    function paperclip(container: Element): HTMLElement {
      return container.querySelector<HTMLElement>('button.composer-attach')!
    }
    function holdPaperclip(container: Element): void {
      fireEvent.touchStart(paperclip(container))
      act(() => {
        vi.advanceTimersByTime(HOLD)
      })
    }

    it('adds nothing outside the shell, so iOS and the browser keep their own sheet', () => {
      vi.useFakeTimers()
      const { queryByLabelText, queryByRole, container } = renderComposer({
        onAttach: vi.fn(),
      })
      expect(queryByLabelText('Take a photo')).toBeNull()
      expect(queryByLabelText('Record a video')).toBeNull()
      holdPaperclip(container)
      expect(queryByRole('menu')).toBeNull()
    })

    it('has a photo and a video input that go straight to the camera', () => {
      inAndroidShell()
      const { getByLabelText } = renderComposer({ onAttach: vi.fn() })
      const photo = getByLabelText('Take a photo') as HTMLInputElement
      const video = getByLabelText('Record a video') as HTMLInputElement
      // One input each: the Android chooser records video whenever `accept`
      // allows it, so a single input could never take a photo.
      expect(photo.accept).toBe('image/*')
      expect(video.accept).toBe('video/*')
      expect(photo.getAttribute('capture')).toBe('environment')
      expect(video.getAttribute('capture')).toBe('environment')
      // The ordinary attach input is untouched: no `capture`, so it still
      // opens the document picker.
      const attach = getByLabelText('Attach a file') as HTMLInputElement
      expect(attach.hasAttribute('capture')).toBe(false)
    })

    it('reports a captured photo and a captured video', () => {
      inAndroidShell()
      const onAttach = vi.fn()
      const { getByLabelText } = renderComposer({ onAttach })
      const photo = png('shot.png')
      const clip = new File(['v'], 'clip.mp4', { type: 'video/mp4' })

      fireEvent.change(getByLabelText('Take a photo'), {
        target: { files: [photo] },
      })
      fireEvent.change(getByLabelText('Record a video'), {
        target: { files: [clip] },
      })

      expect([...onAttach.mock.calls[0][0]]).toEqual([photo])
      expect([...onAttach.mock.calls[1][0]]).toEqual([clip])
    })

    it('a plain tap still opens the file picker, with no menu', () => {
      vi.useFakeTimers()
      inAndroidShell()
      const { getByLabelText, queryByRole, container } = renderComposer({
        onAttach: vi.fn(),
      })
      const picker = vi.spyOn(
        getByLabelText('Attach a file') as HTMLInputElement,
        'click',
      )

      fireEvent.touchStart(paperclip(container))
      fireEvent.touchEnd(paperclip(container))
      fireEvent.click(paperclip(container))

      expect(picker).toHaveBeenCalledTimes(1)
      expect(queryByRole('menu')).toBeNull()
    })

    it('a long press opens the menu, and the release does not also open the picker', () => {
      vi.useFakeTimers()
      inAndroidShell()
      const { getByLabelText, getAllByRole, queryByRole, container } =
        renderComposer({ onAttach: vi.fn() })
      const picker = vi.spyOn(
        getByLabelText('Attach a file') as HTMLInputElement,
        'click',
      )

      holdPaperclip(container)
      expect(queryByRole('menu')).not.toBeNull()
      expect(getAllByRole('menuitem').map((i) => i.textContent)).toEqual([
        'Attach a file',
        'Take a photo',
        'Record a video',
      ])

      // Lifting the finger ends the gesture with a click on the same button.
      fireEvent.touchEnd(paperclip(container))
      fireEvent.click(paperclip(container))
      expect(picker).not.toHaveBeenCalled()
    })

    it('moving the finger before the hold elapses cancels it', () => {
      vi.useFakeTimers()
      inAndroidShell()
      const { queryByRole, container } = renderComposer({
        onAttach: vi.fn(),
      })

      fireEvent.touchStart(paperclip(container))
      fireEvent.touchMove(paperclip(container))
      act(() => {
        vi.advanceTimersByTime(HOLD)
      })

      expect(queryByRole('menu')).toBeNull()
    })

    it('each item opens its own input and closes the menu', () => {
      vi.useFakeTimers()
      inAndroidShell()
      const { getByLabelText, getByRole, queryByRole, container } =
        renderComposer({ onAttach: vi.fn() })
      const clicks = {
        file: vi.spyOn(
          getByLabelText('Attach a file') as HTMLInputElement,
          'click',
        ),
        photo: vi.spyOn(
          getByLabelText('Take a photo') as HTMLInputElement,
          'click',
        ),
        video: vi.spyOn(
          getByLabelText('Record a video') as HTMLInputElement,
          'click',
        ),
      }

      holdPaperclip(container)
      fireEvent.click(getByRole('menuitem', { name: 'Take a photo' }))
      expect(clicks.photo).toHaveBeenCalledTimes(1)
      expect(clicks.video).not.toHaveBeenCalled()
      expect(clicks.file).not.toHaveBeenCalled()
      expect(queryByRole('menu')).toBeNull()

      holdPaperclip(container)
      fireEvent.click(getByRole('menuitem', { name: 'Record a video' }))
      expect(clicks.video).toHaveBeenCalledTimes(1)

      holdPaperclip(container)
      fireEvent.click(getByRole('menuitem', { name: 'Attach a file' }))
      expect(clicks.file).toHaveBeenCalledTimes(1)
    })

    it('closes on Escape and on a touch outside, but not on a touch inside', () => {
      vi.useFakeTimers()
      inAndroidShell()
      const { getByRole, queryByRole, container } = renderComposer({
        onAttach: vi.fn(),
      })

      holdPaperclip(container)
      fireEvent.pointerDown(getByRole('menuitem', { name: 'Take a photo' }))
      expect(queryByRole('menu')).not.toBeNull()

      fireEvent.pointerDown(document.body)
      expect(queryByRole('menu')).toBeNull()

      holdPaperclip(container)
      fireEvent.keyDown(document, { key: 'Escape' })
      expect(queryByRole('menu')).toBeNull()
    })
  })

  it('accepts a multi-select from the picker', () => {
    const onAttach = vi.fn()
    const { getByLabelText } = renderComposer({ onAttach })
    const input = getByLabelText('Attach a file') as HTMLInputElement
    expect(input.multiple).toBe(true)

    const files = [png('a.png'), png('b.png'), png('c.png')]
    fireEvent.change(input, { target: { files } })

    expect([...onAttach.mock.calls[0][0]]).toHaveLength(3)
  })

  it('reports pasted files, but leaves an ordinary text paste alone', () => {
    const onAttach = vi.fn()
    const { textarea } = renderComposer({ onAttach })
    const file = png()

    fireEvent.paste(textarea, { clipboardData: { files: [file] } })
    expect([...onAttach.mock.calls[0][0]]).toEqual([file])

    // A paste carrying no file must fall through to the browser's own handling,
    // or pasting text into the composer stops working.
    onAttach.mockClear()
    const textPaste = new Event('paste', { bubbles: true, cancelable: true })
    Object.defineProperty(textPaste, 'clipboardData', { value: { files: [] } })
    fireEvent(textarea, textPaste)

    expect(onAttach).not.toHaveBeenCalled()
    expect(textPaste.defaultPrevented).toBe(false)
  })

  it('submits an attachment with an empty draft — a bare file is a message', () => {
    const { form, onSubmit } = renderComposer({
      onAttach: vi.fn(),
      attachments: staged([png()]),
    })

    fireEvent.submit(form)

    expect(onSubmit).toHaveBeenCalledWith('')
  })

  it('sends the typed text as the caption', () => {
    const { form, textarea, onSubmit } = renderComposer({
      onAttach: vi.fn(),
      attachments: staged([png()]),
    })

    fireEvent.input(textarea, { target: { value: 'look at this' } })
    fireEvent.submit(form)

    expect(onSubmit).toHaveBeenCalledWith('look at this')
  })

  it('stays inert on an empty draft with nothing attached', () => {
    const { form, onSubmit } = renderComposer({ onAttach: vi.fn() })
    fireEvent.submit(form)
    expect(onSubmit).not.toHaveBeenCalled()
  })

  it('shows a single staged file as the original chip, and removes it', () => {
    // The one-file path is deliberately unchanged, so nothing about the
    // existing behaviour shifts for people who never attach two.
    const onRemove = vi.fn()
    const { getByText, getByLabelText, container } = renderComposer({
      onAttach: vi.fn(),
      attachments: staged([png()], { onRemove }),
    })

    expect(container.querySelector('.composer-attachment')).not.toBeNull()
    expect(container.querySelector('.composer-attachment-strip')).toBeNull()
    expect(getByText('cat.png')).toBeTruthy()

    fireEvent.click(getByLabelText('Remove cat.png'))
    expect(onRemove).toHaveBeenCalledWith('id-0')
  })

  it('shows several as a strip, and removes the one asked for', () => {
    const onRemove = vi.fn()
    const { getByLabelText, container } = renderComposer({
      onAttach: vi.fn(),
      attachments: staged([png('a.png'), png('b.png'), png('c.png')], {
        onRemove,
      }),
    })

    expect(container.querySelector('.composer-attachment-strip')).not.toBeNull()
    expect(
      container.querySelectorAll('.composer-attachment-item'),
    ).toHaveLength(3)

    fireEvent.click(getByLabelText('Remove file 2 of 3, b.png'))
    expect(onRemove).toHaveBeenCalledWith('id-1')
  })

  it('distinguishes two staged files that share a name', () => {
    // Staging is additive and keyed by id, so pasting the same screenshot
    // twice is two items with one name. Named alone, a screen-reader user
    // could not tell which remove button was which.
    const onRemove = vi.fn()
    const { getByLabelText } = renderComposer({
      onAttach: vi.fn(),
      attachments: staged([png('shot.png'), png('shot.png')], { onRemove }),
    })

    fireEvent.click(getByLabelText('Remove file 2 of 2, shot.png'))
    expect(onRemove).toHaveBeenCalledWith('id-1')
  })

  it('says the first image carries the caption', () => {
    const { getByText } = renderComposer({
      onAttach: vi.fn(),
      attachments: staged([png('a.png'), png('b.png')]),
    })
    expect(getByText(/captions the first attachment/)).toBeTruthy()
  })

  it('admits the files a cap refused, and why', () => {
    const { getByText, rerender } = renderComposer({
      onAttach: vi.fn(),
      attachments: staged([png()], { skipped: 2, skippedReason: 'count' }),
    })
    expect(getByText(/2 files not added/)).toBeTruthy()
    expect(getByText(/too many at once/)).toBeTruthy()
    void rerender
  })

  it('names the size cap when that is what refused them', () => {
    const { getByText } = renderComposer({
      onAttach: vi.fn(),
      attachments: staged([png()], { skipped: 1, skippedReason: 'size' }),
    })
    expect(getByText(/batch would be too large/)).toBeTruthy()
  })

  it('offers no attach affordance without an onAttach handler (edit mode)', () => {
    const { queryByLabelText } = renderComposer()
    expect(queryByLabelText('Attach a file')).toBeNull()
  })
})

describe('Composer link paste', () => {
  it('wraps selected text in a Markdown link when pasting a URL', async () => {
    const { textarea, onDraftChange } = renderComposer()
    fireEvent.input(textarea, { target: { value: 'This is a link.' } })
    textarea.setSelectionRange('This is a '.length, 'This is a link'.length)

    fireEvent.paste(textarea, {
      clipboardData: {
        files: [],
        getData: () => 'https://example.com',
      },
    })

    await waitFor(() =>
      expect(textarea.value).toBe('This is a [link](https://example.com).'),
    )
    expect(onDraftChange).toHaveBeenLastCalledWith(
      'This is a [link](https://example.com).',
    )
    await waitFor(() =>
      expect(textarea.selectionStart).toBe(
        'This is a [link](https://example.com)'.length,
      ),
    )
  })

  it('escapes Markdown metacharacters in pasted link text', async () => {
    const { textarea } = renderComposer()
    fireEvent.input(textarea, { target: { value: 'Read [docs]' } })
    textarea.setSelectionRange('Read '.length, 'Read [docs]'.length)

    fireEvent.paste(textarea, {
      clipboardData: {
        files: [],
        getData: () => 'https://example.com/docs)',
      },
    })

    await waitFor(() =>
      expect(textarea.value).toBe(
        'Read [\\[docs\\]](https://example.com/docs\\))',
      ),
    )
  })

  it('leaves selected non-URL text paste to the browser', () => {
    const { textarea } = renderComposer()
    fireEvent.input(textarea, { target: { value: 'This is a link.' } })
    textarea.setSelectionRange('This is a '.length, 'This is a link'.length)
    const textPaste = new Event('paste', { bubbles: true, cancelable: true })
    Object.defineProperty(textPaste, 'clipboardData', {
      value: {
        files: [],
        getData: () => 'not a url',
      },
    })

    fireEvent(textarea, textPaste)

    expect(textPaste.defaultPrevented).toBe(false)
    expect(textarea.value).toBe('This is a link.')
  })
})
