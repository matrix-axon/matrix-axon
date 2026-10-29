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
  // Even a selection-ending click belongs to the card: letting it reach the
  // row would open message actions after a text drag. Pointer gestures still
  // reach the row independently.
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
