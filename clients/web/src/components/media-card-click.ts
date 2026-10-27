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
  const opener = event.currentTarget.querySelector<HTMLButtonElement>(
    'button[aria-haspopup="dialog"]',
  )
  const control = closestGestureControl(event.target)
  if (control !== null && control !== opener) return
  // The card owns a plain click; pointer gestures still reach the message row.
  event.stopPropagation()
  const selection = window.getSelection()
  if (
    event.detail !== 0 &&
    selection?.isCollapsed === false &&
    (event.currentTarget.contains(selection.anchorNode) ||
      event.currentTarget.contains(selection.focusNode))
  )
    return
  // Focus the real preview button so closing the dialog restores a useful
  // keyboard target even when the reader tapped non-focusable caption text.
  opener?.focus({ preventScroll: true })
  open()
}
