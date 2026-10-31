/** Shared arrow navigation for the Unicode and image-pack grids. */
export function moveGridFocus(
  event: KeyboardEvent,
  active: HTMLButtonElement,
  buttons: readonly HTMLButtonElement[],
  grid: Element,
  beforeFirstRow?: HTMLElement | null,
): boolean {
  if (event.isComposing) return false
  const index = buttons.indexOf(active)
  if (index < 0) return false
  const columns = Math.max(
    1,
    getComputedStyle(grid).gridTemplateColumns.split(' ').filter(Boolean)
      .length,
  )
  const step =
    event.key === 'ArrowLeft'
      ? -1
      : event.key === 'ArrowRight'
        ? 1
        : event.key === 'ArrowUp'
          ? -columns
          : event.key === 'ArrowDown'
            ? columns
            : 0
  if (!step) return false
  const next =
    buttons[index + step] ??
    (event.key === 'ArrowUp' && index < columns ? beforeFirstRow : null)
  if (!next) return false
  event.preventDefault()
  next.focus()
  return true
}
