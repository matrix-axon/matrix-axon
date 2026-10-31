import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from 'preact/hooks'
import { EMOJI_PICKER_DATA_SOURCE } from '../emoji'
import type { CustomEmoji } from '../emoji-packs'
import { useShortcuts } from '../shortcuts'
import { BodyPortal } from './BodyPortal'
import { ImagePackPicker } from './CustomEmojiPicker'
import { useModalFocus } from './use-modal-focus'
import { moveGridFocus } from './grid-keyboard'

type EmojiPickerClickDetail = {
  unicode?: string
  emoji?: { unicode?: string }
}

type PickerPosition = {
  left: number
  top: number
}

interface PickerRect {
  top: number
  right: number
  bottom: number
}

interface PickerSize {
  width: number
  height: number
}

export function fullReactionPickerPosition({
  anchor,
  dialog,
  viewport,
  margin = 8,
  gap = 8,
}: {
  anchor: PickerRect
  dialog: PickerSize
  viewport: PickerSize
  margin?: number
  gap?: number
}): PickerPosition {
  const maxLeft = Math.max(margin, viewport.width - dialog.width - margin)
  const maxTop = Math.max(margin, viewport.height - dialog.height - margin)
  const left = Math.min(Math.max(anchor.right, margin), maxLeft)
  const preferredAbove = anchor.top - dialog.height
  const preferredBelow = anchor.bottom + gap
  const top =
    preferredAbove >= margin
      ? preferredAbove
      : preferredBelow + dialog.height <= viewport.height - margin
        ? preferredBelow
        : Math.min(Math.max(preferredAbove, margin), maxTop)

  return { left, top: Math.min(Math.max(top, margin), maxTop) }
}

export interface EmojiPickerProps {
  anchor: DOMRectReadOnly | null
  accountId?: string
  roomId?: string
  action?: 'insert' | 'react'
  onSelect: (key: string, shortcode?: string, image?: CustomEmoji) => void
  onClose: () => void
  restoreTo?: () => HTMLElement | null
  /** Controls that stay interactive while this dialog is open. */
  relatedControls?: () => HTMLElement | null
}

/** Shared full picker: reaction and composer callers only choose what a selection does. */
export function EmojiPicker(props: EmojiPickerProps) {
  return (
    <BodyPortal>
      <EmojiPickerSurface {...props} />
    </BodyPortal>
  )
}

