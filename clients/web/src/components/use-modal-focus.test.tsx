import { cleanup, fireEvent, render } from '@testing-library/preact'
import { afterEach, describe, expect, it } from 'vitest'
import { useRef } from 'preact/hooks'
import { useModalFocus } from './use-modal-focus'

afterEach(cleanup)

/**
 * A minimal overlay using the shared contract. Eight components depend on
 * `useModalFocus`, so these tests pin the behaviour they already rely on
 * before `restoreTo` widens it (ADR 0081).
 */
function Modal({
  restoreTo,
  initialFocusButton,
  buttons = ['first', 'second'],
}: {
  restoreTo?: () => HTMLElement | null
  initialFocusButton?: string
  buttons?: string[]
}) {
  const initialFocusRef = useRef<HTMLButtonElement>(null)
  const { containerRef } = useModalFocus<HTMLDivElement>({
    initialFocus: () => initialFocusRef.current,
    restoreTo,
  })
  return (
    <div ref={containerRef} role="dialog">
      {buttons.map((name) => (
        <button
          key={name}
          ref={name === initialFocusButton ? initialFocusRef : undefined}
          type="button"
        >
          {name}
        </button>
      ))}
    </div>
  )
}

function outsideButton(label: string): HTMLButtonElement {
  const button = document.createElement('button')
  button.textContent = label
  document.body.append(button)
  return button
}

describe('useModalFocus', () => {
  it('moves focus to the first focusable on mount', () => {
    const { getByText } = render(<Modal />)
    expect(document.activeElement).toBe(getByText('first'))
  })

  it('uses an explicit initial focus target without changing Tab order', () => {
    const { getByText } = render(
      <Modal
        initialFocusButton="second"
        buttons={['first', 'second', 'third']}
      />,
    )
    expect(document.activeElement).toBe(getByText('second'))

    fireEvent.keyDown(document, { key: 'Tab' })
    expect(document.activeElement).toBe(getByText('third'))
  })

  it('restores focus to the previously focused element on unmount', () => {
    const opener = outsideButton('opener')
    opener.focus()
    expect(document.activeElement).toBe(opener)

    const { unmount } = render(<Modal />)
    expect(document.activeElement).not.toBe(opener)

    unmount()
    expect(document.activeElement).toBe(opener)
    opener.remove()
  })

  it('wraps Tab forward at the last focusable', () => {
    const { getByText } = render(<Modal />)
    getByText('second').focus()
    fireEvent.keyDown(document, { key: 'Tab' })
    expect(document.activeElement).toBe(getByText('first'))
  })

  it('wraps Shift-Tab backward at the first focusable', () => {
    const { getByText } = render(<Modal />)
    getByText('first').focus()
    fireEvent.keyDown(document, { key: 'Tab', shiftKey: true })
    expect(document.activeElement).toBe(getByText('second'))
  })

  it('pulls focus back in when it escapes the container', () => {
    const stray = outsideButton('stray')
    const { getByText } = render(<Modal />)
    stray.focus()
    fireEvent.keyDown(document, { key: 'Tab' })
    expect(document.activeElement).toBe(getByText('first'))
    stray.remove()
  })

  it('skips a disabled button, so an inert control is not a dead tab stop', () => {
    // The paging lightbox disables its newest-end button; `collectFocusable`
    // filters `button:not([disabled])`, which is why the ADR requires the
    // attribute rather than `aria-disabled` alone.
    const { getByText, container } = render(<Modal />)
    getByText('first').setAttribute('disabled', '')
    const second = getByText('second')
    second.focus()
    fireEvent.keyDown(document, { key: 'Tab' })
    expect(document.activeElement).toBe(second)
    expect(container.querySelectorAll('button')).toHaveLength(2)
  })

  describe('restoreTo', () => {
    it('restores to the resolved element instead of the opener', () => {
      const opener = outsideButton('opener')
      const target = outsideButton('target')
      opener.focus()

      const { unmount } = render(<Modal restoreTo={() => target} />)
      unmount()

      expect(document.activeElement).toBe(target)
      opener.remove()
      target.remove()
    })

    it('resolves at unmount, not at mount', () => {
      // The paging lightbox cannot know its restore target when it opens: the
      // user may page to a different image, and back-pagination may replace
      // the row it came from.
      const opener = outsideButton('opener')
      opener.focus()
      let target: HTMLElement | null = null

      const { unmount } = render(<Modal restoreTo={() => target} />)
      const late = outsideButton('late')
      target = late

      unmount()
      expect(document.activeElement).toBe(late)
      opener.remove()
      late.remove()
    })

    it('falls back to the opener when the target is gone', () => {
      // `.focus()` on a detached node silently drops focus to <body>, which
      // would strand the user at the top of the page after back-pagination.
      const opener = outsideButton('opener')
      const doomed = outsideButton('doomed')
      opener.focus()

      const { unmount } = render(<Modal restoreTo={() => doomed} />)
      doomed.remove()
      unmount()

      expect(document.activeElement).toBe(opener)
      opener.remove()
    })

    it('falls back to the opener when the target cannot take focus', () => {
      // Found live: the viewer resolves to an image row, but a row whose
      // image is still lazily loading has no focusable control in it yet.
      // `.focus()` on such an element is a silent no-op that leaves focus on
      // `<body>`, which is worse than landing on the wrong image.
      const opener = outsideButton('opener')
      opener.focus()
      const inert = document.createElement('div')
      inert.textContent = 'not focusable'
      document.body.append(inert)

      const { unmount } = render(<Modal restoreTo={() => inert} />)
      unmount()

      expect(document.activeElement).toBe(opener)
      opener.remove()
      inert.remove()
    })

    it('falls back to the opener when the resolver returns null', () => {
      const opener = outsideButton('opener')
      opener.focus()

      const { unmount } = render(<Modal restoreTo={() => null} />)
      unmount()

      expect(document.activeElement).toBe(opener)
      opener.remove()
    })
  })
})
