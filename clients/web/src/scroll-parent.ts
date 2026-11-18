/** The nearest scrolling ancestor, or `null` when nothing scrolls (a
 *  layout-less test environment, or an element outside any scroller). */
export function scrollParentOf(
  element: HTMLElement | null,
): HTMLElement | null {
  for (
    let node = element?.parentElement ?? null;
    node !== null;
    node = node.parentElement
  ) {
    const { overflowY } = getComputedStyle(node)
    if (overflowY === 'auto' || overflowY === 'scroll') {
      return node
    }
  }
  return null
}