function EmojiPickerSurface({
  anchor,
  accountId,
  roomId,
  action = 'react',
  onSelect,
  onClose,
  restoreTo,
  relatedControls,
}: EmojiPickerProps) {
  const { containerRef } = useModalFocus<HTMLDivElement>({
    restoreTo,
    initialFocus: (): HTMLElement | null =>
      containerRef.current
        ?.querySelector('emoji-picker')
        ?.shadowRoot?.querySelector<HTMLInputElement>('input[type="search"]') ??
      containerRef.current?.querySelector<HTMLInputElement>(
        'input[type="search"]',
      ) ??
      null,
  })
  const [category, setCategory] = useState<'unicode' | 'custom'>('unicode')
  const [position, setPosition] = useState<PickerPosition | null>(null)
  const customAvailable = accountId !== undefined && roomId !== undefined
  const place = useCallback(() => {
    const dialog = containerRef.current
    if (!dialog) return
    const style = getComputedStyle(document.documentElement)
    const px = (name: string) =>
      Number.parseFloat(style.getPropertyValue(name)) || 0
    const left = px('--app-viewport-left') + px('--safe-left')
    const top = px('--app-viewport-top') + px('--safe-top')
    const width =
      (px('--app-viewport-width') || window.innerWidth) -
      px('--safe-left') -
      px('--safe-right')
    const height =
      (px('--app-viewport-height') || window.innerHeight) -
      px('--safe-top') -
      px('--safe-bottom')
    const measured = fullReactionPickerPosition({
      anchor: anchor
        ? {
            top: anchor.top - top,
            right: anchor.right - left,
            bottom: anchor.bottom - top,
          }
        : { top: height / 2, right: width / 2, bottom: height / 2 },
      dialog: dialog.getBoundingClientRect(),
      viewport: { width, height },
    })
    setPosition({ left: measured.left + left, top: measured.top + top })
  }, [anchor, containerRef])
  useLayoutEffect(place, [place])
  useEffect(() => {
    const observer =
      typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(place)
    if (containerRef.current) observer?.observe(containerRef.current)
    window.visualViewport?.addEventListener('resize', place)
    window.visualViewport?.addEventListener('scroll', place)
    window.addEventListener('resize', place)
    window.addEventListener('scroll', place, true)
    const outside = (event: PointerEvent) => {
      if (
        event.target instanceof Node &&
        !containerRef.current?.contains(event.target) &&
        !relatedControls?.()?.contains(event.target)
      )
        onClose()
    }
    document.addEventListener('pointerdown', outside)
    return () => {
      observer?.disconnect()
      window.visualViewport?.removeEventListener('resize', place)
      window.visualViewport?.removeEventListener('scroll', place)
      window.removeEventListener('resize', place)
      window.removeEventListener('scroll', place, true)
      document.removeEventListener('pointerdown', outside)
    }
  }, [place, containerRef, onClose, relatedControls])
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
    },
    { whileTyping: true, capture: true },
  )
  return (
    <div
      ref={containerRef}
      class="reaction-full-picker"
      role="dialog"
      aria-modal="true"
      aria-label="Emoji picker"
      style={
        position
          ? { left: `${position.left}px`, top: `${position.top}px` }
          : undefined
      }
    >
      <div class="reaction-full-picker-head">
        {customAvailable ? (
          <div
            class="reaction-picker-categories"
            role="group"
            aria-label="Emoji type"
          >
            <button
              type="button"
              aria-pressed={category === 'unicode'}
              onClick={() => setCategory('unicode')}
            >
              Emoji
            </button>
            <button
              type="button"
              aria-pressed={category === 'custom'}
              onClick={() => setCategory('custom')}
            >
              Custom
            </button>
          </div>
        ) : (
          <span>Emoji picker</span>
        )}
        <button type="button" class="ghost" onClick={onClose}>
          Close
        </button>
      </div>
      {category === 'custom' && customAvailable ? (
        <ImagePackPicker
          accountId={accountId}
          roomId={roomId}
          action={action}
          onSelect={(image) => onSelect(image.url, image.shortcode, image)}
        />
      ) : (
        <UnicodeEmojiPicker onSelect={onSelect} />
      )}
    </div>
  )
}

function UnicodeEmojiPicker({ onSelect }: { onSelect: (key: string) => void }) {
  const host = useRef<HTMLDivElement>(null)
  const select = useRef(onSelect)
  const [retry, setRetry] = useState(0)
  const [problem, setProblem] = useState(false)
  useEffect(() => {
    select.current = onSelect
  }, [onSelect])
  useEffect(() => {
    let cancelled = false
    let picker: HTMLElement | null = null
    let removeFocus = () => {}
    const click = (event: Event) => {
      const detail = (event as CustomEvent<EmojiPickerClickDetail>).detail
      const unicode = detail.unicode ?? detail.emoji?.unicode ?? ''
      if (unicode) select.current(unicode)
    }
    const keydown = (event: KeyboardEvent) => {
      if (picker) moveEmojiPickerFocus(picker, event)
    }
    setProblem(false)
    void import('emoji-picker-element')
      .then(({ Picker }) => {
        if (cancelled || !host.current) return
        picker = new Picker({ dataSource: EMOJI_PICKER_DATA_SOURCE })
        removeFocus = installEmojiPickerFocusVisible(picker)
        picker.addEventListener('emoji-click', click)
        picker.addEventListener('keydown', keydown)
        host.current.replaceChildren(picker)
        focusEmojiPickerSearch(picker)
      })
      .catch(() => {
        if (!cancelled) setProblem(true)
      })
    return () => {
      cancelled = true
      removeFocus()
      picker?.removeEventListener('emoji-click', click)
      picker?.removeEventListener('keydown', keydown)
      picker?.remove()
    }
  }, [retry])
  return (
    <div class="reaction-full-picker-host" ref={host}>
      {problem && (
        <p role="status">
          Could not load emoji.{' '}
          <button type="button" onClick={() => setRetry((value) => value + 1)}>
            Retry
          </button>
        </p>
      )}
    </div>
  )
}

