import { useEffect } from 'preact/hooks'
import type { Platform } from './platform'
import { isPrimaryModifier, useShortcuts } from './shortcuts'
import { DEFAULT_ZOOM, stepZoom } from './zoom'

/**
 * Page zoom in the desktop shell (ADR 0107): apply the saved level, and bind
 * Ctrl/⌘ `+` `-` `0` to change it.
 *
 * Mounted on every screen, like `useExternalLinks`: by `App`, which covers the
 * signed-in shell and the sign-in screen, and by `ServerSetup`, which comes
 * before `App`. It used to live in the signed-in shell alone, so a saved 200%
 * came back as 100% on those screens, and the keys and the View menu's zoom
 * items (which replay the keys) did nothing there.
 *
 * `get` reads the level at the moment of the key press rather than the one
 * captured at the last render: two presses before the next render (key
 * repeat, the menu firing quickly) must step twice.
 *
 * Inert where `setZoom` is absent or null: a browser keeps its own zoom, and
 * the mobile shells zoom by pinching. The image viewer claims these chords
 * first while it shows an image, so they zoom the image there instead.
 */
export function usePageZoom(
  setZoom: Platform['setZoom'] | undefined,
  zoom: { level: number; get: () => number; set: (level: number) => void },
): void {
  const apply = setZoom ?? null
  useEffect(() => {
    apply?.(zoom.level).catch((error: unknown) => {
      console.error('could not set the page zoom', error)
    })
  }, [apply, zoom.level])

  const step = (direction: 1 | -1 | 0) => (event: KeyboardEvent) => {
    if (apply === null || !isPrimaryModifier(event)) {
      return
    }
    event.preventDefault()
    zoom.set(direction === 0 ? DEFAULT_ZOOM : stepZoom(zoom.get(), direction))
  }
  useShortcuts(
    {
      // Zoom in answers to `=` as well as `+`: the unshifted key is what the
      // label calls `+` on most layouts, and a numpad `+` needs no Shift.
      'mod+=': step(1),
      'mod++': step(1),
      'mod+shift+=': step(1),
      'mod+shift++': step(1),
      'mod+-': step(-1),
      'mod+0': step(0),
    },
    { whileTyping: true },
  )
}
