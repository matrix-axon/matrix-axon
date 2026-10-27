import type { TargetedMouseEvent } from 'preact'
import { closestGestureControl } from '../gestures'

/** Delegate only unmodified card clicks; selection and embedded controls win. */
export function openMediaCard(
  event: TargetedMouseEvent<HTMLElement>,
  open: () => void,
) {
  if (
    event.defaultPrevented ||
    event.button !== 0 ||
    event.altKey ||
    event.ctrlKey ||
    event.metaKey ||
    event.shiftKey
  )
    return
  if (closestGestureControl(event.target) !== event.currentTarget) return
  if (event.detail !== 0 && window.getSelection()?.isCollapsed === false) return
  // Focus the real preview button so closing the dialog restores a useful
  // keyboard target even when the reader tapped non-focusable caption text.
  const opener =
    event.currentTarget instanceof HTMLButtonElement
      ? event.currentTarget
      : event.currentTarget.querySelector<HTMLButtonElement>(
          '[aria-haspopup="dialog"]',
        )
  opener?.focus({ preventScroll: true })
  open()
}
