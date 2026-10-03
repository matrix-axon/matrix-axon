/**
 * Zoom and pan for the image in the media viewer.
 *
 * The viewer caps an image at the viewport (`.lightbox-image img`), so neither
 * a browser's page zoom nor the shell's (ADR 0107) makes a photo any bigger.
 * And on a phone, a pinch inside the viewer has nothing to act on. This is the
 * viewer's own zoom: a scale and a translation applied to the `<img>` as a
 * transform about its centre.
 *
 * Pure functions over plain numbers, so the gesture code in `Lightbox` stays a
 * thin translation from events to these.
 */

export interface ZoomState {
  /** 1 is "fit to the viewer", the image as it opens. */
  scale: number
  /** Translation in CSS pixels, applied before the scale. */
  x: number
  y: number
}

export interface Point {
  x: number
  y: number
}

export interface Size {
  width: number
  height: number
}

export const FIT: ZoomState = { scale: 1, x: 0, y: 0 }
export const MIN_SCALE = 1
export const MAX_SCALE = 6
/** One press of a zoom button or key. */
export const ZOOM_STEP = 1.5

export function clampScale(scale: number): number {
  return Math.min(MAX_SCALE, Math.max(MIN_SCALE, scale))
}

/**
 * Keep the image from being panned off-screen. Each edge may travel to the
 * edge of the image's unzoomed box and no further, so at scale 1 the image
 * cannot move at all.
 *
 * `size` is the image's *untransformed* size (`offsetWidth`/`offsetHeight`),
 * which a transform does not change.
 */
export function clampPan(state: ZoomState, size: Size): ZoomState {
  const maxX = (size.width * (state.scale - 1)) / 2
  const maxY = (size.height * (state.scale - 1)) / 2
  return {
    scale: state.scale,
    x: Math.min(maxX, Math.max(-maxX, state.x)),
    y: Math.min(maxY, Math.max(-maxY, state.y)),
  }
}

/**
 * Change the scale to `scale` while keeping the image point under `anchor`
 * where it is, which is what makes a pinch or a Ctrl-scroll zoom *into* the
 * spot the user is pointing at.
 *
 * `anchor` is relative to the image's untransformed centre. The point under
 * it is at `(anchor - t) / s` in image coordinates, and we want it under the
 * anchor at the new scale too: `anchor = t' + s' * (anchor - t) / s`.
 */
export function zoomAt(
  state: ZoomState,
  scale: number,
  anchor: Point,
  size: Size,
): ZoomState {
  const next = clampScale(scale)
  if (next === MIN_SCALE) {
    return FIT
  }
  const ratio = next / state.scale
  return clampPan(
    {
      scale: next,
      x: anchor.x - (anchor.x - state.x) * ratio,
      y: anchor.y - (anchor.y - state.y) * ratio,
    },
    size,
  )
}

/** Move the zoomed image by `delta`, within bounds. */
export function panBy(state: ZoomState, delta: Point, size: Size): ZoomState {
  if (state.scale <= MIN_SCALE) {
    return FIT
  }
  return clampPan(
    { scale: state.scale, x: state.x + delta.x, y: state.y + delta.y },
    size,
  )
}

/** The `transform` for a state; `undefined` at fit, so nothing is applied. */
export function zoomTransform(state: ZoomState): string | undefined {
  if (state.scale === MIN_SCALE && state.x === 0 && state.y === 0) {
    return undefined
  }
  return `translate(${state.x}px, ${state.y}px) scale(${state.scale})`
}
