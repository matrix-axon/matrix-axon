import type { RefObject } from 'preact'
import { useEffect, useRef } from 'preact/hooks'

/** What a modal considers reachable by Tab. */
const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), ' +
  'select:not([disabled]), textarea:not([disabled]), ' +
  '[tabindex]:not([tabindex="-1"])'

function isVisible(element: HTMLElement): boolean {
  return (
    !element.hasAttribute('hidden') &&
    element.getAttribute('aria-hidden') !== 'true'
  )
}

function collectFocusable(root: ParentNode): HTMLElement[] {
  const focusables: HTMLElement[] = []
  const visit = (node: ParentNode) => {
    if (
      node instanceof HTMLElement &&
      node.matches(FOCUSABLE) &&
      isVisible(node)
    ) {
      focusables.push(node)
    }
    for (const child of node.querySelectorAll<HTMLElement>('*')) {
      if (child.matches(FOCUSABLE) && isVisible(child)) {
        focusables.push(child)
      }
      if (child.shadowRoot !== null) {
        visit(child.shadowRoot)
      }
    }
  }
  visit(root)
  return focusables
}

function activeElementDeep(
  root: Document | ShadowRoot = document,
): HTMLElement | null {
  const active = root.activeElement
  if (active === null) {
    return null
  }
  if (active.shadowRoot?.activeElement instanceof HTMLElement) {
    return activeElementDeep(active.shadowRoot)
  }
  return active instanceof HTMLElement ? active : null
}

/** Whether focusing this element would actually do anything. */
function isAttached(element: HTMLElement | null): element is HTMLElement {
  return element !== null && element.isConnected
}

/**
 * The modal focus contract (ADR 0078 pattern; WCR-14), shared by every
 * overlay: on mount, remember the focused element and move focus to the
 * first focusable inside the container (or `initialFocus` when provided);
 * while mounted, trap Tab inside it
 * (wrapping at both ends, and pulling focus back in if it ever ends up
 * outside); on unmount, restore focus to where it was.
 *
 * Escape stays each modal's own binding (`useShortcuts` with `capture`), so
 * the staged-Escape ordering is unchanged. Attach the returned ref to the
 * modal's outermost element.
 *
 * `initialFocus` overrides the initial target without changing DOM/Tab order.
 * `restoreTo` overrides where focus lands on close, and is resolved **at
 * unmount** rather than captured at mount. A modal whose content pages —
 * the media viewer of ADR 0081 — cannot know its restore target when it
 * opens: the user may move to a different image, and back-pagination may
 * replace the row they came from. A resolver that returns `null`, or an
 * element no longer in the document, falls back to the opener; focusing a
 * detached node is a silent no-op that would drop the user on `<body>`.
 */
export function useModalFocus<T extends HTMLElement = HTMLElement>(
  options: {
    initialFocus?: () => HTMLElement | null
    restoreTo?: () => HTMLElement | null
  } = {},
): {
  containerRef: RefObject<T>
} {
  const containerRef = useRef<T>(null)
  // Read through a ref so a caller can pass an inline closure without
  // remounting the trap effect — and so the resolver sees current state.
  // Written in an effect, never during render.
  const restoreToRef = useRef(options.restoreTo)
  const initialFocusRef = useRef(options.initialFocus)
  useEffect(() => {
    restoreToRef.current = options.restoreTo
    initialFocusRef.current = options.initialFocus
  })

  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null
    const container = containerRef.current
    const initial = initialFocusRef.current?.() ?? null
    if (isAttached(initial)) {
      initial.focus()
    } else {
      collectFocusable(container ?? document.createElement('div'))[0]?.focus()
    }

    // Document-level and capture-phase, so the trap holds no matter where
    // inside (or outside) the modal the keydown lands.
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Tab') {
        return
      }
      const root = containerRef.current
      if (root === null) {
        return
      }
      const focusables = collectFocusable(root)
      if (focusables.length === 0) {
        return
      }
      const active = activeElementDeep()
      const index = active === null ? -1 : focusables.indexOf(active)
      const nextIndex =
        index === -1
          ? event.shiftKey
            ? focusables.length - 1
            : 0
          : (index + (event.shiftKey ? -1 : 1) + focusables.length) %
            focusables.length
      event.preventDefault()
      focusables[nextIndex]?.focus()
    }
    document.addEventListener('keydown', onKeyDown, true)
    return () => {
      document.removeEventListener('keydown', onKeyDown, true)
      const requested = restoreToRef.current?.() ?? null
      const target = isAttached(requested) ? requested : null
      target?.focus?.()
      // `.focus()` is a silent no-op on an element that cannot take focus, so
      // verify rather than trust: a resolver may legitimately return a row
      // whose focusable control has not rendered yet (a lazily-loaded image
      // still showing its skeleton). Without this check, focus is left on
      // `<body>` and the user is dropped at the top of the page.
      if (target === null || document.activeElement !== target) {
        previouslyFocused?.focus?.()
      }
    }
  }, [])

  return { containerRef }
}
