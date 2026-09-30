/**
 * Page zoom for the native shell (ADR 0107).
 *
 * A browser zooms on `Ctrl-+`/`Ctrl--`/`Ctrl-0` by itself; the shell's webview
 * does not, so the client does it through `Platform.setZoom`. Tauri's own
 * `zoom_hotkeys_enabled` is not used. On macOS and Linux it injects a script
 * that also zooms 20% per Ctrl-wheel event, and a trackpad pinch arrives as a
 * burst of exactly those events, so one pinch throws the window to its 20% or
 * 1000% limit. It also forgets the level on every launch.
 *
 * The levels are the browsers' own, so a step feels the same as it does in a
 * tab of the web client.
 */
export const ZOOM_LEVELS = [
  0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2, 2.5, 3,
] as const

export const DEFAULT_ZOOM = 1

/**
 * The next level up (`direction` 1) or down (-1) from `current`, clamped at
 * the ends. `current` need not be one of the levels: a stored value from an
 * older build still steps to its nearest neighbour.
 */
export function stepZoom(current: number, direction: 1 | -1): number {
  if (direction > 0) {
    return (
      ZOOM_LEVELS.find((level) => level > current + 1e-9) ?? ZOOM_LEVELS.at(-1)!
    )
  }
  return (
    [...ZOOM_LEVELS].reverse().find((level) => level < current - 1e-9) ??
    ZOOM_LEVELS[0]
  )
}

/** A persisted zoom factor, or the default when it is not a usable one. */
export function parseZoom(value: unknown): number {
  return typeof value === 'number' &&
    Number.isFinite(value) &&
    value >= ZOOM_LEVELS[0] &&
    value <= ZOOM_LEVELS.at(-1)!
    ? value
    : DEFAULT_ZOOM
}