function focusEmojiPickerSearch(picker: HTMLElement): void {
  const focusSearch = () => {
    if (!picker.isConnected) return false
    const root = picker.shadowRoot ?? picker
    const search = root.querySelector<HTMLInputElement>(
      'input[type="search"], input[aria-label*="search" i], input',
    )
    if (search !== null) {
      search.focus()
      return true
    }
    return false
  }

  if (focusSearch()) {
    return
  }
  requestAnimationFrame(() => {
    if (focusSearch()) {
      return
    }
    window.setTimeout(focusSearch, 0)
  })
}

function installEmojiPickerFocusVisible(picker: HTMLElement): () => void {
  const root = picker.shadowRoot
  if (root === null) {
    return () => {}
  }

  picker.setAttribute('data-js-focus-visible', '')
  const style = document.createElement('style')
  style.textContent = `
    [data-focus-visible-added] {
      outline: 2px solid var(--outline-color) !important;
      outline-offset: -2px !important;
    }
    button[data-focus-visible-added],
    .nav-button[data-focus-visible-added],
    .emoji[data-focus-visible-added] {
      background: var(--button-hover-background) !important;
      box-shadow: inset 0 0 0 2px var(--outline-color) !important;
    }
    input[data-focus-visible-added] {
      box-shadow: inset 0 0 0 2px var(--outline-color) !important;
    }
  `
  root.append(style)
  let keyboardMode = false

  const clear = () => {
    for (const element of root.querySelectorAll('[data-focus-visible-added]')) {
      element.removeAttribute('data-focus-visible-added')
    }
  }

  const mark = (element: HTMLElement | null) => {
    if (!keyboardMode || element === null) {
      return
    }
    clear()
    element.setAttribute('data-focus-visible-added', '')
  }

  const onKeyDown = () => {
    keyboardMode = true
    queueMicrotask(() => {
      mark(
        root.activeElement instanceof HTMLElement ? root.activeElement : null,
      )
    })
  }
  const onPointerDown = () => {
    keyboardMode = false
    clear()
  }
  const onFocusIn = (event: Event) => {
    mark(event.target instanceof HTMLElement ? event.target : null)
  }

  document.addEventListener('keydown', onKeyDown, true)
  root.addEventListener('pointerdown', onPointerDown, true)
  root.addEventListener('mousedown', onPointerDown, true)
  root.addEventListener('focusin', onFocusIn)

  return () => {
    document.removeEventListener('keydown', onKeyDown, true)
    root.removeEventListener('pointerdown', onPointerDown, true)
    root.removeEventListener('mousedown', onPointerDown, true)
    root.removeEventListener('focusin', onFocusIn)
    clear()
    style.remove()
    picker.removeAttribute('data-js-focus-visible')
  }
}

function moveEmojiPickerFocus(
  picker: HTMLElement,
  event: KeyboardEvent,
): boolean {
  const root = picker.shadowRoot
  if (root === null) {
    return false
  }
  const active = root.activeElement
  if (!(active instanceof HTMLElement)) {
    return false
  }

  const emojiButton = active.closest('button[role="menuitem"]')
  if (emojiButton instanceof HTMLButtonElement) {
    const menu = emojiButton.closest('.emoji-menu')
    if (!(menu instanceof HTMLElement)) {
      return false
    }
    const buttons = [
      ...menu.querySelectorAll<HTMLButtonElement>('button[role="menuitem"]'),
    ]
    return moveGridFocus(event, emojiButton, buttons, menu)
  }

  const navButton = active.closest('.nav-button')
  if (
    navButton instanceof HTMLButtonElement &&
    (event.key === 'ArrowLeft' || event.key === 'ArrowRight')
  ) {
    const buttons = [...root.querySelectorAll<HTMLButtonElement>('.nav-button')]
    const index = buttons.indexOf(navButton)
    const next =
      event.key === 'ArrowLeft' ? buttons[index - 1] : buttons[index + 1]
    if (next === undefined) {
      return false
    }
    event.preventDefault()
    next.focus()
    return true
  }

  return false
}
